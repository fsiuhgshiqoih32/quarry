//! The settings window. Most switches apply at once; the folder lists wait for "Apply and rescan"
//! because a rescan of a whole drive takes a while.

use std::path::Path;

use eframe::egui::{self, Button, Checkbox, DragValue, RichText};

use quarry::config::{Config, Theme};

use super::hotkey;

pub struct State {
    roots: Vec<String>,
    excludes: Vec<String>,
    new_root: String,
    new_exclude: String,
    hotkey: String,
    port: u16,
    rescan_minutes: u32,
    pub autostart: bool,
}

impl State {
    pub fn new(cfg: &Config, autostart: bool) -> State {
        State {
            roots: cfg.roots.clone(),
            excludes: cfg.excludes.clone(),
            new_root: String::new(),
            new_exclude: String::new(),
            hotkey: cfg.hotkey.clone(),
            port: cfg.http_port,
            rescan_minutes: cfg.rescan_minutes,
            autostart,
        }
    }
}

pub struct Info<'a> {
    pub api_port: Option<u16>,
    pub api_error: Option<&'a str>,
    pub hotkey_error: Option<&'a str>,
    pub tray_error: Option<&'a str>,
    pub has_tray: bool,
    pub autostart_available: bool,
    pub config_path: &'a Path,
    pub index_path: &'a Path,
}

#[derive(Default)]
pub struct Outcome {
    pub config: Option<Config>,
    pub hotkey: Option<String>,
    pub rescan: bool,
    pub autostart: Option<bool>,
}

fn error(ui: &mut egui::Ui, text: &str) {
    let c = ui.visuals().error_fg_color;
    ui.label(RichText::new(text).color(c));
}

/// An editable list of paths with an add field. Returns nothing; edits `list` in place.
fn path_list(ui: &mut egui::Ui, id: &str, list: &mut Vec<String>, new: &mut String, hint: &str, empty: &str) {
    if list.is_empty() {
        ui.label(RichText::new(empty).weak());
    }
    let mut remove = None;
    for (i, item) in list.iter().enumerate() {
        ui.horizontal(|ui| {
            if ui.small_button("Remove").clicked() {
                remove = Some(i);
            }
            ui.monospace(item);
        });
    }
    if let Some(i) = remove {
        list.remove(i);
    }
    ui.horizontal(|ui| {
        let edit =
            egui::TextEdit::singleline(new).id_salt(id).hint_text(hint).desired_width(ui.available_width() - 60.0);
        let r = ui.add(edit);
        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if (ui.button("Add").clicked() || enter) && !new.trim().is_empty() {
            let v = new.trim().to_string();
            if !list.contains(&v) {
                list.push(v);
            }
            new.clear();
        }
    });
}

pub fn window(ctx: &egui::Context, open: &mut bool, st: &mut State, cfg: &Config, info: Info<'_>) -> Outcome {
    let mut out = Outcome::default();
    let mut next = cfg.clone();
    egui::Window::new("Settings")
        .open(open)
        .collapsible(false)
        .resizable(true)
        .default_width(560.0)
        .default_height(560.0)
        .show(ctx, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, true]).show(ui, |ui| {
                ui.strong("Folders to index");
                path_list(
                    ui,
                    "new-root",
                    &mut st.roots,
                    &mut st.new_root,
                    if cfg!(windows) { r"e.g. D:\Projects" } else { "e.g. /home/me/projects" },
                    if cfg!(windows) { "Every local drive (default)" } else { "The whole file system, / (default)" },
                );
                ui.add_space(6.0);
                ui.strong("Skip");
                path_list(
                    ui,
                    "new-exclude",
                    &mut st.excludes,
                    &mut st.new_exclude,
                    "a full path, or a folder name like node_modules (* and ? work)",
                    "Nothing beyond system folders",
                );
                let pending = st.roots != cfg.roots || st.excludes != cfg.excludes;
                ui.horizontal(|ui| {
                    if ui.add_enabled(pending, Button::new("Apply and rescan")).clicked() {
                        next.roots = st.roots.clone();
                        next.excludes = st.excludes.clone();
                    }
                    if pending && ui.button("Undo").clicked() {
                        st.roots = cfg.roots.clone();
                        st.excludes = cfg.excludes.clone();
                    }
                    if ui.button("Rescan now").clicked() {
                        out.rescan = true;
                    }
                });

                ui.separator();
                ui.strong("Search");
                ui.checkbox(&mut next.show_hidden, "Show hidden files and folders");
                ui.checkbox(&mut next.match_path, "Match full paths, not just names, for every term");

                ui.separator();
                ui.strong("Staying current");
                ui.checkbox(&mut next.live_updates, "Follow changes as they happen");
                if cfg!(target_os = "linux") {
                    ui.label(
                        RichText::new(
                            "On Linux each folder needs an inotify watch. If the system limit runs out,                              the folders without one are refreshed hourly.",
                        )
                        .weak(),
                    );
                }
                ui.checkbox(&mut next.rescan_on_start, "Catch up with changes when Quarry starts");
                ui.horizontal(|ui| {
                    ui.label("Refresh every");
                    ui.add(DragValue::new(&mut st.rescan_minutes).range(0..=10_080).suffix(" min"));
                    ui.label(RichText::new("0 turns it off").weak());
                });
                next.rescan_minutes = st.rescan_minutes;

                ui.separator();
                ui.strong("Window");
                ui.horizontal(|ui| {
                    ui.label("Global shortcut");
                    ui.add(egui::TextEdit::singleline(&mut st.hotkey).desired_width(160.0).hint_text("off"));
                    let valid = hotkey::is_valid(&st.hotkey);
                    if ui.add_enabled(valid && st.hotkey.trim() != cfg.hotkey, Button::new("Apply")).clicked() {
                        out.hotkey = Some(st.hotkey.trim().to_string());
                    }
                    if !valid {
                        error(ui, "Try a form like Alt+Shift+Space");
                    }
                });
                if let Some(e) = info.hotkey_error {
                    error(ui, e);
                }
                ui.add_enabled(
                    info.has_tray,
                    Checkbox::new(&mut next.close_to_tray, "Closing the window keeps Quarry running in the tray"),
                );
                if let Some(e) = info.tray_error {
                    ui.label(RichText::new(format!("No tray icon: {e}")).weak());
                }
                if info.autostart_available {
                    let mut on = st.autostart;
                    if ui.checkbox(&mut on, "Start at login, hidden in the tray").changed() {
                        out.autostart = Some(on);
                    }
                }
                ui.horizontal(|ui| {
                    ui.label("Theme");
                    ui.radio_value(&mut next.theme, Theme::System, "System");
                    ui.radio_value(&mut next.theme, Theme::Light, "Light");
                    ui.radio_value(&mut next.theme, Theme::Dark, "Dark");
                });

                ui.separator();
                ui.strong("HTTP API");
                ui.checkbox(&mut next.http_enabled, "Answer searches on 127.0.0.1 (for scripts and launchers)");
                ui.horizontal(|ui| {
                    ui.label("Port");
                    ui.add(DragValue::new(&mut st.port).range(1024..=65535));
                    if st.port != cfg.http_port && ui.button("Apply").clicked() {
                        next.http_port = st.port;
                    }
                });
                if let Some(port) = info.api_port {
                    let url = format!("http://127.0.0.1:{port}/search?q=");
                    ui.hyperlink_to(format!("{url}..."), format!("{url}quarry"));
                }
                if let Some(e) = info.api_error {
                    error(ui, e);
                }

                ui.separator();
                ui.label(RichText::new(format!("Settings file: {}", info.config_path.display())).weak().small());
                ui.label(RichText::new(format!("Index file: {}", info.index_path.display())).weak().small());
            });
        });
    if next != *cfg {
        out.config = Some(next);
    }
    out
}
