//! The syntax cheat sheet.

use eframe::egui::{self, RichText};

const SYNTAX: &[(&str, &str)] = &[
    ("report 2026", "names containing both words"),
    ("jpg|png", "either word"),
    ("!temp", "names without the word"),
    ("<jpg|png> holiday", "group with < >"),
    ("\"my file\"", "keep the space"),
    ("*.mp3   draft-??", "wildcards match the whole name"),
    ("projects/notes", "a slash matches the full path"),
    ("ext:pdf;docx", "by extension"),
    ("size:>100mb   size:1mb..5mb", "by size (b, kb, mb, gb, tb)"),
    ("size:empty|tiny|large|huge", "Everything's size names"),
    ("dm:today   dm:last7days", "by date modified"),
    ("dm:2026-01-01..2026-03-31", "date range; also dm:>2026, dm:thismonth"),
    ("file:   folder:", "only files, only folders"),
    ("parent:\"C:\\Users\\me\"", "directly inside a folder"),
    ("case:Readme", "match case"),
    ("ww:log", "whole word"),
    ("regex:^IMG_\\d{4}\\.jpe?g$", "regular expression"),
    ("path:src main", "match this term against the full path"),
    ("startwith:IMG   endwith:.bak", "start or end of the name"),
];

const KEYS: &[(&str, &str)] = &[
    ("Down / Up", "move between the search box and results"),
    ("Enter", "open"),
    ("Ctrl+Enter (Cmd on macOS)", "show in folder"),
    ("Ctrl+C / Ctrl+Shift+C", "copy full paths / names"),
    ("Shift+click, Ctrl+click", "select several"),
    ("Drag rows", "drop files into other apps (Windows, macOS)"),
    ("Ctrl+F", "back to the search box"),
    ("F5", "rescan"),
    ("Esc", "clear the search, then hide"),
];

fn table(ui: &mut egui::Ui, id: &str, rows: &[(&str, &str)]) {
    egui::Grid::new(id).num_columns(2).striped(true).spacing([16.0, 4.0]).show(ui, |ui| {
        for (a, b) in rows {
            ui.label(RichText::new(*a).monospace());
            ui.label(*b);
            ui.end_row();
        }
    });
}

pub fn window(ctx: &egui::Context, open: &mut bool) {
    egui::Window::new("Search syntax").open(open).collapsible(false).default_width(520.0).show(ctx, |ui| {
        egui::ScrollArea::vertical().show(ui, |ui| {
            table(ui, "syntax", SYNTAX);
            ui.add_space(8.0);
            ui.strong("Keyboard");
            table(ui, "keys", KEYS);
        });
    });
}
