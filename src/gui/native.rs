//! Bringing the window back from hidden. A hidden window gets no repaints on Windows and macOS, so
//! egui never sees a "show" command sent while it is hidden; these calls go to the OS directly.

use eframe::egui;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

pub struct Native {
    #[cfg(windows)]
    hwnd: isize,
    #[cfg(target_os = "macos")]
    view: usize,
}

impl Native {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Native {
        let raw = cc.window_handle().ok().map(|h| h.as_raw());
        #[cfg(windows)]
        let hwnd = match raw {
            Some(RawWindowHandle::Win32(h)) => h.hwnd.get(),
            _ => 0,
        };
        #[cfg(target_os = "macos")]
        let view = match raw {
            Some(RawWindowHandle::AppKit(h)) => h.ns_view.as_ptr() as usize,
            _ => 0,
        };
        #[cfg(not(any(windows, target_os = "macos")))]
        let _ = raw;
        Native {
            #[cfg(windows)]
            hwnd,
            #[cfg(target_os = "macos")]
            view,
        }
    }

    /// Shows and focuses the window. Safe from any thread on Windows; on macOS it only acts on the
    /// main thread. Elsewhere egui's own viewport commands do the job.
    pub fn raise(&self) {
        #[cfg(windows)]
        unsafe {
            use windows_sys::Win32::UI::WindowsAndMessaging::{
                IsIconic, SW_RESTORE, SW_SHOW, SetForegroundWindow, ShowWindow,
            };
            if self.hwnd == 0 {
                return;
            }
            let h = self.hwnd as windows_sys::Win32::Foundation::HWND;
            ShowWindow(h, if IsIconic(h) != 0 { SW_RESTORE } else { SW_SHOW });
            SetForegroundWindow(h);
        }
        #[cfg(target_os = "macos")]
        {
            use objc2::MainThreadMarker;
            use objc2_app_kit::{NSApplication, NSView};
            let Some(mtm) = MainThreadMarker::new() else { return };
            if self.view == 0 {
                return;
            }
            let view: &NSView = unsafe { &*(self.view as *const NSView) };
            // Some objc2 releases mark these unsafe, some don't.
            #[allow(unused_unsafe, deprecated)]
            unsafe {
                if let Some(window) = view.window() {
                    window.makeKeyAndOrderFront(None);
                }
                NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
            }
        }
    }
}

// The window handle is only used through OS calls that are thread-safe on Windows and guarded by a
// main-thread check on macOS.
unsafe impl Send for Native {}
unsafe impl Sync for Native {}

/// Keeps egui's idea of the window in step after [`Native::raise`] showed it behind its back.
pub fn sync_shown(ctx: &egui::Context) {
    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
}
