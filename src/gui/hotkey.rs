//! The global shortcut that brings up the window.

use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};

pub struct Hotkeys {
    manager: Option<GlobalHotKeyManager>,
    current: Option<HotKey>,
    unavailable: Option<String>,
}

impl Hotkeys {
    /// `on_press` runs on the thread that delivers hotkey events (the main thread on Windows and macOS).
    pub fn new(on_press: impl Fn() + Send + Sync + 'static) -> Hotkeys {
        let (manager, unavailable) = match GlobalHotKeyManager::new() {
            Ok(m) => (Some(m), None),
            Err(e) => (None, Some(e.to_string())),
        };
        GlobalHotKeyEvent::set_event_handler(Some(move |ev: GlobalHotKeyEvent| {
            if ev.state == HotKeyState::Pressed {
                on_press();
            }
        }));
        Hotkeys { manager, current: None, unavailable }
    }

    /// Registers `spec` (e.g. "Alt+Shift+Space") in place of the current shortcut; empty turns it off.
    pub fn set(&mut self, spec: &str) -> Result<(), String> {
        let Some(manager) = &self.manager else {
            return Err(format!(
                "global shortcuts are unavailable here ({})",
                self.unavailable.as_deref().unwrap_or("")
            ));
        };
        if let Some(old) = self.current.take() {
            let _ = manager.unregister(old);
        }
        let spec = spec.trim();
        if spec.is_empty() {
            return Ok(());
        }
        let hk: HotKey = spec.parse().map_err(|e| format!("{spec:?} is not a valid shortcut: {e}"))?;
        manager.register(hk).map_err(|e| format!("could not register {spec}: {e}"))?;
        self.current = Some(hk);
        Ok(())
    }
}

pub fn is_valid(spec: &str) -> bool {
    spec.trim().is_empty() || spec.trim().parse::<HotKey>().is_ok()
}
