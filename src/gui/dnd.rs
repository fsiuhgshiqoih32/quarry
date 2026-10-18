//! Dragging results out of the window into other apps (Windows and macOS).

use std::path::PathBuf;

pub const SUPPORTED: bool = cfg!(any(windows, target_os = "macos"));

#[cfg(any(windows, target_os = "macos"))]
pub fn start(frame: &eframe::Frame, paths: Vec<PathBuf>) -> Result<(), String> {
    let image = drag::Image::Raw(quarry::art::icon_png(48));
    drag::start_drag(frame, drag::DragItem::Files(paths), image, |_, _| {}, drag::Options::default())
        .map_err(|e| e.to_string())
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn start(_frame: &eframe::Frame, _paths: Vec<PathBuf>) -> Result<(), String> {
    Err("dragging files out of Quarry is not supported on Linux yet".into())
}
