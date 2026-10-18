//! The search window.

mod autostart;
mod dnd;
mod fonts;
mod help;
mod hotkey;
mod instance;
mod native;
mod settings;
#[cfg(feature = "tray")]
mod tray;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align, Color32, CornerRadius, Event, FontId, Key, Modifiers, Rect, RichText, Sense, Stroke, StrokeKind,
    TextFormat, Vec2, text::LayoutJob,
};
use egui_extras::{Column, TableBuilder};
use parking_lot::Mutex;

use quarry::config::{self, Config, Theme};
use quarry::engine::{Engine, Shared, Watching};
use quarry::fmt;
use quarry::index::Index;
use quarry::query::Options;
use quarry::search::{Request, Searcher, SortKey};
use quarry::serve::Api;

use native::Native;

pub struct Launch {
    pub hidden: bool,
    pub query: Option<String>,
    pub smoke_test: bool,
}

/// Flags set from other threads (tray, hotkey, second launches) and read by the UI.
#[derive(Default)]
struct Signals {
    show: AtomicBool,
    hide: AtomicBool,
    rescan: AtomicBool,
    settings: AtomicBool,
    /// A search sent by another launch (`quarry --query ...`).
    query: Mutex<Option<String>>,
    /// Kept current by the UI.
    visible: AtomicBool,
    focused: AtomicBool,
}

/// The engine, reachable from the tray's Quit even while the window is hidden and not repainting.
type EngineCell = Arc<Mutex<Option<Engine>>>;

const SEARCH_ID: &str = "search-box";
static SMOKE_FAILED: AtomicBool = AtomicBool::new(false);

pub fn run(launch: Launch) -> Result<(), String> {
    let (cfg_path, index_path, cfg, listener) = if launch.smoke_test {
        let dir = std::env::temp_dir().join(format!("quarry-gui-smoke-{}", std::process::id()));
        let data = dir.join("files");
        std::fs::create_dir_all(&data).map_err(|e| e.to_string())?;
        std::fs::write(data.join("smoke-test-target.txt"), b"x").map_err(|e| e.to_string())?;
        let cfg =
            Config { roots: vec![data.to_string_lossy().into_owned()], hotkey: String::new(), ..Config::default() };
        (dir.join("config.toml"), dir.join("index.bin"), cfg, None)
    } else {
        let listener = match instance::acquire() {
            instance::Acquire::First(l) => Some(l),
            instance::Acquire::Running => {
                instance::signal_show(launch.query.as_deref());
                return Ok(());
            }
            instance::Acquire::Unknown => None,
        };
        let path = config::config_path();
        let cfg = Config::load(&path);
        (path, config::index_path(), cfg, listener)
    };
    let icon = egui::IconData { rgba: quarry::art::app_icon(64), width: 64, height: 64 };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Quarry")
            .with_app_id("quarry")
            .with_inner_size([1000.0, 640.0])
            .with_min_inner_size([560.0, 320.0])
            .with_icon(icon),
        persist_window: !launch.smoke_test,
        ..Default::default()
    };
    let smoke = launch.smoke_test;
    eframe::run_native(
        "Quarry",
        options,
        Box::new(move |cc| Ok(Box::new(App::new(cc, cfg, cfg_path, index_path, listener, launch)))),
    )
    .map_err(|e| e.to_string())?;
    if smoke && SMOKE_FAILED.load(Ordering::Relaxed) {
        return Err("GUI smoke test did not find its file".into());
    }
    Ok(())
}

/// What a result row shows, cached so rows can be drawn while the index is locked for writing.
#[derive(Clone)]
struct Row {
    name: String,
    folder: String,
    size: u64,
    mtime: i64,
    dir: bool,
    gone: bool,
}

struct Sent {
    req: Request,
    revision: u64,
    epoch: u64,
    at: Instant,
}

struct App {
    cfg: Config,
    cfg_path: PathBuf,
    index_path: PathBuf,
    shared: Arc<Shared>,
    engine: EngineCell,
    searcher: Searcher,
    api: Option<Api>,
    api_error: Option<String>,
    signals: Arc<Signals>,
    hotkeys: hotkey::Hotkeys,
    hotkey_error: Option<String>,
    #[cfg(feature = "tray")]
    tray: Option<tray::Tray>,
    tray_error: Option<String>,
    autostart: Option<auto_launch::AutoLaunch>,

    query: String,
    sent: Option<Sent>,
    results: Arc<Vec<u32>>,
    results_epoch: u64,
    results_query: String,
    highlights: Vec<String>,
    search_error: Option<String>,
    search_time: Duration,
    rows: HashMap<u32, Row>,
    rows_key: (u64, u64),
    /// Last known index size, for when the index is busy.
    indexed: usize,

    selected: HashSet<u32>,
    cursor: Option<usize>,
    anchor: Option<usize>,
    scroll_to: Option<usize>,
    focus_search: bool,
    select_all_query: bool,
    message: Option<(String, Instant)>,

    settings: settings::State,
    show_settings: bool,
    show_help: bool,
    start_hidden: bool,
    /// Set when closing for real, so close-to-tray doesn't cancel it.
    quitting: bool,
    smoke: Option<Instant>,
    frames: u64,
}

impl App {
    fn new(
        cc: &eframe::CreationContext<'_>,
        cfg: Config,
        cfg_path: PathBuf,
        index_path: PathBuf,
        listener: Option<instance::Listener>,
        launch: Launch,
    ) -> App {
        let ctx = cc.egui_ctx.clone();
        fonts::install(&ctx);
        apply_theme(&ctx, cfg.theme);
        ctx.style_mut(|s| {
            s.spacing.item_spacing = egui::vec2(8.0, 4.0);
            s.spacing.scroll = egui::style::ScrollStyle::solid();
        });

        let c = ctx.clone();
        let engine = Engine::start(cfg.clone(), index_path.clone(), move || c.request_repaint());
        let shared = engine.shared.clone();
        let engine: EngineCell = Arc::new(Mutex::new(Some(engine)));
        let c = ctx.clone();
        let searcher = Searcher::new(shared.index.clone(), move || c.request_repaint());
        let signals = Arc::new(Signals::default());
        signals.visible.store(true, Ordering::Relaxed);
        let native = Arc::new(Native::new(cc));

        // Shows the window, or hides it if it's already in front.
        let summon = {
            let (s, n, c) = (signals.clone(), native.clone(), ctx.clone());
            move || {
                if s.visible.load(Ordering::Relaxed) && s.focused.load(Ordering::Relaxed) {
                    s.hide.store(true, Ordering::Relaxed);
                } else {
                    s.show.store(true, Ordering::Relaxed);
                    n.raise();
                }
                c.request_repaint();
            }
        };
        if let Some(l) = listener {
            let (s, n, c) = (signals.clone(), native.clone(), ctx.clone());
            instance::serve(l, move |query| {
                if query.is_some() {
                    *s.query.lock() = query;
                }
                s.show.store(true, Ordering::Relaxed);
                n.raise();
                c.request_repaint();
            });
        }
        let mut hotkeys = hotkey::Hotkeys::new(summon);
        let hotkey_error = if launch.smoke_test { None } else { hotkeys.set(&cfg.hotkey).err() };

        #[cfg(feature = "tray")]
        let (tray, tray_error) = {
            let (s, n, c, e) = (signals.clone(), native.clone(), ctx.clone(), engine.clone());
            let handler: tray::Handler = Arc::new(move |action| match action {
                tray::Action::Show | tray::Action::Settings => {
                    if action == tray::Action::Settings {
                        s.settings.store(true, Ordering::Relaxed);
                    }
                    s.show.store(true, Ordering::Relaxed);
                    n.raise();
                    c.request_repaint();
                }
                tray::Action::Rescan => {
                    s.rescan.store(true, Ordering::Relaxed);
                    c.request_repaint();
                    if let Some(engine) = e.lock().as_ref() {
                        engine.rescan();
                    }
                }
                tray::Action::Quit => {
                    // The window may be hidden and not repainting, so save and leave from here.
                    if let Some(mut engine) = e.lock().take() {
                        engine.shutdown();
                    }
                    std::process::exit(0);
                }
            });
            match tray::start(handler) {
                Ok(t) => (Some(t), None),
                Err(e) => (None, Some(e)),
            }
        };
        #[cfg(not(feature = "tray"))]
        let tray_error: Option<String> = None;

        let opts = Options { match_path: cfg.match_path, show_hidden: cfg.show_hidden };
        let (api, api_error) = start_api(&cfg, &shared, opts);
        let autostart = autostart::launcher();
        let settings = settings::State::new(&cfg, autostart.as_ref().is_some_and(autostart::is_enabled));

        App {
            cfg_path,
            index_path,
            shared,
            engine,
            searcher,
            api,
            api_error,
            signals,
            hotkeys,
            hotkey_error,
            #[cfg(feature = "tray")]
            tray,
            tray_error,
            autostart,
            query: if launch.smoke_test { "smoke-test".into() } else { launch.query.clone().unwrap_or_default() },
            sent: None,
            results: Arc::new(Vec::new()),
            results_epoch: 0,
            results_query: String::new(),
            highlights: Vec::new(),
            search_error: None,
            search_time: Duration::ZERO,
            rows: HashMap::new(),
            rows_key: (0, 0),
            indexed: 0,
            selected: HashSet::new(),
            cursor: None,
            anchor: None,
            scroll_to: None,
            focus_search: true,
            select_all_query: false,
            message: None,
            settings,
            show_settings: false,
            show_help: false,
            start_hidden: launch.hidden,
            quitting: false,
            smoke: launch.smoke_test.then(Instant::now),
            frames: 0,
            cfg,
        }
    }

    fn opts(&self) -> Options {
        Options { match_path: self.cfg.match_path, show_hidden: self.cfg.show_hidden }
    }

    fn has_tray(&self) -> bool {
        #[cfg(feature = "tray")]
        {
            self.tray.is_some()
        }
        #[cfg(not(feature = "tray"))]
        {
            false
        }
    }

    fn with_engine(&self, f: impl FnOnce(&Engine)) {
        if let Some(e) = self.engine.lock().as_ref() {
            f(e);
        }
    }

    fn say(&mut self, text: impl Into<String>) {
        self.message = Some((text.into(), Instant::now()));
    }

    /// Saves settings and hands them to the engine (which rescans if the indexed set changed).
    fn apply_config(&mut self, cfg: Config) {
        let restart_api = cfg.http_enabled != self.cfg.http_enabled || cfg.http_port != self.cfg.http_port;
        self.cfg = cfg;
        if let Err(e) = self.cfg.save(&self.cfg_path) {
            self.say(format!("Could not save settings: {e}"));
        }
        let c = self.cfg.clone();
        self.with_engine(|e| e.configure(c));
        if restart_api {
            self.api = None;
            let (api, err) = start_api(&self.cfg, &self.shared, self.opts());
            self.api = api;
            self.api_error = err;
        }
    }

    fn hide(&mut self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        self.signals.visible.store(false, Ordering::Relaxed);
        self.signals.focused.store(false, Ordering::Relaxed);
    }

    fn handle_signals(&mut self, ctx: &egui::Context) {
        if self.signals.show.swap(false, Ordering::Relaxed) {
            native::sync_shown(ctx);
            self.signals.visible.store(true, Ordering::Relaxed);
            self.focus_search = true;
            self.select_all_query = true;
        }
        if let Some(q) = self.signals.query.lock().take() {
            self.query = q;
        }
        if self.signals.hide.swap(false, Ordering::Relaxed) {
            self.hide(ctx);
        }
        if self.signals.settings.swap(false, Ordering::Relaxed) {
            self.show_settings = true;
        }
        if self.signals.rescan.swap(false, Ordering::Relaxed) {
            self.say("Rescanning");
        }
        let focused = ctx.input(|i| i.viewport().focused.unwrap_or(false));
        self.signals.focused.store(focused, Ordering::Relaxed);
        if ctx.input(|i| i.viewport().close_requested()) && !self.quitting && self.cfg.close_to_tray && self.has_tray()
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.hide(ctx);
        }
    }

    /// Starts a search when the query, the options or the index changed.
    fn maybe_search(&mut self, ctx: &egui::Context) {
        if let Some(o) = self.searcher.take() {
            // A different query starts a fresh selection; an index refresh keeps it.
            let new_query = o.request.text != self.results_query;
            self.results_query = o.request.text;
            self.results = o.ids;
            self.results_epoch = o.epoch;
            self.highlights = o.highlights;
            self.search_error = o.error;
            self.search_time = o.elapsed;
            if new_query {
                self.selected.clear();
                self.cursor = None;
                self.anchor = None;
                self.scroll_to = Some(0);
            } else {
                let n = self.results.len();
                self.cursor = self.cursor.filter(|&c| c < n);
            }
        }
        let Some(ix) = self.shared.index.try_read() else {
            ctx.request_repaint_after(Duration::from_millis(50));
            return;
        };
        let (revision, epoch) = (ix.revision, ix.epoch);
        drop(ix);
        let req =
            Request { text: self.query.clone(), opts: self.opts(), sort: self.cfg.sort, desc: self.cfg.sort_desc };
        let due = match &self.sent {
            None => true,
            Some(s) if s.req != req || s.epoch != epoch => true,
            Some(s) if s.revision != revision => {
                // The index changed under the same query: refresh, at most a couple of times a second.
                let wait = Duration::from_millis(500).saturating_sub(s.at.elapsed());
                if wait.is_zero() {
                    true
                } else {
                    ctx.request_repaint_after(wait);
                    false
                }
            }
            _ => false,
        };
        if due {
            self.searcher.submit(req.clone());
            self.sent = Some(Sent { req, revision, epoch, at: Instant::now() });
        }
    }

    fn selected_paths(&self, ix: &Index) -> Vec<String> {
        let mut rows: Vec<usize> = Vec::new();
        if self.selected.is_empty() {
            rows.extend(self.cursor);
        }
        let mut out: Vec<String> = Vec::new();
        if !self.selected.is_empty() {
            // Keep the on-screen order.
            for &id in self.results.iter() {
                if self.selected.contains(&id) {
                    out.push(ix.path(id));
                }
            }
        }
        for r in rows {
            if let Some(&id) = self.results.get(r) {
                out.push(ix.path(id));
            }
        }
        out
    }

    fn open_selected(&mut self, reveal: bool) {
        let paths = {
            let ix = self.shared.index.read();
            if self.results_epoch != ix.epoch {
                return;
            }
            self.selected_paths(&ix)
        };
        if paths.len() > 25 {
            self.say(format!("Not opening {} items at once; select 25 or fewer", paths.len()));
            return;
        }
        for p in paths {
            let r = if reveal { opener::reveal(&p) } else { opener::open(&p) };
            if let Err(e) = r {
                self.say(format!("Could not open {p}: {e}"));
            }
        }
    }

    fn copy_selected(&mut self, ctx: &egui::Context, names_only: bool) {
        let paths = {
            let ix = self.shared.index.read();
            if self.results_epoch != ix.epoch {
                return;
            }
            self.selected_paths(&ix)
        };
        if paths.is_empty() {
            return;
        }
        let text: Vec<String> = if names_only {
            paths.iter().map(|p| p.rsplit(['/', '\\']).next().unwrap_or(p).to_string()).collect()
        } else {
            paths
        };
        let n = text.len();
        ctx.copy_text(text.join("\n"));
        self.say(if n == 1 { "Copied".to_string() } else { format!("Copied {n} items") });
    }

    fn move_cursor(&mut self, to: usize, extend: bool) {
        let n = self.results.len();
        if n == 0 {
            return;
        }
        let to = to.min(n - 1);
        if extend {
            let anchor = *self.anchor.get_or_insert(self.cursor.unwrap_or(to));
            let (a, b) = (anchor.min(to), anchor.max(to));
            self.selected = self.results[a..=b].iter().copied().collect();
        } else {
            self.anchor = Some(to);
            self.selected.clear();
            self.selected.insert(self.results[to]);
        }
        self.cursor = Some(to);
        self.scroll_to = Some(to);
    }

    /// Keys that work everywhere, and list navigation when the search box doesn't have focus.
    fn keyboard(&mut self, ctx: &egui::Context) {
        let search_id = egui::Id::new(SEARCH_ID);
        let search_focused = ctx.memory(|m| m.has_focus(search_id));
        // A text field in the settings window has the keyboard; leave it alone.
        let elsewhere = ctx.memory(|m| m.focused().is_some_and(|f| f != search_id));
        let page = 20;
        let mut actions: Vec<KeyAction> = Vec::new();
        ctx.input_mut(|i| {
            let cmd = i.modifiers.command;
            if i.consume_key(Modifiers::COMMAND, Key::F) || i.consume_key(Modifiers::COMMAND, Key::L) {
                actions.push(Box::new(|a, _| {
                    a.focus_search = true;
                    a.select_all_query = true;
                }));
            }
            if i.consume_key(Modifiers::NONE, Key::F1) {
                actions.push(Box::new(|a, _| a.show_help = !a.show_help));
            }
            if i.consume_key(Modifiers::NONE, Key::F5) {
                actions.push(Box::new(|a, _| {
                    a.with_engine(|e| e.rescan());
                    a.say("Rescanning");
                }));
            }
            if i.consume_key(Modifiers::NONE, Key::Escape) {
                actions.push(Box::new(|a, ctx| {
                    if a.show_settings || a.show_help {
                        a.show_settings = false;
                        a.show_help = false;
                    } else if !a.query.is_empty() {
                        a.query.clear();
                        a.focus_search = true;
                    } else if a.has_tray() && a.cfg.close_to_tray {
                        a.hide(ctx);
                    } else {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                    }
                }));
            }
            let n = self.results.len();
            let cur = self.cursor;
            let shift = i.modifiers.shift;
            if elsewhere {
                return;
            }
            if search_focused {
                if i.consume_key(Modifiers::NONE, Key::ArrowDown) && n > 0 {
                    actions.push(Box::new(move |a, ctx| {
                        a.move_cursor(cur.map_or(0, |c| c + 1), false);
                        ctx.memory_mut(|m| m.surrender_focus(search_id));
                    }));
                }
                if i.consume_key(Modifiers::NONE, Key::Enter) && n > 0 {
                    actions.push(Box::new(move |a, _| {
                        if a.cursor.is_none() {
                            a.move_cursor(0, false);
                        }
                        a.open_selected(false);
                    }));
                }
                if i.consume_key(Modifiers::COMMAND, Key::Enter) && n > 0 {
                    actions.push(Box::new(move |a, _| {
                        if a.cursor.is_none() {
                            a.move_cursor(0, false);
                        }
                        a.open_selected(true);
                    }));
                }
                return;
            }
            // The list has the keyboard.
            let step = |k: Key, i: &mut egui::InputState| {
                i.consume_key(Modifiers::NONE, k) || i.consume_key(Modifiers::SHIFT, k)
            };
            let mut target = None;
            if step(Key::ArrowDown, i) {
                target = Some(cur.map_or(0, |c| c + 1));
            }
            if step(Key::ArrowUp, i) {
                match cur {
                    Some(0) | None if !shift => {
                        actions.push(Box::new(|a, _| a.focus_search = true));
                    }
                    Some(c) => target = Some(c.saturating_sub(1)),
                    None => {}
                }
            }
            if step(Key::PageDown, i) {
                target = Some(cur.map_or(0, |c| c + page));
            }
            if step(Key::PageUp, i) {
                target = Some(cur.map_or(0, |c| c.saturating_sub(page)));
            }
            if step(Key::Home, i) {
                target = Some(0);
            }
            if step(Key::End, i) {
                target = Some(n.saturating_sub(1));
            }
            if let Some(t) = target {
                actions.push(Box::new(move |a, _| a.move_cursor(t, shift)));
            }
            if i.consume_key(Modifiers::COMMAND, Key::A) {
                actions.push(Box::new(|a, _| {
                    if a.results.len() <= 1_000_000 {
                        a.selected = a.results.iter().copied().collect();
                    }
                }));
            }
            if i.consume_key(Modifiers::NONE, Key::Enter) {
                actions.push(Box::new(|a, _| a.open_selected(false)));
            }
            if i.consume_key(Modifiers::COMMAND, Key::Enter) {
                actions.push(Box::new(|a, _| a.open_selected(true)));
            }
            let copy = i.events.iter().any(|e| matches!(e, Event::Copy))
                || i.consume_key(Modifiers::COMMAND, Key::C)
                || i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::C);
            if copy {
                let names = i.modifiers.shift;
                actions.push(Box::new(move |a, ctx| a.copy_selected(ctx, names)));
            }
            // Typing goes back to the search box.
            let typed: String = i
                .events
                .iter()
                .filter_map(|e| match e {
                    Event::Text(t) if !cmd => Some(t.clone()),
                    _ => None,
                })
                .collect();
            if !typed.is_empty() {
                actions.push(Box::new(move |a, _| {
                    a.query.push_str(&typed);
                    a.focus_search = true;
                }));
            }
        });
        for act in actions {
            act(self, ctx);
        }
    }

    fn search_bar(&mut self, ui: &mut egui::Ui) {
        let search_id = egui::Id::new(SEARCH_ID);
        ui.horizontal(|ui| {
            let buttons = 150.0;
            let edit = egui::TextEdit::singleline(&mut self.query)
                .id(search_id)
                .hint_text("Search files and folders")
                .font(FontId::proportional(16.0))
                .margin(egui::vec2(8.0, 6.0))
                .desired_width((ui.available_width() - buttons).max(120.0));
            let resp = ui.add(edit);
            if self.focus_search {
                resp.request_focus();
                self.focus_search = false;
            }
            if self.select_all_query {
                self.select_all_query = false;
                if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), search_id) {
                    let end = self.query.chars().count();
                    state.cursor.set_char_range(Some(egui::text::CCursorRange::two(
                        egui::text::CCursor::new(0),
                        egui::text::CCursor::new(end),
                    )));
                    state.store(ui.ctx(), search_id);
                }
            }
            if ui.button("Syntax").on_hover_text("Search syntax (F1)").clicked() {
                self.show_help = !self.show_help;
            }
            if ui.button("Settings").clicked() {
                self.show_settings = !self.show_settings;
            }
        });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        let st = self.shared.status.lock().clone();
        if let Some(ix) = self.shared.index.try_read() {
            self.indexed = ix.live_count();
        }
        let live = self.indexed;
        ui.horizontal(|ui| {
            if let Some(err) = &self.search_error {
                ui.label(RichText::new(err).color(ui.visuals().error_fg_color));
            } else {
                let n = self.results.len();
                ui.label(format!("{} {}", fmt::count(n), if n == 1 { "object" } else { "objects" }));
                ui.label(RichText::new(format!("{} ms", self.search_time.as_millis())).weak());
            }
            if let Some((m, at)) = &self.message
                && at.elapsed() < Duration::from_secs(4)
            {
                ui.separator();
                ui.label(m);
                ui.ctx().request_repaint_after(Duration::from_millis(500));
            }
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                let watch = match &st.watching {
                    Watching::On => ("Live".to_string(), None),
                    Watching::Off => ("Live updates off".to_string(), None),
                    Watching::Partly(why) => ("Live (partly)".to_string(), Some(why.clone())),
                    Watching::Failed(why) => ("Not live".to_string(), Some(why.clone())),
                };
                let r = ui.label(RichText::new(watch.0).weak());
                if let Some(why) = watch.1 {
                    r.on_hover_text(why);
                }
                ui.separator();
                if st.scanning {
                    let n = self.shared.progress.items.load(Ordering::Relaxed);
                    ui.label(format!("Indexing {}", fmt::count(n as usize)));
                    ui.spinner();
                    ui.ctx().request_repaint_after(Duration::from_millis(200));
                } else if live > 0 {
                    let mut text = format!("{} indexed", fmt::count(live));
                    if let Some((_, took, _)) = st.last_scan {
                        text.push_str(&format!(" in {:.0}s", took.as_secs_f32()));
                    }
                    ui.label(RichText::new(text).weak());
                }
                if let Some(e) = &st.error {
                    ui.separator();
                    ui.label(RichText::new(e).color(ui.visuals().error_fg_color));
                }
            });
        });
    }

    /// Fills the row cache for `ids` if the index can be read right now.
    fn refresh_rows(&mut self, ids: &[u32]) {
        let Some(ix) = self.shared.index.try_read_for(Duration::from_millis(15)) else { return };
        if ix.epoch != self.results_epoch {
            return;
        }
        let key = (ix.epoch, ix.revision);
        if key != self.rows_key || self.rows.len() > 4000 {
            self.rows.clear();
            self.rows_key = key;
        }
        for &id in ids {
            if self.rows.contains_key(&id) || id as usize >= ix.len() {
                continue;
            }
            let e = ix.entry(id);
            self.rows.insert(
                id,
                Row {
                    name: ix.name(id).to_string(),
                    folder: ix.parent_path(id),
                    size: e.size,
                    mtime: e.mtime(),
                    dir: e.is_dir(),
                    gone: e.is_deleted(),
                },
            );
        }
    }

    fn table(&mut self, ui: &mut egui::Ui, frame: &eframe::Frame) {
        let stale = self.shared.index.try_read().is_some_and(|ix| ix.epoch != self.results_epoch);
        if stale || self.results_epoch == 0 {
            ui.centered_and_justified(|ui| {
                let st = self.shared.status.lock().clone();
                ui.label(RichText::new(if st.scanning { "Building the index..." } else { "Updating..." }).weak());
            });
            return;
        }
        let row_h = 20.0;
        let n = self.results.len();
        // Cache rows around the visible window: a screenful either side is plenty.
        let first = self.scroll_to.or(self.cursor).unwrap_or(0).saturating_sub(60);
        let visible_guess: Vec<u32> = self.results.iter().skip(first).take(160).copied().collect();
        self.refresh_rows(&visible_guess);

        let sort = self.cfg.sort;
        let desc = self.cfg.sort_desc;
        let mut clicked_header: Option<SortKey> = None;
        let mut row_clicked: Option<(usize, Modifiers)> = None;
        let mut row_double: Option<usize> = None;
        let mut drag_from: Option<usize> = None;
        let mut menu_action: Option<(usize, MenuAction)> = None;
        let mut needed: Vec<u32> = Vec::new();
        let highlight_bg = ui.visuals().selection.bg_fill.gamma_multiply(0.55);
        let text_color = ui.visuals().text_color();
        let weak = ui.visuals().weak_text_color();

        let mut builder = TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .sense(Sense::click_and_drag())
            .cell_layout(egui::Layout::left_to_right(Align::Center))
            .column(Column::initial(300.0).at_least(120.0).clip(true))
            .column(Column::remainder().at_least(120.0).clip(true))
            .column(Column::initial(80.0).at_least(60.0).clip(true))
            .column(Column::initial(130.0).at_least(90.0).clip(true))
            .min_scrolled_height(0.0);
        if let Some(r) = self.scroll_to.take() {
            builder = builder.scroll_to_row(r, None);
        }
        builder
            .header(22.0, |mut header| {
                for (key, label) in [
                    (SortKey::Name, "Name"),
                    (SortKey::Path, "Path"),
                    (SortKey::Size, "Size"),
                    (SortKey::Modified, "Modified"),
                ] {
                    header.col(|ui| {
                        let arrow = if sort == key { if desc { " \u{23f7}" } else { " \u{23f6}" } } else { "" };
                        let r = ui.add(
                            egui::Label::new(RichText::new(format!("{label}{arrow}")).strong()).sense(Sense::click()),
                        );
                        if r.clicked() {
                            clicked_header = Some(key);
                        }
                    });
                }
            })
            .body(|body| {
                body.rows(row_h, n, |mut row| {
                    let i = row.index();
                    let id = self.results[i];
                    row.set_selected(
                        self.selected.contains(&id) || (self.selected.is_empty() && self.cursor == Some(i)),
                    );
                    let Some(r) = self.rows.get(&id).cloned() else {
                        needed.push(id);
                        row.col(|ui| {
                            ui.label("");
                        });
                        return;
                    };
                    let color = if r.gone { weak } else { text_color };
                    row.col(|ui| {
                        icon(ui, r.dir);
                        ui.add(
                            egui::Label::new(highlight(&r.name, &self.highlights, color, highlight_bg))
                                .truncate()
                                .selectable(false),
                        );
                    });
                    row.col(|ui| {
                        ui.add(egui::Label::new(RichText::new(&r.folder).color(weak)).truncate().selectable(false));
                    });
                    row.col(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                            ui.add(egui::Label::new(RichText::new(fmt::size(r.size)).color(color)).selectable(false));
                        });
                    });
                    row.col(|ui| {
                        ui.add(
                            egui::Label::new(RichText::new(fmt::date(r.mtime)).color(weak))
                                .truncate()
                                .selectable(false),
                        );
                    });
                    let resp = row.response();
                    if resp.double_clicked() {
                        row_double = Some(i);
                    } else if resp.clicked() {
                        row_clicked = Some((i, resp.ctx.input(|inp| inp.modifiers)));
                    }
                    if resp.drag_started() {
                        drag_from = Some(i);
                    }
                    if resp.secondary_clicked() && !self.selected.contains(&id) {
                        row_clicked = Some((i, Modifiers::NONE));
                    }
                    resp.context_menu(|ui| {
                        ui.set_min_width(200.0);
                        for (label, act) in [
                            ("Open", MenuAction::Open),
                            (reveal_label(), MenuAction::Reveal),
                            ("Copy full path", MenuAction::CopyPath),
                            ("Copy name", MenuAction::CopyName),
                            ("Search in this folder", MenuAction::SearchHere),
                        ] {
                            if ui.button(label).clicked() {
                                menu_action = Some((i, act));
                                ui.close();
                            }
                        }
                    });
                });
            });

        if !needed.is_empty() {
            self.refresh_rows(&needed);
            ui.ctx().request_repaint();
        }
        if let Some(key) = clicked_header {
            let mut cfg = self.cfg.clone();
            if cfg.sort == key {
                cfg.sort_desc = !cfg.sort_desc;
            } else {
                cfg.sort = key;
                // Big and recent first is what people want when they sort by size or date.
                cfg.sort_desc = matches!(key, SortKey::Size | SortKey::Modified);
            }
            self.apply_config(cfg);
        }
        if let Some((i, m)) = row_clicked {
            let id = self.results[i];
            if m.command {
                if !self.selected.insert(id) {
                    self.selected.remove(&id);
                }
                self.cursor = Some(i);
                self.anchor = Some(i);
            } else if m.shift {
                self.move_cursor(i, true);
            } else {
                self.move_cursor(i, false);
            }
            ui.ctx().memory_mut(|mem| mem.surrender_focus(egui::Id::new(SEARCH_ID)));
        }
        if let Some(i) = row_double {
            self.move_cursor(i, false);
            self.open_selected(false);
        }
        if let Some((i, act)) = menu_action {
            if !self.selected.contains(&self.results[i]) {
                self.move_cursor(i, false);
            }
            match act {
                MenuAction::Open => self.open_selected(false),
                MenuAction::Reveal => self.open_selected(true),
                MenuAction::CopyPath => self.copy_selected(ui.ctx(), false),
                MenuAction::CopyName => self.copy_selected(ui.ctx(), true),
                MenuAction::SearchHere => {
                    let ix = self.shared.index.read();
                    let id = self.results[i];
                    let folder = if ix.entry(id).is_dir() { ix.path(id) } else { ix.parent_path(id) };
                    drop(ix);
                    self.query = format!("parent:\"{folder}\" ");
                    self.focus_search = true;
                }
            }
        }
        if let Some(i) = drag_from {
            if !self.selected.contains(&self.results[i]) {
                self.move_cursor(i, false);
            }
            if dnd::SUPPORTED {
                let paths: Vec<PathBuf> = {
                    let ix = self.shared.index.read();
                    self.selected_paths(&ix).into_iter().take(500).map(PathBuf::from).collect()
                };
                if let Err(e) = dnd::start(frame, paths) {
                    self.say(format!("Drag failed: {e}"));
                }
            }
        }
    }
}

/// Work decided while reading input, done once the input borrow ends.
type KeyAction = Box<dyn FnOnce(&mut App, &egui::Context)>;

#[derive(Clone, Copy)]
enum MenuAction {
    Open,
    Reveal,
    CopyPath,
    CopyName,
    SearchHere,
}

fn reveal_label() -> &'static str {
    if cfg!(windows) {
        "Show in Explorer"
    } else if cfg!(target_os = "macos") {
        "Show in Finder"
    } else {
        "Show in folder"
    }
}

fn start_api(cfg: &Config, shared: &Arc<Shared>, opts: Options) -> (Option<Api>, Option<String>) {
    if !cfg.http_enabled {
        return (None, None);
    }
    match Api::start(shared.clone(), cfg.http_port, opts) {
        Ok(a) => (Some(a), None),
        Err(e) => (None, Some(e)),
    }
}

fn apply_theme(ctx: &egui::Context, theme: Theme) {
    ctx.set_theme(match theme {
        Theme::System => egui::ThemePreference::System,
        Theme::Light => egui::ThemePreference::Light,
        Theme::Dark => egui::ThemePreference::Dark,
    });
}

/// A small folder or page shape in front of the name.
fn icon(ui: &mut egui::Ui, dir: bool) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(14.0, 14.0), Sense::hover());
    let p = ui.painter();
    if dir {
        let body = Rect::from_min_max(rect.min + egui::vec2(0.5, 3.5), rect.max - egui::vec2(0.5, 1.5));
        let tab = Rect::from_min_size(rect.min + egui::vec2(0.5, 1.5), egui::vec2(6.0, 3.0));
        let fill = Color32::from_rgb(222, 165, 72);
        p.rect_filled(tab, CornerRadius::same(1), fill);
        p.rect_filled(body, CornerRadius::same(2), fill);
    } else {
        let page = Rect::from_min_max(rect.min + egui::vec2(2.5, 0.5), rect.max - egui::vec2(2.5, 0.5));
        let stroke = Stroke::new(1.0_f32, ui.visuals().weak_text_color());
        p.rect_stroke(page, CornerRadius::same(1), stroke, StrokeKind::Inside);
    }
}

/// The name with every occurrence of a search term marked.
fn highlight(name: &str, terms: &[String], color: Color32, bg: Color32) -> LayoutJob {
    let font = FontId::proportional(14.0);
    let plain = TextFormat { font_id: font.clone(), color, ..Default::default() };
    let marked = TextFormat { font_id: font, color, background: bg, ..Default::default() };
    let mut job = LayoutJob::default();
    let lower = name.to_ascii_lowercase();
    let mut marks = vec![false; name.len()];
    for t in terms {
        if t.is_empty() {
            continue;
        }
        let t = t.to_ascii_lowercase();
        let mut from = 0;
        while let Some(pos) = lower[from..].find(&t) {
            let at = from + pos;
            marks[at..at + t.len()].iter_mut().for_each(|m| *m = true);
            from = at + t.len().max(1);
        }
    }
    let mut start = 0;
    while start < name.len() {
        let on = marks[start];
        let mut end = start;
        while end < name.len() && marks[end] == on {
            end += 1;
        }
        while !name.is_char_boundary(end) {
            end += 1;
        }
        job.append(&name[start..end], 0.0, if on { marked.clone() } else { plain.clone() });
        start = end;
    }
    job.wrap.max_rows = 1;
    job
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.frames += 1;
        if self.start_hidden {
            self.start_hidden = false;
            self.hide(ctx);
        }
        self.handle_signals(ctx);
        self.keyboard(ctx);
        self.maybe_search(ctx);

        egui::TopBottomPanel::top("search")
            .frame(egui::Frame::side_top_panel(&ctx.style()).inner_margin(8.0))
            .show(ctx, |ui| self.search_bar(ui));
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| self.status_bar(ui));
        egui::CentralPanel::default()
            .frame(egui::Frame::central_panel(&ctx.style()).inner_margin(0.0))
            .show(ctx, |ui| self.table(ui, frame));

        if self.show_help {
            help::window(ctx, &mut self.show_help);
        }
        if self.show_settings {
            let mut open = true;
            let has_tray = self.has_tray();
            let out = settings::window(
                ctx,
                &mut open,
                &mut self.settings,
                &self.cfg,
                settings::Info {
                    api_port: self.api.as_ref().map(|a| a.port),
                    api_error: self.api_error.as_deref(),
                    hotkey_error: self.hotkey_error.as_deref(),
                    tray_error: self.tray_error.as_deref(),
                    has_tray,
                    autostart_available: self.autostart.is_some(),
                    config_path: &self.cfg_path,
                    index_path: &self.index_path,
                },
            );
            self.show_settings = open;
            self.apply_settings(ctx, out);
        }

        if let Some(started) = self.smoke {
            let found = self.results.len() == 1 && self.results_epoch > 0;
            if found || started.elapsed() > Duration::from_secs(60) {
                if !found {
                    SMOKE_FAILED.store(true, Ordering::Relaxed);
                }
                println!("gui smoke test: {} after {} frames", if found { "ok" } else { "FAILED" }, self.frames);
                self.smoke = None;
                self.quitting = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }
}

impl App {
    fn apply_settings(&mut self, ctx: &egui::Context, out: settings::Outcome) {
        if let Some(cfg) = out.config {
            if cfg.theme != self.cfg.theme {
                apply_theme(ctx, cfg.theme);
            }
            self.apply_config(cfg);
        }
        if let Some(spec) = out.hotkey {
            match self.hotkeys.set(&spec) {
                Ok(()) => {
                    self.hotkey_error = None;
                    let mut cfg = self.cfg.clone();
                    cfg.hotkey = spec;
                    self.apply_config(cfg);
                }
                Err(e) => self.hotkey_error = Some(e),
            }
        }
        if out.rescan {
            self.with_engine(|e| e.rescan());
            self.say("Rescanning");
        }
        if let Some(on) = out.autostart
            && let Some(l) = &self.autostart
        {
            match autostart::set(l, on) {
                Ok(()) => self.settings.autostart = autostart::is_enabled(l),
                Err(e) => self.say(format!("Could not change start at login: {e}")),
            }
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.api = None;
        if let Some(mut e) = self.engine.lock().take() {
            e.shutdown();
        }
        // The smoke test works in its own temporary folder.
        if let Some(dir) = self.cfg_path.parent()
            && dir.file_name().is_some_and(|n| n.to_string_lossy().starts_with("quarry-gui-smoke-"))
        {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}
