//! File system change notifications, reduced to "something changed at this path".
//!
//! - Windows: one `ReadDirectoryChangesW` per root with a 512 KiB buffer. If the system's buffer
//!   overflows anyway, the changes are lost and the root is refreshed.
//! - macOS: FSEvents through `notify`; it costs nothing per folder.
//! - Linux: inotify, one watch per folder, added as folders are listed. If the kernel's watch limit
//!   runs out, the folders without a watch are kept current by periodic refreshes.
//!
//! Dropping a [`Watcher`] stops it.

use std::path::{Path, PathBuf};

use crossbeam_channel::Sender;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// Something at this path was created, removed, renamed or modified.
    Path(PathBuf),
    /// Events were lost below this path (`None`: below every root).
    Rescan(Option<PathBuf>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    On,
    Partly(String),
    Failed(String),
}

pub use imp::Watcher;

#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use std::collections::HashMap;
    use std::os::fd::AsRawFd;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use inotify::{EventMask, Inotify, WatchDescriptor, WatchMask, Watches};
    use parking_lot::Mutex;

    pub struct Watcher {
        watches: Option<Mutex<Watches>>,
        map: Arc<Mutex<HashMap<WatchDescriptor, PathBuf>>>,
        limit_hit: AtomicBool,
        count: AtomicUsize,
        alive: Arc<AtomicBool>,
        error: Option<String>,
    }

    fn mask() -> WatchMask {
        WatchMask::CREATE
            | WatchMask::DELETE
            | WatchMask::MOVED_FROM
            | WatchMask::MOVED_TO
            | WatchMask::CLOSE_WRITE
            | WatchMask::ATTRIB
            | WatchMask::DELETE_SELF
            | WatchMask::MOVE_SELF
            | WatchMask::DONT_FOLLOW
            | WatchMask::ONLYDIR
            | WatchMask::EXCL_UNLINK
    }

    impl Watcher {
        pub fn new(tx: Sender<Change>) -> Watcher {
            let map: Arc<Mutex<HashMap<WatchDescriptor, PathBuf>>> = Arc::default();
            let alive = Arc::new(AtomicBool::new(true));
            let mut inotify = match Inotify::init() {
                Ok(i) => i,
                Err(e) => {
                    return Watcher {
                        watches: None,
                        map,
                        limit_hit: AtomicBool::new(false),
                        count: AtomicUsize::new(0),
                        alive,
                        error: Some(format!("inotify is unavailable: {e}")),
                    };
                }
            };
            let watches = inotify.watches();
            let (m, a) = (map.clone(), alive.clone());
            let reader = std::thread::Builder::new().name("inotify".into()).spawn(move || {
                let fd = inotify.as_raw_fd();
                let mut buf = vec![0u8; 256 * 1024];
                // Wakes twice a second to notice when the watcher was dropped.
                while a.load(Ordering::Relaxed) {
                    let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
                    if unsafe { libc::poll(&mut pfd, 1, 500) } <= 0 {
                        continue;
                    }
                    let events = match inotify.read_events(&mut buf) {
                        Ok(events) => events,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    };
                    for ev in events {
                        if ev.mask.contains(EventMask::Q_OVERFLOW) {
                            let _ = tx.send(Change::Rescan(None));
                            continue;
                        }
                        if ev.mask.contains(EventMask::IGNORED) {
                            m.lock().remove(&ev.wd);
                            continue;
                        }
                        let Some(dir) = m.lock().get(&ev.wd).cloned() else { continue };
                        let path = match ev.name {
                            Some(name) => dir.join(name),
                            None => dir,
                        };
                        if tx.send(Change::Path(path)).is_err() {
                            return;
                        }
                    }
                }
            });
            let error = reader.err().map(|e| format!("could not start the inotify reader: {e}"));
            Watcher {
                watches: error.is_none().then(|| Mutex::new(watches)),
                map,
                limit_hit: AtomicBool::new(false),
                count: AtomicUsize::new(0),
                alive,
                error,
            }
        }

        /// Linux watches folders one by one instead; see [`Watcher::watch_dir`].
        pub fn watch_root(&self, _root: &Path) {}

        /// True if folders need [`Watcher::watch_dir`] calls.
        pub fn wants_dirs(&self) -> bool {
            self.watches.is_some()
        }

        pub fn watch_dir(&self, dir: &Path) {
            let Some(watches) = &self.watches else { return };
            if self.limit_hit.load(Ordering::Relaxed) {
                return;
            }
            match watches.lock().add(dir, mask()) {
                Ok(wd) => {
                    if self.map.lock().insert(wd, dir.to_path_buf()).is_none() {
                        self.count.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Err(e) if e.raw_os_error() == Some(libc::ENOSPC) => self.limit_hit.store(true, Ordering::Relaxed),
                Err(_) => {}
            }
        }

        pub fn state(&self) -> State {
            if let Some(e) = &self.error {
                State::Failed(e.clone())
            } else if self.limit_hit.load(Ordering::Relaxed) {
                State::Partly(format!(
                    "the inotify limit was reached after {} folders; the rest is refreshed hourly. Raise \
                     fs.inotify.max_user_watches to watch everything",
                    self.count.load(Ordering::Relaxed)
                ))
            } else {
                State::On
            }
        }
    }

    impl Drop for Watcher {
        fn drop(&mut self) {
            // The reader notices within half a second and closes the inotify descriptor.
            self.alive.store(false, Ordering::Relaxed);
        }
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use std::collections::HashSet;

    use notify::Watcher as _;
    use parking_lot::Mutex;

    pub struct Watcher {
        inner: Mutex<Option<notify::RecommendedWatcher>>,
        watched: Mutex<HashSet<PathBuf>>,
        failed: Mutex<Vec<String>>,
        error: Option<String>,
    }

    impl Watcher {
        pub fn new(tx: Sender<Change>) -> Watcher {
            let inner = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                let Ok(ev) = res else { return };
                if ev.need_rescan() {
                    if ev.paths.is_empty() {
                        let _ = tx.send(Change::Rescan(None));
                    }
                    for p in ev.paths {
                        let _ = tx.send(Change::Rescan(Some(p)));
                    }
                    return;
                }
                if matches!(ev.kind, notify::EventKind::Access(_)) {
                    return;
                }
                for p in ev.paths {
                    let _ = tx.send(Change::Path(p));
                }
            });
            let (inner, error) = match inner {
                Ok(w) => (Some(w), None),
                Err(e) => (None, Some(format!("FSEvents is unavailable: {e}"))),
            };
            Watcher { inner: Mutex::new(inner), watched: Mutex::default(), failed: Mutex::default(), error }
        }

        pub fn watch_root(&self, root: &Path) {
            if !self.watched.lock().insert(root.to_path_buf()) {
                return;
            }
            if let Some(w) = self.inner.lock().as_mut()
                && let Err(e) = w.watch(root, notify::RecursiveMode::Recursive)
            {
                self.failed.lock().push(format!("{}: {e}", root.display()));
            }
        }

        pub fn wants_dirs(&self) -> bool {
            false
        }

        pub fn watch_dir(&self, _dir: &Path) {}

        pub fn state(&self) -> State {
            if let Some(e) = &self.error {
                return State::Failed(e.clone());
            }
            let failed = self.failed.lock();
            if failed.is_empty() {
                State::On
            } else if failed.len() >= self.watched.lock().len() {
                State::Failed(failed.join("; "))
            } else {
                State::Partly(failed.join("; "))
            }
        }
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::collections::HashSet;
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use parking_lot::Mutex;
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_DIR_NAME,
        FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE, FILE_NOTIFY_INFORMATION,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, ReadDirectoryChangesW,
    };
    use windows_sys::Win32::System::IO::CancelIoEx;

    const ERROR_INVALID_PARAMETER: u32 = 87;

    /// What the watcher threads share with the [`Watcher`].
    #[derive(Default)]
    struct Common {
        stop: AtomicBool,
        /// Open directory handles, so dropping the watcher can cancel their pending reads.
        handles: Mutex<Vec<usize>>,
        failed: Mutex<Vec<String>>,
    }

    pub struct Watcher {
        tx: Sender<Change>,
        watched: Mutex<HashSet<PathBuf>>,
        common: Arc<Common>,
    }

    impl Watcher {
        pub fn new(tx: Sender<Change>) -> Watcher {
            Watcher { tx, watched: Mutex::default(), common: Arc::default() }
        }

        pub fn watch_root(&self, root: &Path) {
            if !self.watched.lock().insert(root.to_path_buf()) {
                return;
            }
            let (root, tx, common) = (root.to_path_buf(), self.tx.clone(), self.common.clone());
            let _ = std::thread::Builder::new().name("watch".into()).spawn(move || run(&root, &tx, &common));
        }

        pub fn wants_dirs(&self) -> bool {
            false
        }

        pub fn watch_dir(&self, _dir: &Path) {}

        pub fn state(&self) -> State {
            let failed = self.common.failed.lock();
            if failed.is_empty() {
                State::On
            } else if failed.len() >= self.watched.lock().len() {
                State::Failed(failed.join("; "))
            } else {
                State::Partly(failed.join("; "))
            }
        }
    }

    impl Drop for Watcher {
        fn drop(&mut self) {
            self.common.stop.store(true, Ordering::Relaxed);
            for &h in self.common.handles.lock().iter() {
                unsafe { CancelIoEx(h as HANDLE, std::ptr::null()) };
            }
        }
    }

    /// Watches one root until stopped or until the handle fails.
    fn run(root: &Path, tx: &Sender<Change>, common: &Common) {
        let wide: Vec<u16> = root.as_os_str().encode_wide().chain([0]).collect();
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_LIST_DIRECTORY,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            let err = std::io::Error::last_os_error();
            common.failed.lock().push(format!("{}: {err}", root.display()));
            return;
        }
        common.handles.lock().push(handle as usize);
        // u32 elements keep the buffer DWORD-aligned, as the API requires.
        let mut buf = vec![0u32; 512 * 1024 / 4];
        let filter = FILE_NOTIFY_CHANGE_FILE_NAME
            | FILE_NOTIFY_CHANGE_DIR_NAME
            | FILE_NOTIFY_CHANGE_SIZE
            | FILE_NOTIFY_CHANGE_LAST_WRITE;
        while !common.stop.load(Ordering::Relaxed) {
            let mut bytes = 0u32;
            let ok = unsafe {
                ReadDirectoryChangesW(
                    handle,
                    buf.as_mut_ptr().cast(),
                    (buf.len() * 4) as u32,
                    1,
                    filter,
                    &mut bytes,
                    std::ptr::null_mut(),
                    None,
                )
            };
            if ok == 0 {
                if unsafe { GetLastError() } == ERROR_INVALID_PARAMETER && buf.len() > 16 * 1024 {
                    // Network drives allow at most 64 KiB.
                    buf.truncate(64 * 1024 / 4);
                    continue;
                }
                if !common.stop.load(Ordering::Relaxed) {
                    let err = std::io::Error::last_os_error();
                    common.failed.lock().push(format!("{}: {err}", root.display()));
                }
                break;
            }
            if bytes == 0 {
                // The system's buffer overflowed and the changes are lost.
                if tx.send(Change::Rescan(Some(root.to_path_buf()))).is_err() {
                    break;
                }
                continue;
            }
            let base = buf.as_ptr() as *const u8;
            let mut offset = 0usize;
            while offset + std::mem::size_of::<FILE_NOTIFY_INFORMATION>() <= bytes as usize {
                let info = unsafe { std::ptr::read_unaligned(base.add(offset) as *const FILE_NOTIFY_INFORMATION) };
                let name_at = offset + std::mem::offset_of!(FILE_NOTIFY_INFORMATION, FileName);
                let len = info.FileNameLength as usize / 2;
                let mut name = vec![0u16; len];
                unsafe { std::ptr::copy_nonoverlapping(base.add(name_at), name.as_mut_ptr() as *mut u8, len * 2) };
                if tx.send(Change::Path(root.join(OsString::from_wide(&name)))).is_err() {
                    common.stop.store(true, Ordering::Relaxed);
                    break;
                }
                if info.NextEntryOffset == 0 {
                    break;
                }
                offset += info.NextEntryOffset as usize;
            }
        }
        let mut handles = common.handles.lock();
        handles.retain(|&h| h != handle as usize);
        unsafe { CloseHandle(handle) };
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod imp {
    use super::*;

    pub struct Watcher;

    impl Watcher {
        pub fn new(_tx: Sender<Change>) -> Watcher {
            Watcher
        }
        pub fn watch_root(&self, _root: &Path) {}
        pub fn wants_dirs(&self) -> bool {
            false
        }
        pub fn watch_dir(&self, _dir: &Path) {}
        pub fn state(&self) -> State {
            State::Failed("live updates are not supported on this system".into())
        }
    }
}
