//! Headless rendering: no window at all.
//!
//! [`crate::start`] selects this when [`Conf::headless`] is set; this module is
//! the same entry point for a caller that already has a `Conf`.
//!
//! On macOS the context is an `NSOpenGLContext` with no view: no `NSWindow`, no
//! `NSApplication` run loop and no display link. On Linux it is an EGL pbuffer
//! on the surfaceless platform: no window, no `DISPLAY`, no Xvfb and no
//! compositor.
//!
//! Either way `update` and `draw` are called back to back, so a capture is as
//! fast as the machine can draw it. Frames are drawn into render targets,
//! exactly as on a window; there is no default framebuffer to present. There is
//! no display scale either: one point is one pixel, so a capture is the same on
//! a Retina Mac, an external display and a CI runner.
//!
//! The remaining desktop and web targets fall back to a surface they never show
//! (or, on the web, the canvas) and say so, until their offscreen backend lands.

use crate::conf::Conf;
use crate::event::EventHandler;

pub fn start<F>(conf: Conf, f: F)
where
    F: 'static + FnOnce() -> Box<dyn EventHandler>,
{
    #[cfg(target_os = "macos")]
    unsafe {
        crate::native::macos::run_headless(&conf, f)
    }

    #[cfg(target_os = "linux")]
    unsafe {
        crate::native::linux_headless::run_headless(&conf, f)
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        #[cfg(not(target_arch = "wasm32"))]
        eprintln!(
            "miniquad: headless is not implemented on this platform yet; creating a window it will not show"
        );

        let mut conf = conf;
        conf.headless = false;
        // A surface it cannot show is the closest this platform has until the
        // offscreen backend lands. On the web the canvas is already the whole
        // surface.
        crate::start_headless_fallback(conf, f);
    }
}
