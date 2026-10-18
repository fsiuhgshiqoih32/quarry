//! Settings, stored as TOML in the per-user config directory.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::search::SortKey;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Folders to index. Empty means every local drive (Windows) or `/` (macOS, Linux).
    pub roots: Vec<String>,
    /// Folders to skip: a full path, or a folder name (wildcards allowed) skipped wherever it appears.
    pub excludes: Vec<String>,
    /// Show hidden files and dot-folders in results.
    pub show_hidden: bool,
    /// Follow file system events so the index stays current.
    pub live_updates: bool,
    /// Rescan once at start-up to pick up changes made while Quarry wasn't running.
    pub rescan_on_start: bool,
    /// Full rescan every this many minutes; 0 turns it off.
    pub rescan_minutes: u32,
    /// Global shortcut that shows the window, e.g. "Alt+Shift+Space". Empty turns it off.
    pub hotkey: String,
    /// Keep running in the tray when the window is closed.
    pub close_to_tray: bool,
    /// Serve the HTTP API on 127.0.0.1.
    pub http_enabled: bool,
    pub http_port: u16,
    pub theme: Theme,
    pub sort: SortKey,
    pub sort_desc: bool,
    /// Match full paths instead of names for every term.
    pub match_path: bool,
}

pub const DEFAULT_PORT: u16 = 7837;

impl Default for Config {
    fn default() -> Self {
        Config {
            roots: Vec::new(),
            excludes: Vec::new(),
            show_hidden: true,
            live_updates: true,
            rescan_on_start: true,
            // Linux watches only the home folder (inotify needs one watch per folder), so the rest of
            // the system is refreshed by rescanning.
            rescan_minutes: if cfg!(target_os = "linux") { 60 } else { 0 },
            hotkey: "Alt+Shift+Space".into(),
            close_to_tray: cfg!(feature = "tray"),
            http_enabled: false,
            http_port: DEFAULT_PORT,
            theme: Theme::System,
            sort: SortKey::Name,
            sort_desc: false,
            match_path: false,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Config {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).unwrap_or_else(|e| {
                eprintln!("quarry: ignoring invalid config {}: {e}", path.display());
                Config::default()
            }),
            Err(_) => Config::default(),
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let text = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, path).map_err(|e| e.to_string())
    }

    /// The folders that will actually be indexed.
    pub fn effective_roots(&self) -> Vec<PathBuf> {
        let roots: Vec<PathBuf> =
            self.roots.iter().map(|r| r.trim()).filter(|r| !r.is_empty()).map(PathBuf::from).collect();
        if roots.is_empty() { default_roots() } else { roots }
    }

    /// Everything that decides what a scan contains, so a saved index from different settings is
    /// thrown away instead of reused.
    pub fn fingerprint(&self) -> u64 {
        let mut h = Fnv::new();
        for r in self.effective_roots() {
            h.write(r.to_string_lossy().as_bytes());
            h.write(b"\0");
        }
        h.write(b"\x01");
        for e in &self.excludes {
            h.write(e.as_bytes());
            h.write(b"\0");
        }
        h.finish()
    }
}

/// 64-bit FNV-1a; stable across builds, unlike `DefaultHasher`.
pub struct Fnv(u64);

impl Fnv {
    pub fn new() -> Self {
        Fnv(0xcbf29ce484222325)
    }
    pub fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= b as u64;
            self.0 = self.0.wrapping_mul(0x100000001b3);
        }
    }
    pub fn finish(&self) -> u64 {
        self.0
    }
}

impl Default for Fnv {
    fn default() -> Self {
        Self::new()
    }
}

/// `QUARRY_HOME`, if set, holds the settings and the index together (handy for portable use).
fn home_override() -> Option<PathBuf> {
    std::env::var_os("QUARRY_HOME").filter(|v| !v.is_empty()).map(PathBuf::from)
}

pub fn config_dir() -> PathBuf {
    home_override().unwrap_or_else(|| dirs::config_dir().unwrap_or_else(std::env::temp_dir).join("quarry"))
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn index_path() -> PathBuf {
    match home_override() {
        Some(h) => h.join("index.bin"),
        None => dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("quarry").join("index.bin"),
    }
}

#[cfg(windows)]
pub fn default_roots() -> Vec<PathBuf> {
    use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives};
    const DRIVE_FIXED: u32 = 3;
    let mask = unsafe { GetLogicalDrives() };
    let mut roots = Vec::new();
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        let root = format!("{letter}:\\");
        let wide: Vec<u16> = root.encode_utf16().chain([0]).collect();
        if unsafe { GetDriveTypeW(wide.as_ptr()) } == DRIVE_FIXED {
            roots.push(PathBuf::from(root));
        }
    }
    if roots.is_empty() {
        roots.push(PathBuf::from("C:\\"));
    }
    roots
}

#[cfg(not(windows))]
pub fn default_roots() -> Vec<PathBuf> {
    vec![PathBuf::from("/")]
}

/// Folders skipped no matter what the user configured: virtual file systems and, on macOS, the
/// second view of the data volume that would list every file twice.
pub fn builtin_excludes() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    if cfg!(target_os = "macos") {
        for p in ["/System/Volumes", "/Volumes", "/dev", "/private/var/folders", "/private/var/vm", "/.fseventsd"] {
            out.push(p.into());
        }
    }
    if cfg!(target_os = "linux") {
        for p in ["/proc", "/sys", "/dev", "/run", "/var/lib/docker", "/var/lib/containers", "/snap"] {
            out.push(p.into());
        }
        out.extend(linux_skipped_mounts());
    }
    out
}

/// Mount points of pseudo, network and image file systems (snaps, WSL's Windows drives, sshfs...),
/// which are slow or meaningless to index.
#[cfg(target_os = "linux")]
fn linux_skipped_mounts() -> Vec<PathBuf> {
    const SKIP: &[&str] = &[
        "proc",
        "sysfs",
        "devtmpfs",
        "devpts",
        "cgroup",
        "cgroup2",
        "pstore",
        "bpf",
        "tracefs",
        "debugfs",
        "securityfs",
        "configfs",
        "fusectl",
        "mqueue",
        "hugetlbfs",
        "autofs",
        "binfmt_misc",
        "efivarfs",
        "nsfs",
        "squashfs",
        "overlay",
        "nfs",
        "nfs4",
        "cifs",
        "smb3",
        "smbfs",
        "9p",
        "drvfs",
        "ramfs",
        "rpc_pipefs",
    ];
    let Ok(text) = std::fs::read_to_string("/proc/self/mountinfo") else { return Vec::new() };
    let mut out = Vec::new();
    for line in text.lines() {
        let Some((left, right)) = line.split_once(" - ") else { continue };
        let Some(mount_point) = left.split(' ').nth(4) else { continue };
        let Some(fstype) = right.split(' ').next() else { continue };
        if SKIP.contains(&fstype) || (fstype.starts_with("fuse.") && fstype != "fuse.ntfs-3g") {
            out.push(PathBuf::from(unescape_mount(mount_point)));
        }
    }
    out
}

#[cfg(not(target_os = "linux"))]
fn linux_skipped_mounts() -> Vec<PathBuf> {
    Vec::new()
}

/// Mount points in mountinfo escape spaces and a few other bytes as `\ooo`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn unescape_mount(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)) {
            out.push((b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0'));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The folders live updates watch. Linux's inotify needs a watch per folder, so a whole-system root
/// is narrowed to the home folder there; periodic rescans cover the rest.
pub fn watch_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    if cfg!(target_os = "linux") {
        let home = dirs::home_dir();
        let mut out = Vec::new();
        for r in roots {
            if r == Path::new("/") {
                if let Some(h) = &home {
                    out.push(h.clone());
                }
            } else {
                out.push(r.clone());
            }
        }
        out.dedup();
        out
    } else {
        roots.to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mountinfo_escapes() {
        assert_eq!(unescape_mount("/media/my\\040disk"), "/media/my disk");
        assert_eq!(unescape_mount("/plain"), "/plain");
    }

    #[test]
    fn round_trip() {
        let c = Config { roots: vec!["/data".into()], http_enabled: true, ..Config::default() };
        let text = toml::to_string_pretty(&c).unwrap();
        assert_eq!(toml::from_str::<Config>(&text).unwrap(), c);
        assert_ne!(c.fingerprint(), Config::default().fingerprint());
    }
}
