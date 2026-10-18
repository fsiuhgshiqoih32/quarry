//! The tray (menu bar) icon. On Linux it lives on its own GTK thread.

use std::sync::Arc;

use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Show,
    Rescan,
    Settings,
    Quit,
}

pub type Handler = Arc<dyn Fn(Action) + Send + Sync>;

/// Keeps the icon alive (Windows and macOS; on Linux the GTK thread owns it).
pub struct Tray {
    _icon: Option<TrayIcon>,
}

fn icon() -> Result<Icon, String> {
    // The macOS menu bar wants a black template image that it tints for light and dark mode.
    let (size, rgba) = if cfg!(target_os = "macos") {
        (36, quarry::art::glyph(36, [0, 0, 0]))
    } else {
        let s = if cfg!(target_os = "linux") { 48 } else { 32 };
        (s, quarry::art::app_icon(s))
    };
    Icon::from_rgba(rgba, size as u32, size as u32).map_err(|e| e.to_string())
}

fn build() -> Result<TrayIcon, String> {
    let menu = Menu::new();
    menu.append_items(&[
        &MenuItem::with_id("show", "Show Quarry", true, None),
        &MenuItem::with_id("rescan", "Rescan now", true, None),
        &MenuItem::with_id("settings", "Settings...", true, None),
        &PredefinedMenuItem::separator(),
        &MenuItem::with_id("quit", "Quit Quarry", true, None),
    ])
    .map_err(|e| e.to_string())?;
    let builder = TrayIconBuilder::new().with_menu(Box::new(menu)).with_tooltip("Quarry");
    #[cfg(target_os = "macos")]
    let builder = builder.with_icon_as_template(true);
    // On Windows a left click opens the window; the menu stays on the right button.
    #[cfg(windows)]
    let builder = builder.with_menu_on_left_click(false);
    builder.with_icon(icon()?).build().map_err(|e| e.to_string())
}

fn route(handler: Handler) {
    let h = handler.clone();
    MenuEvent::set_event_handler(Some(move |ev: MenuEvent| {
        let action = match ev.id.0.as_str() {
            "show" => Action::Show,
            "rescan" => Action::Rescan,
            "settings" => Action::Settings,
            "quit" => Action::Quit,
            _ => return,
        };
        h(action);
    }));
    TrayIconEvent::set_event_handler(Some(move |ev: TrayIconEvent| {
        if cfg!(windows)
            && let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = ev
        {
            handler(Action::Show);
        }
    }));
}

pub fn start(handler: Handler) -> Result<Tray, String> {
    route(handler);
    #[cfg(target_os = "linux")]
    {
        std::thread::Builder::new()
            .name("tray".into())
            .spawn(|| {
                if gtk::init().is_err() {
                    eprintln!("quarry: no tray icon (GTK could not start)");
                    return;
                }
                match build() {
                    Ok(_icon) => gtk::main(),
                    Err(e) => eprintln!("quarry: no tray icon ({e})"),
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Tray { _icon: None })
    }
    #[cfg(not(target_os = "linux"))]
    {
        Ok(Tray { _icon: Some(build()?) })
    }
}
