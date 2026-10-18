//! Walks folders in parallel and builds an [`Index`].

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::SystemTime;

use rayon::prelude::*;

use crate::index::{CASE_INSENSITIVE_FS, DIR, HIDDEN, Index, Meta, SYMLINK};

/// Counters a running scan updates, and a flag to stop it early.
#[derive(Default)]
pub struct Progress {
    pub items: AtomicU64,
    pub unreadable: AtomicU64,
    pub cancel: AtomicBool,
}

impl Progress {
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

/// What to leave out of the index.
#[derive(Clone, Debug, Default)]
pub struct Excludes {
    paths: Vec<String>,
    names: Vec<String>,
}

impl Excludes {
    /// `user` entries containing a path separator are folders; others are folder names, wildcards
    /// allowed. Built-in folders that are themselves roots, or contain one, are kept.
    pub fn new(user: &[String], builtin: Vec<PathBuf>, roots: &[PathBuf]) -> Excludes {
        let mut ex = Excludes::default();
        let root_keys: Vec<String> = roots.iter().map(|r| key(r)).collect();
        for p in builtin {
            let k = key(&p);
            if root_keys.iter().any(|r| r == &k || is_under(r, &k)) {
                continue;
            }
            ex.paths.push(k);
        }
        for u in user {
            let u = u.trim();
            if u.is_empty() {
                continue;
            }
            if u.contains('/') || u.contains('\\') {
                ex.paths.push(key(Path::new(u)));
            } else {
                ex.names.push(if CASE_INSENSITIVE_FS { u.to_lowercase() } else { u.to_string() });
            }
        }
        ex.paths.sort();
        ex.paths.dedup();
        ex
    }

    /// True if a folder at `path` named `name` should not be entered.
    pub fn skip_dir(&self, path: &Path, name: &str) -> bool {
        if !self.names.is_empty() {
            let n = if CASE_INSENSITIVE_FS { name.to_lowercase() } else { name.to_string() };
            if self.names.iter().any(|pat| wildcard_match(pat.as_bytes(), n.as_bytes())) {
                return true;
            }
        }
        !self.paths.is_empty() && self.paths.binary_search(&key(path)).is_ok()
    }

    /// True if `path` or a folder above it is excluded.
    pub fn covers(&self, path: &Path) -> bool {
        let k = key(path);
        if self.paths.iter().any(|p| &k == p || is_under(&k, p)) {
            return true;
        }
        if self.names.is_empty() {
            return false;
        }
        path.ancestors().skip(1).any(|a| a.file_name().map(|n| self.skip_dir(a, &n.to_string_lossy())).unwrap_or(false))
    }
}

/// A path as a comparable string: no trailing separator, case-folded where the file system is.
fn key(p: &Path) -> String {
    let s = p.to_string_lossy();
    let trimmed = s.trim_end_matches(['/', '\\']);
    let s = if trimmed.is_empty() || trimmed.ends_with(':') { s.as_ref() } else { trimmed };
    if CASE_INSENSITIVE_FS { s.to_lowercase() } else { s.to_string() }
}

fn is_under(child: &str, parent: &str) -> bool {
    child.len() > parent.len()
        && child.starts_with(parent)
        && (parent.ends_with(['/', '\\']) || matches!(child.as_bytes()[parent.len()], b'/' | b'\\'))
}

/// `*` and `?` wildcards over bytes.
pub fn wildcard_match(pat: &[u8], s: &[u8]) -> bool {
    let (mut p, mut i) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while i < s.len() {
        if p < pat.len() && (pat[p] == b'?' || pat[p] == s[i]) {
            p += 1;
            i += 1;
        } else if p < pat.len() && pat[p] == b'*' {
            star = Some(p);
            p += 1;
            mark = i;
        } else if let Some(sp) = star {
            p = sp + 1;
            mark += 1;
            i = mark;
        } else {
            return false;
        }
    }
    pat[p..].iter().all(|&c| c == b'*')
}

/// A scanned file or folder before it goes into the index.
pub struct Node {
    pub name: String,
    pub meta: Meta,
    pub kids: Vec<Node>,
}

pub fn secs(t: std::io::Result<SystemTime>) -> i64 {
    match t {
        Ok(t) => match t.duration_since(SystemTime::UNIX_EPOCH) {
            Ok(d) => d.as_secs() as i64,
            Err(e) => -(e.duration().as_secs() as i64),
        },
        Err(_) => 0,
    }
}

/// Metadata for an entry, from `symlink_metadata`-style information (links are not followed).
pub fn meta_from(md: &fs::Metadata, name: &str) -> Meta {
    let ft = md.file_type();
    let mut flags = 0;
    if ft.is_symlink() {
        flags |= SYMLINK;
    } else if ft.is_dir() {
        flags |= DIR;
    }
    if is_hidden(md, name) {
        flags |= HIDDEN;
    }
    Meta { flags, size: if flags & DIR != 0 { 0 } else { md.len() }, mtime: secs(md.modified()) }
}

#[cfg(windows)]
fn is_hidden(md: &fs::Metadata, name: &str) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_HIDDEN: u32 = 2;
    md.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0 || name.starts_with('.')
}

#[cfg(target_os = "macos")]
fn is_hidden(md: &fs::Metadata, name: &str) -> bool {
    use std::os::macos::fs::MetadataExt;
    const UF_HIDDEN: u32 = 0x8000;
    name.starts_with('.') || md.st_flags() & UF_HIDDEN != 0
}

#[cfg(not(any(windows, target_os = "macos")))]
fn is_hidden(_md: &fs::Metadata, name: &str) -> bool {
    name.starts_with('.')
}

/// Lists one folder: name and metadata of each entry, links not followed.
#[cfg(not(windows))]
fn list(path: &Path) -> Option<Vec<(String, Meta)>> {
    let rd = fs::read_dir(path).ok()?;
    let mut out = Vec::new();
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().into_owned();
        let meta = match ent.metadata() {
            Ok(md) => meta_from(&md, &name),
            Err(_) => match ent.file_type() {
                Ok(ft) if ft.is_dir() => Meta { flags: DIR, ..Meta::default() },
                _ => Meta::default(),
            },
        };
        out.push((name, meta));
    }
    Some(out)
}

/// Lists one folder with `FindFirstFileExW` in large-fetch mode, which skips 8.3 names and returns
/// sizes and times with the listing.
#[cfg(windows)]
fn list(path: &Path) -> Option<Vec<(String, Meta)>> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{
        FIND_FIRST_EX_LARGE_FETCH, FindClose, FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW, FindNextFileW,
        WIN32_FIND_DATAW,
    };
    const DIRECTORY: u32 = 0x10;
    const HIDDEN_ATTR: u32 = 0x2;
    const REPARSE: u32 = 0x400;
    const NAME_SURROGATE: u32 = 0x2000_0000;

    const BACKSLASH: u16 = b'\\' as u16;
    let mut pattern: Vec<u16> = Vec::with_capacity(path.as_os_str().len() + 8);
    let raw: Vec<u16> = path.as_os_str().encode_wide().map(|c| if c == b'/' as u16 { BACKSLASH } else { c }).collect();
    // Past MAX_PATH the API needs the verbatim prefix.
    if raw.len() > 240 && !raw.starts_with(&[BACKSLASH, BACKSLASH]) {
        pattern.extend(r"\\?\".encode_utf16());
    }
    pattern.extend(&raw);
    if pattern.last() != Some(&BACKSLASH) {
        pattern.push(BACKSLASH);
    }
    pattern.extend([b'*' as u16, 0]);

    let mut data: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
    let handle = unsafe {
        FindFirstFileExW(
            pattern.as_ptr(),
            FindExInfoBasic,
            &mut data as *mut WIN32_FIND_DATAW as *mut _,
            FindExSearchNameMatch,
            std::ptr::null(),
            FIND_FIRST_EX_LARGE_FETCH,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut out = Vec::new();
    loop {
        let len = data.cFileName.iter().position(|&c| c == 0).unwrap_or(data.cFileName.len());
        let wide = &data.cFileName[..len];
        let dots = wide == [b'.' as u16] || wide == [b'.' as u16, b'.' as u16];
        if !dots {
            let name = String::from_utf16_lossy(wide);
            let attrs = data.dwFileAttributes;
            let link = attrs & REPARSE != 0 && data.dwReserved0 & NAME_SURROGATE != 0;
            let mut flags = 0;
            if link {
                flags |= SYMLINK;
            } else if attrs & DIRECTORY != 0 {
                flags |= DIR;
            }
            if attrs & HIDDEN_ATTR != 0 || name.starts_with('.') {
                flags |= HIDDEN;
            }
            let size = if flags & DIR != 0 { 0 } else { (data.nFileSizeHigh as u64) << 32 | data.nFileSizeLow as u64 };
            let ft = (data.ftLastWriteTime.dwHighDateTime as u64) << 32 | data.ftLastWriteTime.dwLowDateTime as u64;
            // FILETIME counts 100 ns ticks from 1601.
            let mtime = (ft / 10_000_000) as i64 - 11_644_473_600;
            out.push((name, Meta { flags, size, mtime }));
        }
        if unsafe { FindNextFileW(handle, &mut data) } == 0 {
            break;
        }
    }
    unsafe { FindClose(handle) };
    Some(out)
}

/// Reads one folder and, in parallel, everything below it.
pub fn scan_dir(path: &Path, ex: &Excludes, progress: &Progress) -> Vec<Node> {
    if progress.cancelled() {
        return Vec::new();
    }
    let Some(listing) = list(path) else {
        progress.unreadable.fetch_add(1, Ordering::Relaxed);
        return Vec::new();
    };
    let mut nodes = Vec::with_capacity(listing.len());
    let mut dirs: Vec<(PathBuf, usize)> = Vec::new();
    for (name, meta) in listing {
        if meta.flags & DIR != 0 {
            let p = path.join(&name);
            if ex.skip_dir(&p, &name) {
                continue;
            }
            dirs.push((p, nodes.len()));
        }
        nodes.push(Node { name, meta, kids: Vec::new() });
    }
    progress.items.fetch_add(nodes.len() as u64, Ordering::Relaxed);
    let subtrees: Vec<(usize, Vec<Node>)> =
        dirs.into_par_iter().map(|(p, at)| (at, scan_dir(&p, ex, progress))).collect();
    for (at, kids) in subtrees {
        nodes[at].kids = kids;
    }
    nodes
}

/// Drops roots that sit inside another root, so nothing is indexed twice.
pub fn dedup_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    let keys: Vec<String> = roots.iter().map(|r| key(r)).collect();
    let mut out: Vec<PathBuf> = Vec::new();
    let mut seen: Vec<&str> = Vec::new();
    for (i, r) in roots.iter().enumerate() {
        let k = keys[i].as_str();
        if seen.contains(&k) || keys.iter().enumerate().any(|(j, other)| j != i && is_under(k, other)) {
            continue;
        }
        seen.push(k);
        out.push(r.clone());
    }
    out
}

/// The name a root is stored under: its path, without a trailing separator unless it is a drive or `/`.
pub fn root_name(p: &Path) -> String {
    let s = p.to_string_lossy();
    let trimmed = s.trim_end_matches(['/', '\\']);
    if trimmed.is_empty() {
        "/".to_string()
    } else if trimmed.ends_with(':') {
        format!("{trimmed}\\")
    } else {
        trimmed.to_string()
    }
}

/// Scans every root and returns the finished index.
pub fn scan(roots: &[PathBuf], ex: &Excludes, progress: &Progress) -> Index {
    let roots = dedup_roots(roots);
    let trees: Vec<(String, Meta, Vec<Node>)> = roots
        .par_iter()
        .filter_map(|r| {
            let md = fs::metadata(r).ok()?;
            if !md.is_dir() {
                return None;
            }
            let name = root_name(r);
            let meta = Meta { flags: DIR, size: 0, mtime: secs(md.modified()) };
            Some((name, meta, scan_dir(r, ex, progress)))
        })
        .collect();
    let mut ix = Index::new();
    for (name, meta, kids) in trees {
        let id = ix.add_root(&name, meta);
        insert(&mut ix, id, kids);
    }
    ix.finish_bulk();
    ix
}

/// Adds scanned nodes below `parent`, depth first so parents keep smaller ids than their children.
pub fn insert(ix: &mut Index, parent: u32, nodes: Vec<Node>) {
    for n in nodes {
        let id = ix.add(parent, &n.name, n.meta);
        if !n.kids.is_empty() {
            insert(ix, id, n.kids);
        }
    }
}

/// Total size of a scanned subtree.
pub fn total_size(nodes: &[Node]) -> u64 {
    nodes.iter().map(|n| if n.meta.flags & DIR != 0 { total_size(&n.kids) } else { n.meta.size }).sum()
}

/// Sets folder sizes inside a subtree that was added with [`insert`] (live updates use this).
pub fn fix_sizes(ix: &mut Index, id: u32) -> u64 {
    if !ix.entry(id).is_dir() {
        return ix.entry(id).size;
    }
    let kids: Vec<u32> = ix.children(id).collect();
    let total = kids.into_iter().map(|k| fix_sizes(ix, k)).sum();
    ix.set_dir_size(id, total);
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcards() {
        assert!(wildcard_match(b"node_modules", b"node_modules"));
        assert!(wildcard_match(b"*.tmp", b"a.tmp"));
        assert!(wildcard_match(b"c?che*", b"cache-x"));
        assert!(!wildcard_match(b"*.tmp", b"a.txt"));
        assert!(wildcard_match(b"*", b""));
    }

    #[test]
    fn excludes_and_roots() {
        let roots = vec![PathBuf::from("/"), PathBuf::from("/home/u")];
        assert_eq!(dedup_roots(&roots), vec![PathBuf::from("/")]);
        let ex = Excludes::new(&["/data/skip".into(), "node_modules".into()], vec!["/proc".into()], &roots);
        assert!(ex.skip_dir(Path::new("/proc"), "proc"));
        assert!(ex.skip_dir(Path::new("/data/skip"), "skip"));
        assert!(ex.skip_dir(Path::new("/x/node_modules"), "node_modules"));
        assert!(!ex.skip_dir(Path::new("/data"), "data"));
        assert!(ex.covers(Path::new("/data/skip/a/b.txt")));
        assert!(ex.covers(Path::new("/x/node_modules/pkg/index.js")));
        assert!(!ex.covers(Path::new("/data/skipped.txt")));
        assert_eq!(root_name(Path::new("C:\\")), "C:\\");
        assert_eq!(root_name(Path::new("/home/u/")), "/home/u");
    }

    #[test]
    fn scans_a_tree() {
        let dir = std::env::temp_dir().join(format!("quarry-scan-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("a/b")).unwrap();
        fs::write(dir.join("a/one.txt"), b"12345").unwrap();
        fs::write(dir.join("a/b/two.txt"), b"123").unwrap();
        let ix = scan(std::slice::from_ref(&dir), &Excludes::default(), &Progress::default());
        assert_eq!(ix.live_count(), 5);
        assert_eq!(ix.entry(ix.roots()[0]).size, 8);
        let two = ix.lookup(&dir.join("a").join("b").join("two.txt")).unwrap();
        assert_eq!(ix.entry(two).size, 3);
        fs::remove_dir_all(&dir).unwrap();
    }
}
