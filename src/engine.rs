//! Owns the index: loads it, scans, follows file system events, and saves it.
//!
//! One thread does all writing. Searches only take the read lock, so typing stays responsive while
//! a rescan runs (the old index keeps serving until the new one is swapped in).

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use crossbeam_channel::{Receiver, Sender, select, unbounded};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use parking_lot::{Mutex, RwLock};

use crate::config::{self, Config};
use crate::index::{DIR, Index, Meta};
use crate::scan::{self, Excludes, Node, Progress};

#[derive(Clone, Debug, Default, PartialEq)]
pub enum Watching {
    #[default]
    Off,
    On,
    /// Some roots could not be watched.
    Partly(String),
    Failed(String),
}

#[derive(Clone, Debug, Default)]
pub struct Status {
    pub scanning: bool,
    /// When the index was last fully scanned, how long it took, and how many unreadable folders it met.
    pub last_scan: Option<(SystemTime, Duration, u64)>,
    pub watching: Watching,
    pub loaded_from_disk: bool,
    pub error: Option<String>,
}

pub struct Shared {
    pub index: Arc<RwLock<Index>>,
    pub status: Mutex<Status>,
    pub progress: Progress,
    notify: Box<dyn Fn() + Send + Sync>,
}

impl Shared {
    fn changed(&self) {
        (self.notify)();
    }
}

enum Cmd {
    Rescan,
    Configure(Box<Config>),
    Shutdown,
}

pub struct Engine {
    pub shared: Arc<Shared>,
    tx: Sender<Cmd>,
    thread: Option<JoinHandle<()>>,
}

impl Engine {
    /// Loads the saved index (if any) and starts scanning and watching in the background.
    /// `notify` is called from the engine thread whenever the index or status changes.
    pub fn start(cfg: Config, index_path: PathBuf, notify: impl Fn() + Send + Sync + 'static) -> Engine {
        let shared = Arc::new(Shared {
            index: Arc::new(RwLock::new(Index::new())),
            status: Mutex::new(Status::default()),
            progress: Progress::default(),
            notify: Box::new(notify),
        });
        let (tx, rx) = unbounded();
        let s = shared.clone();
        let thread = std::thread::Builder::new()
            .name("engine".into())
            .spawn(move || Worker::new(s, cfg, index_path).run(rx))
            .expect("spawn engine thread");
        Engine { shared, tx, thread: Some(thread) }
    }

    pub fn rescan(&self) {
        let _ = self.tx.send(Cmd::Rescan);
    }

    /// Applies new settings; rescans if they change what gets indexed.
    pub fn configure(&self, cfg: Config) {
        let _ = self.tx.send(Cmd::Configure(Box::new(cfg)));
    }

    /// Stops scanning and saves the index. Blocks until done.
    pub fn shutdown(&mut self) {
        self.shared.progress.cancel.store(true, Ordering::Relaxed);
        let _ = self.tx.send(Cmd::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct Worker {
    shared: Arc<Shared>,
    cfg: Config,
    index_path: PathBuf,
    excludes: Excludes,
    watcher: Option<RecommendedWatcher>,
    events_tx: Sender<notify::Result<notify::Event>>,
    events_rx: Receiver<notify::Result<notify::Event>>,
    dirty: bool,
    last_save: Instant,
    last_scan: Instant,
}

impl Worker {
    fn new(shared: Arc<Shared>, cfg: Config, index_path: PathBuf) -> Worker {
        let (events_tx, events_rx) = unbounded();
        let excludes = excludes_for(&cfg);
        Worker {
            shared,
            cfg,
            index_path,
            excludes,
            watcher: None,
            events_tx,
            events_rx,
            dirty: false,
            last_save: Instant::now(),
            last_scan: Instant::now(),
        }
    }

    fn run(mut self, rx: Receiver<Cmd>) {
        let loaded = crate::store::load(&self.index_path, self.cfg.fingerprint());
        let mut need_scan = loaded.is_none() || self.cfg.rescan_on_start;
        if let Some(mut ix) = loaded {
            ix.epoch = 1;
            *self.shared.index.write() = ix;
            self.shared.status.lock().loaded_from_disk = true;
            self.shared.changed();
        }
        self.restart_watcher();
        loop {
            if need_scan {
                need_scan = false;
                if !self.full_scan() {
                    break;
                }
            }
            select! {
                recv(rx) -> cmd => match cmd {
                    Ok(Cmd::Rescan) => need_scan = true,
                    Ok(Cmd::Configure(c)) => need_scan = self.configure(*c),
                    Ok(Cmd::Shutdown) | Err(_) => break,
                },
                recv(self.events_rx) -> ev => {
                    if let Ok(ev) = ev {
                        need_scan |= self.handle_events(ev);
                    }
                },
                default(Duration::from_secs(20)) => {
                    if self.dirty && self.last_save.elapsed() > Duration::from_secs(600) {
                        self.save();
                    }
                    let every = self.cfg.rescan_minutes as u64 * 60;
                    if every > 0 && self.last_scan.elapsed() > Duration::from_secs(every) {
                        need_scan = true;
                    }
                },
            }
        }
        if self.dirty {
            self.save();
        }
    }

    /// Returns true if the new settings need a rescan.
    fn configure(&mut self, cfg: Config) -> bool {
        let rescan = cfg.fingerprint() != self.cfg.fingerprint();
        let rewatch = rescan || cfg.live_updates != self.cfg.live_updates;
        self.cfg = cfg;
        self.excludes = excludes_for(&self.cfg);
        if rewatch {
            self.restart_watcher();
        }
        rescan
    }

    fn restart_watcher(&mut self) {
        self.watcher = None;
        let state = if self.cfg.live_updates {
            let tx = self.events_tx.clone();
            match notify::recommended_watcher(move |res| {
                let _ = tx.send(res);
            }) {
                Ok(mut w) => {
                    let mut failed = Vec::new();
                    let roots = config::watch_roots(&scan::dedup_roots(&self.cfg.effective_roots()));
                    for r in &roots {
                        if let Err(e) = w.watch(r, RecursiveMode::Recursive) {
                            failed.push(format!("{}: {e}", r.display()));
                        }
                    }
                    let state = if failed.is_empty() {
                        Watching::On
                    } else if failed.len() == roots.len() {
                        Watching::Failed(failed.join("; "))
                    } else {
                        Watching::Partly(failed.join("; "))
                    };
                    self.watcher = Some(w);
                    state
                }
                Err(e) => Watching::Failed(e.to_string()),
            }
        } else {
            Watching::Off
        };
        self.shared.status.lock().watching = state;
        self.shared.changed();
    }

    /// Returns false if the scan was cancelled for shutdown.
    fn full_scan(&mut self) -> bool {
        let p = &self.shared.progress;
        p.items.store(0, Ordering::Relaxed);
        p.unreadable.store(0, Ordering::Relaxed);
        self.shared.status.lock().scanning = true;
        self.shared.changed();
        let start = Instant::now();
        let roots = self.cfg.effective_roots();
        let mut ix = scan::scan(&roots, &self.excludes, p);
        if p.cancelled() {
            return false;
        }
        {
            let mut cur = self.shared.index.write();
            ix.epoch = cur.epoch + 1;
            *cur = ix;
        }
        self.last_scan = Instant::now();
        {
            let mut st = self.shared.status.lock();
            st.scanning = false;
            st.last_scan = Some((SystemTime::now(), start.elapsed(), p.unreadable.load(Ordering::Relaxed)));
        }
        self.shared.changed();
        self.save();
        true
    }

    fn save(&mut self) {
        let result = {
            let ix = self.shared.index.read();
            crate::store::save(&ix, &self.index_path, self.cfg.fingerprint())
        };
        self.dirty = false;
        self.last_save = Instant::now();
        if let Err(e) = result {
            self.shared.status.lock().error = Some(format!("could not save the index: {e}"));
            self.shared.changed();
        }
    }

    /// Collects a burst of events, then applies them in one go. Returns true if the watcher lost
    /// track and a rescan is needed.
    fn handle_events(&mut self, first: notify::Result<notify::Event>) -> bool {
        let mut paths: HashSet<PathBuf> = HashSet::new();
        let mut rescan = false;
        self.take_event(first, &mut paths, &mut rescan);
        let deadline = Instant::now() + Duration::from_millis(250);
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match self.events_rx.recv_timeout(left) {
                Ok(ev) => self.take_event(ev, &mut paths, &mut rescan),
                Err(_) => break,
            }
            if paths.len() > 200_000 {
                break;
            }
        }
        let own = self.index_path.parent().map(Path::to_path_buf);
        paths.retain(|p| own.as_deref().is_none_or(|o| !p.starts_with(o)));
        if cfg!(target_os = "macos") {
            // FSEvents can report the data volume's own path for files that live at /Users/...
            paths = paths.into_iter().map(firmlinked).collect();
        }
        if !paths.is_empty() {
            let mut list: Vec<PathBuf> = paths.into_iter().collect();
            list.sort_by_key(|p| p.components().count());
            apply(&self.shared, &self.excludes, list);
            self.dirty = true;
            self.shared.changed();
        }
        rescan
    }
}

impl Worker {
    fn take_event(&self, ev: notify::Result<notify::Event>, paths: &mut HashSet<PathBuf>, rescan: &mut bool) {
        match ev {
            Ok(ev) => {
                if ev.need_rescan() {
                    *rescan = true;
                }
                if !matches!(ev.kind, EventKind::Access(_)) {
                    paths.extend(ev.paths);
                }
            }
            Err(e) => {
                if matches!(e.kind, notify::ErrorKind::MaxFilesWatch) {
                    self.shared.status.lock().watching =
                        Watching::Partly("the system limit on watched folders was reached".into());
                }
            }
        }
    }
}

/// `/System/Volumes/Data/Users/x` is `/Users/x` seen through the macOS firmlink.
fn firmlinked(p: PathBuf) -> PathBuf {
    match p.strip_prefix("/System/Volumes/Data") {
        Ok(rest) if !rest.as_os_str().is_empty() => Path::new("/").join(rest),
        _ => p,
    }
}

fn excludes_for(cfg: &Config) -> Excludes {
    let roots = cfg.effective_roots();
    Excludes::new(&cfg.excludes, config::builtin_excludes(), &roots)
}

enum Op {
    Remove(u32),
    Update(u32, Meta),
    Add(PathBuf, Meta, Vec<Node>),
}

/// Brings the index in line with the current state of `paths` on disk. File system work happens
/// under the read lock (searches keep running); the write lock is held only to apply the result.
pub fn apply(shared: &Shared, ex: &Excludes, paths: Vec<PathBuf>) {
    let quiet = Progress::default();
    let mut ops = Vec::new();
    {
        let ix = shared.index.read();
        for p in paths {
            if ix.root_for(&p).is_none() || ex.covers(&p) {
                continue;
            }
            let name = match p.file_name() {
                Some(n) => n.to_string_lossy().into_owned(),
                None => continue,
            };
            let existing = ix.lookup(&p);
            match fs::symlink_metadata(&p) {
                Err(_) => {
                    if let Some(id) = existing {
                        ops.push(Op::Remove(id));
                    }
                }
                Ok(md) => {
                    let meta = scan::meta_from(&md, &name);
                    let is_dir = meta.flags & DIR != 0;
                    let kids = |p: &Path| if is_dir { scan::scan_dir(p, ex, &quiet, &|_| {}) } else { Vec::new() };
                    match existing {
                        Some(id) if ix.entry(id).is_dir() == is_dir && ix.name(id) == name => {
                            ops.push(Op::Update(id, meta))
                        }
                        Some(id) => {
                            ops.push(Op::Remove(id));
                            ops.push(Op::Add(p.clone(), meta, kids(&p)));
                        }
                        None => ops.push(Op::Add(p.clone(), meta, kids(&p))),
                    }
                }
            }
        }
    }
    if ops.is_empty() {
        return;
    }
    let mut ix = shared.index.write();
    for op in &ops {
        if let Op::Remove(id) = op {
            ix.remove(*id);
        }
    }
    for op in ops {
        match op {
            Op::Remove(_) => {}
            Op::Update(id, meta) => {
                if !ix.entry(id).is_deleted() {
                    ix.update(id, meta.size, meta.mtime);
                }
            }
            Op::Add(path, meta, kids) => {
                if ix.lookup(&path).is_some() {
                    continue;
                }
                let Some(parent) = path.parent().and_then(|pp| ensure_dir(&mut ix, pp, ex)) else { continue };
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let id = ix.add(parent, &name, meta);
                scan::insert(&mut ix, id, kids);
                let total = scan::fix_sizes(&mut ix, id);
                ix.add_size(parent, total as i64);
            }
        }
    }
    ix.merge_pending();
}

/// The id of a folder, adding it (and missing folders above it) if events arrived out of order.
fn ensure_dir(ix: &mut Index, path: &Path, ex: &Excludes) -> Option<u32> {
    if let Some(id) = ix.lookup(path) {
        return ix.entry(id).is_dir().then_some(id);
    }
    ix.root_for(path)?;
    if ex.covers(path) {
        return None;
    }
    let parent = ensure_dir(ix, path.parent()?, ex)?;
    let md = fs::symlink_metadata(path).ok()?;
    let name = path.file_name()?.to_string_lossy().into_owned();
    let meta = scan::meta_from(&md, &name);
    if meta.flags & DIR == 0 {
        return None;
    }
    Some(ix.add(parent, &name, meta))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_tracks_changes() {
        let dir = std::env::temp_dir().join(format!("quarry-apply-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("a")).unwrap();
        fs::write(dir.join("a/old.txt"), b"1234").unwrap();
        let ex = Excludes::default();
        let shared = Shared {
            index: Arc::new(RwLock::new(scan::scan(std::slice::from_ref(&dir), &ex, &Progress::default()))),
            status: Mutex::new(Status::default()),
            progress: Progress::default(),
            notify: Box::new(|| {}),
        };
        fs::create_dir_all(dir.join("a/new/deep")).unwrap();
        fs::write(dir.join("a/new/deep/f.bin"), b"123456").unwrap();
        fs::remove_file(dir.join("a/old.txt")).unwrap();
        apply(&shared, &ex, vec![dir.join("a/new"), dir.join("a/new/deep/f.bin"), dir.join("a/old.txt")]);
        let ix = shared.index.read();
        assert!(ix.lookup(&dir.join("a").join("old.txt")).is_none());
        let f = ix.lookup(&dir.join("a").join("new").join("deep").join("f.bin")).unwrap();
        assert_eq!(ix.entry(f).size, 6);
        assert_eq!(ix.entry(ix.roots()[0]).size, 6);
        assert_eq!(ix.by_name().len(), ix.live_count());
        drop(ix);
        fs::remove_dir_all(&dir).unwrap();
    }
}
