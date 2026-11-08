//! Linux headless rendering: an EGL pbuffer, with no window and no display
//! server.
//!
//! `EGL_MESA_platform_surfaceless` gives EGL a display with no native window
//! system behind it, and a pbuffer is its drawable. A capture therefore needs
//! no `DISPLAY`, no Xvfb and no Wayland compositor.
//!
//! The context is GLES2, as on the X11 EGL backend. Frames are drawn into
//! render targets exactly as on a window; there is no default framebuffer to
//! present, so nothing is swapped and the loop never waits on a display.

use crate::{
    event::EventHandler,
    native::{
        egl::{
            EGLConfig, EGLContext, EGLDisplay, EGLSurface, EGLint, LibEgl, EGL_ALPHA_SIZE,
            EGL_BLUE_SIZE, EGL_CONTEXT_CLIENT_VERSION, EGL_DEPTH_SIZE, EGL_GREEN_SIZE, EGL_HEIGHT,
            EGL_NONE, EGL_OPENGL_ES2_BIT, EGL_OPENGL_ES_API, EGL_PBUFFER_BIT,
            EGL_PLATFORM_SURFACELESS_MESA, EGL_RED_SIZE, EGL_RENDERABLE_TYPE, EGL_SAMPLES,
            EGL_STENCIL_SIZE, EGL_SURFACE_TYPE, EGL_WIDTH,
        },
        gl, module, Clipboard, NativeDisplayData, Request,
    },
};

use std::ffi::{c_void, CString};

type GetPlatformDisplay = unsafe extern "C" fn(u32, *mut c_void, *const EGLint) -> EGLDisplay;

/// A process-local clipboard: headless has no desktop to share one with.
#[derive(Default)]
struct HeadlessClipboard(Option<String>);

impl Clipboard for HeadlessClipboard {
    fn get(&mut self) -> Option<String> {
        self.0.clone()
    }

    fn set(&mut self, string: &str) {
        self.0 = Some(string.to_owned());
    }
}

pub unsafe fn run_headless<F>(conf: &crate::conf::Conf, f: F)
where
    F: 'static + FnOnce() -> Box<dyn EventHandler>,
{
    let egl = LibEgl::try_load().expect("miniquad: headless rendering needs libEGL");
    // `eglGetPlatformDisplay` is EGL 1.5 and the EXT spelling is an extension,
    // so neither can go in `LibEgl`'s required-symbol list: a libEGL without
    // them would stop loading altogether. This handle keeps the library mapped
    // for as long as the pointer is used.
    let lib = module::Module::load("libEGL.so")
        .or_else(|_| module::Module::load("libEGL.so.1"))
        .expect("miniquad: libEGL not found for headless rendering");
    let get_platform_display = platform_display(&egl, &lib);

    let display = get_platform_display(
        EGL_PLATFORM_SURFACELESS_MESA,
        std::ptr::null_mut(),
        std::ptr::null(),
    );
    if display.is_null() {
        panic!(
            "miniquad: EGL_PLATFORM_SURFACELESS_MESA is not available (EGL error 0x{:x})",
            (egl.eglGetError)()
        );
    }
    check(
        &egl,
        "eglInitialize",
        (egl.eglInitialize)(display, std::ptr::null_mut(), std::ptr::null_mut()) == 1,
    );
    check(
        &egl,
        "eglBindAPI(EGL_OPENGL_ES_API)",
        (egl.eglBindAPI)(EGL_OPENGL_ES_API) == 1,
    );

    let width = conf.window_width.max(1);
    let height = conf.window_height.max(1);
    let (config, mut surface, context) = create_pbuffer(
        &egl,
        display,
        width,
        height,
        conf.platform.framebuffer_alpha,
        conf.sample_count,
    );
    check(
        &egl,
        "eglMakeCurrent",
        (egl.eglMakeCurrent)(display, surface, surface, context) == 1,
    );

    gl::load_gl_funcs(|proc| {
        let name = CString::new(proc).unwrap();
        (egl.eglGetProcAddress)(name.as_ptr() as _)
    });

    let (tx, rx) = std::sync::mpsc::channel();
    crate::set_display(NativeDisplayData {
        ..NativeDisplayData::new(width, height, tx, Box::new(HeadlessClipboard::default()))
    });

    let mut event_handler = f();
    event_handler.resize_event(width as f32, height as f32);

    loop {
        while let Ok(request) = rx.try_recv() {
            match request {
                Request::ScheduleUpdate => {}
                Request::SetWindowSize {
                    new_width,
                    new_height,
                } => {
                    let w = new_width.max(1) as i32;
                    let h = new_height.max(1) as i32;
                    // A pbuffer's size is fixed at creation, so a resize is a
                    // new surface on the same context.
                    (egl.eglMakeCurrent)(
                        display,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    );
                    (egl.eglDestroySurface)(display, surface);
                    surface = create_surface(&egl, display, config, w, h);
                    check(
                        &egl,
                        "eglMakeCurrent",
                        (egl.eglMakeCurrent)(display, surface, surface, context) == 1,
                    );
                    {
                        let mut d = crate::native_display().lock().unwrap();
                        d.screen_width = w;
                        d.screen_height = h;
                    }
                    event_handler.resize_event(w as f32, h as f32);
                }
                _ => {}
            }
        }

        let (ordered, requested) = {
            let d = crate::native_display().lock().unwrap();
            (d.quit_ordered, d.quit_requested)
        };
        if ordered {
            break;
        }
        if requested {
            event_handler.quit_requested_event();
            let mut d = crate::native_display().lock().unwrap();
            if d.quit_requested {
                d.quit_requested = false;
                d.quit_ordered = true;
                break;
            }
        }

        event_handler.update();
        event_handler.draw();
    }

    // The app's backend deletes GL objects when it drops, so it goes before
    // the context does.
    drop(event_handler);

    (egl.eglMakeCurrent)(
        display,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        std::ptr::null_mut(),
    );
    (egl.eglDestroySurface)(display, surface);
    (egl.eglDestroyContext)(display, context);
    (egl.eglTerminate)(display);
}

/// `eglGetPlatformDisplay` (EGL 1.5) first, then the `EGL_EXT_platform_base`
/// spelling, which is only reachable through `eglGetProcAddress`.
unsafe fn platform_display(egl: &LibEgl, egl_module: &module::Module) -> GetPlatformDisplay {
    if let Ok(get) = egl_module.get_symbol::<GetPlatformDisplay>("eglGetPlatformDisplay") {
        return get;
    }

    let name = CString::new("eglGetPlatformDisplayEXT").unwrap();
    match (egl.eglGetProcAddress)(name.as_ptr() as _) {
        Some(get) => std::mem::transmute::<unsafe extern "C" fn(), GetPlatformDisplay>(get),
        None => panic!(
            "miniquad: headless rendering needs eglGetPlatformDisplay (EGL 1.5) or EGL_EXT_platform_base"
        ),
    }
}

unsafe fn create_pbuffer(
    egl: &LibEgl,
    display: EGLDisplay,
    width: i32,
    height: i32,
    alpha: bool,
    sample_count: i32,
) -> (EGLConfig, EGLSurface, EGLContext) {
    let alpha_size = if alpha { 8 } else { 0 };
    // miniquad's default is one sample, which means no multisampling; EGL
    // spells that zero, and a driver need not offer a config for a request of
    // one.
    let samples = if sample_count > 1 {
        sample_count as u32
    } else {
        0
    };
    #[rustfmt::skip]
    let attributes = [
        EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
        EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT,
        EGL_RED_SIZE, 8,
        EGL_GREEN_SIZE, 8,
        EGL_BLUE_SIZE, 8,
        EGL_ALPHA_SIZE, alpha_size,
        EGL_DEPTH_SIZE, 16,
        EGL_STENCIL_SIZE, 0,
        EGL_SAMPLES, samples,
        EGL_NONE,
    ];
    let mut configs = [std::ptr::null_mut(); 32];
    let mut config_count = 0;
    check(
        egl,
        "eglChooseConfig",
        (egl.eglChooseConfig)(
            display,
            attributes.as_ptr() as _,
            configs.as_mut_ptr(),
            configs.len() as _,
            &mut config_count,
        ) == 1,
    );
    if config_count == 0 {
        panic!("miniquad: no EGL config for a GLES2 pbuffer");
    }
    let config = configs[0];

    let surface = create_surface(egl, display, config, width, height);

    let context_attributes = [EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE];
    let context = (egl.eglCreateContext)(
        display,
        config,
        std::ptr::null_mut(),
        context_attributes.as_ptr() as _,
    );
    if context.is_null() {
        panic!(
            "miniquad: eglCreateContext failed (EGL error 0x{:x})",
            (egl.eglGetError)()
        );
    }

    (config, surface, context)
}

unsafe fn create_surface(
    egl: &LibEgl,
    display: EGLDisplay,
    config: EGLConfig,
    width: i32,
    height: i32,
) -> EGLSurface {
    let attributes = [EGL_WIDTH, width as u32, EGL_HEIGHT, height as u32, EGL_NONE];
    let surface = (egl.eglCreatePbufferSurface)(display, config, attributes.as_ptr() as _);
    if surface.is_null() {
        panic!(
            "miniquad: eglCreatePbufferSurface({width}x{height}) failed (EGL error 0x{:x})",
            (egl.eglGetError)()
        );
    }
    surface
}

unsafe fn check(egl: &LibEgl, what: &str, ok: bool) {
    if !ok {
        panic!(
            "miniquad: {what} failed (EGL error 0x{:x})",
            (egl.eglGetError)()
        );
    }
}
