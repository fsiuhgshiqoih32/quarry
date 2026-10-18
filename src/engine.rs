//! Owns the index: loads it, scans, follows file system events, and saves it.
//!
//! One thread does all writing. Searches only take the read lock, so typing stays responsive while
//! the index changes. At start-up a saved index is searchable at once and is then brought up to
//! date in place, folder by folder; a full rescan builds a new index and swaps it in.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use crossbeam_channel::{Receiver, Sender, select, unbounded};
use parking_lot::{Mutex, RwLock};

use crate::config::{self, Config};
use crate::index::{DIR, Index, Meta, SYMLINK};
use crate::scan::{self, Excludes, Node, OnDir, Progress};
use crate::watch::{self, Change, Watcher};

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
    watcher: Option<Watcher>,
    events_tx: Sender<Change>,
    events_rx: Receiver<Change>,
    dirty: bool,
    last_save: Instant,
    last_scan: Instant,
}

/// What a burst of file system events asks for beyond patching the changed paths.
enum Resync {
    Nothing,
    /// Events were lost below these folders.
    Folders(Vec<PathBuf>),
    /// Events were lost everywhere.
    Everything,
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
        let have_index = loaded.is_some();
        if let Some(mut ix) = loaded {
            ix.epoch = 1;
            *self.shared.index.write() = ix;
            self.shared.status.lock().loaded_from_disk = true;
            self.shared.changed();
        }
        self.restart_watcher();
        let mut need_scan = !have_index;
        let mut need_refresh = have_index && self.cfg.rescan_on_start;
        loop {
            if need_scan {
                need_scan = false;
                need_refresh = false;
                if !self.full_scan() {
                    break;
                }
            }
            if need_refresh {
                need_refresh = false;
                if !self.refresh(None) {
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
                        match self.handle_events(ev) {
                            Resync::Nothing => {}
                            Resync::Everything => need_refresh = true,
                            Resync::Folders(dirs) => {
                                for d in dirs {
                                    if !self.refresh(Some(d)) {
                                        break;
                                    }
                                }
                            }
                        }
                    }
                },
                default(Duration::from_secs(20)) => {
                    if self.dirty && self.last_save.elapsed() > Duration::from_secs(600) {
                        self.save();
                    }
                    let every = self.refresh_minutes() as u64 * 60;
                    if every > 0 && self.last_scan.elapsed() > Duration::from_secs(every) {
                        need_refresh = true;
                    }
                },
            }
            if self.shared.progress.cancelled() {
                break;
            }
        }
        if self.dirty {
            self.save();
        }
    }

    /// Minutes between periodic refreshes. Folders the watcher cannot cover (the inotify limit
    /// on Linux, for instance) get an hourly refresh even when the setting is off.
    fn refresh_minutes(&self) -> u32 {
        if self.cfg.rescan_minutes > 0 {
            return self.cfg.rescan_minutes;
        }
        let watching = self.shared.status.lock().watching.clone();
        if self.cfg.live_updates && matches!(watching, Watching::Partly(_) | Watching::Failed(_)) { 60 } else { 0 }
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
        if self.cfg.live_updates {
            let w = Watcher::new(self.events_tx.clone());
            for r in &scan::dedup_roots(&self.cfg.effective_roots()) {
                w.watch_root(r);
            }
            self.watcher = Some(w);
            self.watch_indexed_folders();
        }
        self.publish_watch_state();
    }

    /// Mirrors the watcher's state into the status, if it changed.
    fn publish_watch_state(&self) {
        let state = match &self.watcher {
            None => Watching::Off,
            Some(w) => match w.state() {
                watch::State::On => Watching::On,
                watch::State::Partly(why) => Watching::Partly(why),
                watch::State::Failed(why) => Watching::Failed(why),
            },
        };
        let mut st = self.shared.status.lock();
        if st.watching != state {
            st.watching = state;
            drop(st);
            self.shared.changed();
        }
    }

    /// Linux: gives every indexed folder an inotify watch, the home folder first so it stays live
    /// if the kernel's limit runs out. Folders that already have one are unaffected.
    fn watch_indexed_folders(&self) {
        let Some(w) = &self.watcher else { return };
        if !w.wants_dirs() {
            return;
        }
        let ix = self.shared.index.read();
        let home = dirs::home_dir();
        let mut path = String::new();
        let mut later = Vec::new();
        for id in 0..ix.len() as u32 {
            let e = ix.entry(id);
            if !e.is_dir() || e.is_deleted() || e.flags & SYMLINK != 0 {
                continue;
            }
            ix.path_into(id, &mut path);
            if home.as_deref().is_some_and(|h| Path::new(&path).starts_with(h)) {
                w.watch_dir(Path::new(&path));
            } else {
                later.push(id);
            }
        }
        for id in later {
            ix.path_into(id, &mut path);
            w.watch_dir(Path::new(&path));
        }
        drop(ix);
        self.publish_watch_state();
    }

    /// During scans only folders in the home folder get their watch at once; the rest follow
    /// when the scan is done, in [`Worker::watch_indexed_folders`].
    fn on_dir(&self) -> impl Fn(&Path) + Sync + '_ {
        let home = dirs::home_dir();
        move |dir: &Path| {
            if let Some(w) = &self.watcher
                && home.as_deref().is_some_and(|h| dir.starts_with(h))
            {
                w.watch_dir(dir);
            }
        }
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
        let mut ix = scan::scan_with(&roots, &self.excludes, p, &self.on_dir());
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
        self.watch_indexed_folders();
        self.save();
        true
    }

    /// Brings the index up to date in place: below one folder, or everywhere with `None`. The
    /// index stays searchable and keeps what was done if Quarry quits part-way. Returns false if
    /// cancelled for shutdown.
    fn refresh(&mut self, below: Option<PathBuf>) -> bool {
        let start: Vec<scan::Job> = {
            let ix = self.shared.index.read();
            match &below {
                None => ix.roots().iter().map(|&r| (r, PathBuf::from(ix.name(r)))).collect(),
                Some(p) => match ix.lookup(p) {
                    Some(id) if ix.entry(id).is_dir() => vec![(id, p.clone())],
                    _ => {
                        drop(ix);
                        apply(&self.shared, &self.excludes, vec![p.clone()], &self.on_dir());
                        self.dirty = true;
                        return true;
                    }
                },
            }
        };
        let everything = below.is_none();
        let shared = self.shared.clone();
        let p = &shared.progress;
        if everything {
            p.items.store(0, Ordering::Relaxed);
            p.unreadable.store(0, Ordering::Relaxed);
            self.shared.status.lock().scanning = true;
            self.shared.changed();
        }
        let started = Instant::now();
        let done = scan::refresh(&shared.index, start, &self.excludes, p, &self.on_dir(), &|| shared.changed());
        self.dirty = true;
        if everything {
            let mut st = self.shared.status.lock();
            st.scanning = false;
            if done {
                st.last_scan = Some((SystemTime::now(), started.elapsed(), p.unreadable.load(Ordering::Relaxed)));
            }
            drop(st);
            self.shared.changed();
            if done {
                self.last_scan = Instant::now();
                self.watch_indexed_folders();
                self.save();
            }
        }
        !p.cancelled()
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

    /// Collects a burst of events, then applies them in one go.
    fn handle_events(&mut self, first: Change) -> Resync {
        let mut paths: HashSet<PathBuf> = HashSet::new();
        let mut lost: Vec<PathBuf> = Vec::new();
        let mut lost_all = false;
        let mut take = |c: Change, paths: &mut HashSet<PathBuf>| match c {
            Change::Path(p) => {
                paths.insert(p);
            }
            Change::Rescan(Some(p)) => lost.push(p),
            Change::Rescan(None) => lost_all = true,
        };
        take(first, &mut paths);
        let deadline = Instant::now() + Duration::from_millis(250);
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match self.events_rx.recv_timeout(left) {
                Ok(ev) => take(ev, &mut paths),
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
            lost = lost.into_iter().map(firmlinked).collect();
        }
        if !paths.is_empty() {
            let mut list: Vec<PathBuf> = paths.into_iter().collect();
            list.sort_by_key(|p| p.components().count());
            apply(&self.shared, &self.excludes, list, &self.on_dir());
            self.dirty = true;
            self.shared.changed();
        }
        // New folders may have used up the last inotify watches.
        self.publish_watch_state();
        if lost_all {
            Resync::Everything
        } else if lost.is_empty() {
            Resync::Nothing
        } else {
            lost.sort();
            lost.dedup();
            Resync::Folders(lost)
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
pub fn apply(shared: &Shared, ex: &Excludes, paths: Vec<PathBuf>, on_dir: OnDir) {
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
                    let kids = |p: &Path| if is_dir { scan::scan_dir(p, ex, &quiet, on_dir) } else { Vec::new() };
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
        apply(&shared, &ex, vec![dir.join("a/new"), dir.join("a/new/deep/f.bin"), dir.join("a/old.txt")], &|_| {});
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
