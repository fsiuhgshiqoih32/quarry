//! "Start at login": launches Quarry hidden in the tray.

pub fn launcher() -> Option<auto_launch::AutoLaunch> {
    let exe = std::env::current_exe().ok()?;
    let path = exe.to_str()?;
    auto_launch::AutoLaunchBuilder::new().set_app_name("quarry").set_app_path(path).set_args(&["--hidden"]).build().ok()
}

pub fn is_enabled(l: &auto_launch::AutoLaunch) -> bool {
    l.is_enabled().unwrap_or(false)
}

pub fn set(l: &auto_launch::AutoLaunch, on: bool) -> Result<(), String> {
    if on { l.enable() } else { l.disable() }.map_err(|e| e.to_string())
}
