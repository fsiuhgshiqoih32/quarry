//! The in-memory index: one compact record per file and folder, with every name in a single arena.
//!
//! Ids are positions in `entries`. A parent always has a smaller id than its children, both after a
//! scan and after live updates (new entries are appended under parents that already exist), so one
//! reverse pass is enough to total folder sizes. Removed entries become tombstones until the index
//! is rebuilt or saved, so ids handed out to a search stay valid while the index changes.

use std::cmp::Ordering;
use std::path::{Component, Path};

use rayon::prelude::*;

pub const NONE: u32 = u32::MAX;

pub const DIR: u16 = 1;
pub const HIDDEN: u16 = 2;
pub const SYMLINK: u16 = 4;
pub const DELETED: u16 = 8;

#[derive(Clone, Copy, Debug)]
pub struct Entry {
    name_off: u32,
    name_len: u16,
    pub flags: u16,
    pub parent: u32,
    first_child: u32,
    next_sibling: u32,
    /// Bytes. For folders, the total of everything below them.
    pub size: u64,
    /// Seconds since the Unix epoch, clamped to what fits in 32 bits (1970 to 2106).
    mtime: u32,
}

/// Times before 1970 or after 2106 are clamped; neither shows up on real file systems.
pub fn pack_time(t: i64) -> u32 {
    t.clamp(0, u32::MAX as i64) as u32
}

impl Entry {
    pub fn mtime(&self) -> i64 {
        self.mtime as i64
    }

    pub fn is_dir(&self) -> bool {
        self.flags & DIR != 0
    }
    pub fn is_hidden(&self) -> bool {
        self.flags & HIDDEN != 0
    }
    pub fn is_deleted(&self) -> bool {
        self.flags & DELETED != 0
    }
}

/// Metadata for a new entry.
#[derive(Clone, Copy, Debug, Default)]
pub struct Meta {
    pub flags: u16,
    pub size: u64,
    pub mtime: i64,
}

#[derive(Default)]
pub struct Index {
    entries: Vec<Entry>,
    /// Every name, concatenated in id order. Case-insensitive searches fold as they compare
    /// instead of keeping a lowercased copy, which would cost as much memory again as the names.
    names: String,
    roots: Vec<u32>,
    /// Live ids in name order. Searches walk this so results come out sorted by name.
    by_name: Vec<u32>,
    /// Position of each id in `by_name` (`NONE` for tombstones and not-yet-merged ids).
    rank: Vec<u32>,
    /// Ids added since `by_name` was last merged.
    pending: Vec<u32>,
    deleted: usize,
    /// Bumped on every change, so searches know to run again.
    pub revision: u64,
    /// Bumped whenever a whole new index replaces this one; ids from another epoch mean nothing here.
    pub epoch: u64,
}

pub fn fold(s: &str) -> Vec<u8> {
    s.as_bytes().to_ascii_lowercase()
}

/// File names compare case-insensitively on Windows and macOS (the default for APFS and NTFS).
pub const CASE_INSENSITIVE_FS: bool = cfg!(any(windows, target_os = "macos"));

pub const SEP: char = std::path::MAIN_SEPARATOR;

impl Index {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.live_count() == 0
    }

    /// Files and folders currently indexed.
    pub fn live_count(&self) -> usize {
        self.entries.len() - self.deleted
    }

    pub fn deleted_count(&self) -> usize {
        self.deleted
    }

    pub fn entry(&self, id: u32) -> &Entry {
        &self.entries[id as usize]
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn roots(&self) -> &[u32] {
        &self.roots
    }

    pub fn by_name(&self) -> &[u32] {
        &self.by_name
    }

    /// Where an id sorts by name. Sorting ids by this puts them in name order.
    pub fn rank(&self, id: u32) -> u32 {
        self.rank.get(id as usize).copied().unwrap_or(NONE)
    }

    /// Every name, concatenated in id order; see [`Index::id_at`].
    pub fn names_bytes(&self) -> &[u8] {
        self.names.as_bytes()
    }

    /// The entry whose name contains the arena bytes `off..off + len`, if one does.
    pub fn id_at(&self, off: usize, len: usize) -> Option<u32> {
        let i = self.entries.partition_point(|e| e.name_off as usize <= off).checked_sub(1)?;
        let e = &self.entries[i];
        (off + len <= e.name_off as usize + e.name_len as usize).then_some(i as u32)
    }

    /// The arena offset just past an entry's name.
    pub fn name_end(&self, id: u32) -> usize {
        let e = &self.entries[id as usize];
        e.name_off as usize + e.name_len as usize
    }

    fn rebuild_rank(&mut self) {
        self.rank.clear();
        self.rank.reserve_exact(self.entries.len());
        self.rank.resize(self.entries.len(), NONE);
        for (pos, &id) in self.by_name.iter().enumerate() {
            self.rank[id as usize] = pos as u32;
        }
    }

    pub fn name(&self, id: u32) -> &str {
        let e = &self.entries[id as usize];
        let start = e.name_off as usize;
        &self.names[start..start + e.name_len as usize]
    }

    /// Adds a root folder: a top-level entry whose name is its full path.
    pub fn add_root(&mut self, path: &str, meta: Meta) -> u32 {
        let id = self.push(path, NONE, Meta { flags: meta.flags | DIR, ..meta });
        self.roots.push(id);
        id
    }

    /// Adds a child of `parent`. Hidden folders hide everything below them.
    pub fn add(&mut self, parent: u32, name: &str, meta: Meta) -> u32 {
        let inherited = self.entries[parent as usize].flags & HIDDEN;
        let id = self.push(name, parent, Meta { flags: meta.flags | inherited, ..meta });
        let first = self.entries[parent as usize].first_child;
        self.entries[id as usize].next_sibling = first;
        self.entries[parent as usize].first_child = id;
        id
    }

    fn push(&mut self, name: &str, parent: u32, meta: Meta) -> u32 {
        // Names longer than u16::MAX bytes don't exist on any supported file system; clip defensively.
        let mut end = name.len().min(u16::MAX as usize);
        while !name.is_char_boundary(end) {
            end -= 1;
        }
        let name = &name[..end];
        let id = u32::try_from(self.entries.len()).expect("index full");
        let name_off = u32::try_from(self.names.len()).expect("name arena full");
        // Past a million entries, grow by an eighth instead of doubling: a refresh that adds a
        // few files to a loaded index would otherwise double its memory.
        if self.entries.len() == self.entries.capacity() && self.entries.len() >= 1 << 20 {
            self.entries.reserve_exact(self.entries.len() / 8);
        }
        if self.names.len() + name.len() > self.names.capacity() && self.names.len() >= 1 << 24 {
            self.names.reserve_exact(self.names.len() / 8 + name.len());
        }
        self.names.push_str(name);
        self.entries.push(Entry {
            name_off,
            name_len: name.len() as u16,
            flags: meta.flags & !DELETED,
            parent,
            first_child: NONE,
            next_sibling: NONE,
            size: meta.size,
            mtime: pack_time(meta.mtime),
        });
        self.pending.push(id);
        self.revision += 1;
        id
    }

    pub fn children(&self, id: u32) -> Children<'_> {
        Children { index: self, next: self.entries[id as usize].first_child }
    }

    /// The full path of an entry.
    pub fn path(&self, id: u32) -> String {
        let mut s = String::new();
        self.path_into(id, &mut s);
        s
    }

    pub fn path_into(&self, id: u32, out: &mut String) {
        out.clear();
        // Paths deeper than this are rare enough to pay for an allocation.
        let mut chain = [0u32; 96];
        let mut n = 0;
        let mut cur = id;
        while cur != NONE && n < chain.len() {
            chain[n] = cur;
            n += 1;
            cur = self.entries[cur as usize].parent;
        }
        if cur != NONE {
            let mut all: Vec<u32> = chain.to_vec();
            while cur != NONE {
                all.push(cur);
                cur = self.entries[cur as usize].parent;
            }
            self.join(all.iter().rev(), out);
        } else {
            self.join(chain[..n].iter().rev(), out);
        }
    }

    fn join<'a>(&self, ids: impl Iterator<Item = &'a u32>, out: &mut String) {
        for &part in ids {
            if !out.is_empty() && !out.ends_with(['/', '\\']) {
                out.push(SEP);
            }
            out.push_str(self.name(part));
        }
    }

    /// The path of an entry's folder; empty for roots.
    pub fn parent_path(&self, id: u32) -> String {
        match self.entries[id as usize].parent {
            NONE => String::new(),
            p => self.path(p),
        }
    }

    pub fn find_child(&self, parent: u32, name: &str) -> Option<u32> {
        let mut loose = None;
        for c in self.children(parent) {
            let n = self.name(c);
            if n == name {
                return Some(c);
            }
            if CASE_INSENSITIVE_FS && loose.is_none() && n.eq_ignore_ascii_case(name) {
                loose = Some(c);
            }
        }
        loose
    }

    /// The root that contains `path`, with the rest of the path after it.
    pub fn root_for<'p>(&self, path: &'p Path) -> Option<(u32, &'p Path)> {
        let mut best: Option<(u32, &Path, usize)> = None;
        for &r in &self.roots {
            if self.entries[r as usize].is_deleted() {
                continue;
            }
            let root = Path::new(self.name(r));
            if let Some(rest) = strip_prefix(path, root) {
                let len = self.name(r).len();
                if best.is_none_or(|b| len > b.2) {
                    best = Some((r, rest, len));
                }
            }
        }
        best.map(|(r, rest, _)| (r, rest))
    }

    /// Finds the entry for a full path.
    pub fn lookup(&self, path: &Path) -> Option<u32> {
        let (mut cur, rest) = self.root_for(path)?;
        for comp in rest.components() {
            match comp {
                Component::Normal(name) => cur = self.find_child(cur, &name.to_string_lossy())?,
                Component::CurDir => {}
                _ => return None,
            }
        }
        Some(cur)
    }

    /// Removes an entry and everything below it.
    pub fn remove(&mut self, id: u32) {
        let e = self.entries[id as usize];
        if e.is_deleted() {
            return;
        }
        if e.parent == NONE {
            self.roots.retain(|&r| r != id);
        } else {
            self.unlink(e.parent, id);
            self.add_size(e.parent, -(e.size as i64));
        }
        let mut stack = vec![id];
        while let Some(cur) = stack.pop() {
            let ent = &mut self.entries[cur as usize];
            if ent.flags & DELETED == 0 {
                ent.flags |= DELETED;
                self.deleted += 1;
            }
            let mut c = ent.first_child;
            while c != NONE {
                stack.push(c);
                c = self.entries[c as usize].next_sibling;
            }
        }
        self.revision += 1;
    }

    /// Removes several children of one folder and everything below them. One pass over the
    /// folder's child list instead of one per entry, which matters for folders with many files.
    pub fn remove_many(&mut self, parent: u32, ids: &[u32]) {
        let mut freed = 0u64;
        let mut any = false;
        for &id in ids {
            let e = self.entries[id as usize];
            if e.is_deleted() || e.parent != parent {
                continue;
            }
            freed += e.size;
            any = true;
            let mut stack = vec![id];
            while let Some(cur) = stack.pop() {
                let ent = &mut self.entries[cur as usize];
                if ent.flags & DELETED == 0 {
                    ent.flags |= DELETED;
                    self.deleted += 1;
                }
                let mut c = ent.first_child;
                while c != NONE {
                    stack.push(c);
                    c = self.entries[c as usize].next_sibling;
                }
            }
        }
        if !any {
            return;
        }
        // Relink the survivors.
        let (mut first, mut last) = (NONE, NONE);
        let mut c = self.entries[parent as usize].first_child;
        while c != NONE {
            let next = self.entries[c as usize].next_sibling;
            if !self.entries[c as usize].is_deleted() {
                if last == NONE {
                    first = c;
                } else {
                    self.entries[last as usize].next_sibling = c;
                }
                last = c;
            }
            c = next;
        }
        if last != NONE {
            self.entries[last as usize].next_sibling = NONE;
        }
        self.entries[parent as usize].first_child = first;
        self.add_size(parent, -(freed as i64));
        self.revision += 1;
    }

    fn unlink(&mut self, parent: u32, id: u32) {
        let next = self.entries[id as usize].next_sibling;
        if self.entries[parent as usize].first_child == id {
            self.entries[parent as usize].first_child = next;
            return;
        }
        let mut c = self.entries[parent as usize].first_child;
        while c != NONE {
            let after = self.entries[c as usize].next_sibling;
            if after == id {
                self.entries[c as usize].next_sibling = next;
                return;
            }
            c = after;
        }
    }

    /// Updates a file's size and time, keeping folder totals right.
    pub fn update(&mut self, id: u32, size: u64, mtime: i64) {
        let e = &mut self.entries[id as usize];
        e.mtime = pack_time(mtime);
        if !e.is_dir() && e.size != size {
            let delta = size as i64 - e.size as i64;
            e.size = size;
            let parent = e.parent;
            if parent != NONE {
                self.add_size(parent, delta);
            }
        }
        self.revision += 1;
    }

    /// Adds `delta` bytes to `id` and every folder above it.
    pub fn add_size(&mut self, mut id: u32, delta: i64) {
        while id != NONE {
            let e = &mut self.entries[id as usize];
            e.size = e.size.saturating_add_signed(delta);
            id = e.parent;
        }
    }

    pub(crate) fn set_dir_size(&mut self, id: u32, size: u64) {
        self.entries[id as usize].size = size;
    }

    /// Call after adding entries without going through [`Index::add_size`]: totals folder sizes and
    /// sorts names. Used once a scan has been loaded in bulk.
    pub fn finish_bulk(&mut self) {
        for e in self.entries.iter_mut() {
            if e.is_dir() {
                e.size = 0;
            }
        }
        for i in (0..self.entries.len()).rev() {
            let e = self.entries[i];
            if e.is_deleted() || e.parent == NONE {
                continue;
            }
            self.entries[e.parent as usize].size += e.size;
        }
        self.pending.clear();
        let mut ids: Vec<u32> =
            (0..self.entries.len() as u32).filter(|&i| !self.entries[i as usize].is_deleted()).collect();
        ids.par_sort_unstable_by(|&a, &b| self.cmp_names(a, b));
        self.by_name = ids;
        self.rebuild_rank();
        self.revision += 1;
    }

    /// Folds entries added since the last call into the name order and drops tombstones from it.
    pub fn merge_pending(&mut self) {
        if self.pending.is_empty() && self.by_name.len() == self.live_count() {
            return;
        }
        let mut fresh = std::mem::take(&mut self.pending);
        fresh.retain(|&i| !self.entries[i as usize].is_deleted());
        fresh.sort_unstable_by(|&a, &b| self.cmp_names(a, b));
        let old = std::mem::take(&mut self.by_name);
        let mut merged = Vec::with_capacity(old.len() + fresh.len());
        let (mut i, mut j) = (0, 0);
        while i < old.len() || j < fresh.len() {
            let take_old = if i == old.len() {
                false
            } else if j == fresh.len() {
                true
            } else {
                self.cmp_names(old[i], fresh[j]) != Ordering::Greater
            };
            let id = if take_old {
                i += 1;
                old[i - 1]
            } else {
                j += 1;
                fresh[j - 1]
            };
            if !self.entries[id as usize].is_deleted() {
                merged.push(id);
            }
        }
        self.by_name = merged;
        self.rebuild_rank();
    }

    fn cmp_names(&self, a: u32, b: u32) -> Ordering {
        let (x, y) = (self.name(a).as_bytes(), self.name(b).as_bytes());
        cmp_folded(x, y).then_with(|| x.cmp(y)).then(a.cmp(&b))
    }

    /// Raw parts for saving. `parent` ids are remapped by the caller.
    pub(crate) fn raw_parts(&self) -> (&[Entry], &[u32]) {
        (&self.entries, &self.by_name)
    }
}

/// Compares two names with ASCII letters folded to lowercase.
pub fn cmp_folded(x: &[u8], y: &[u8]) -> Ordering {
    for (a, b) in x.iter().zip(y) {
        let (a, b) = (a.to_ascii_lowercase(), b.to_ascii_lowercase());
        if a != b {
            return a.cmp(&b);
        }
    }
    x.len().cmp(&y.len())
}

/// Rebuilds a saved index one record at a time, so loading needs no second copy of the data.
pub(crate) struct Loader {
    entries: Vec<Entry>,
    names_len: usize,
}

impl Loader {
    pub(crate) fn with_capacity(count: usize) -> Loader {
        Loader { entries: Vec::with_capacity(count), names_len: 0 }
    }

    /// Adds the next record: name length, flags, parent id, size and modification time.
    pub(crate) fn push(&mut self, len: u16, flags: u16, parent: u32, size: u64, mtime: i64) -> Result<(), String> {
        let i = self.entries.len();
        if parent != NONE && parent as usize >= i {
            return Err("corrupt parent link".into());
        }
        self.entries.push(Entry {
            name_off: u32::try_from(self.names_len).map_err(|_| "name table too large")?,
            name_len: len,
            flags: flags & !DELETED,
            parent,
            first_child: NONE,
            next_sibling: NONE,
            size,
            mtime: pack_time(mtime),
        });
        self.names_len += len as usize;
        Ok(())
    }

    /// Finishes with the concatenated names and the saved name order.
    pub(crate) fn finish(self, names: String, by_name: Vec<u32>) -> Result<Index, String> {
        let n = self.entries.len();
        if self.names_len != names.len() {
            return Err("corrupt name table".into());
        }
        let mut ix = Index { entries: self.entries, names, ..Default::default() };
        for e in &ix.entries {
            let (a, b) = (e.name_off as usize, e.name_off as usize + e.name_len as usize);
            if !ix.names.is_char_boundary(a) || !ix.names.is_char_boundary(b) {
                return Err("corrupt name table".into());
            }
        }
        for i in (0..n).rev() {
            let p = ix.entries[i].parent;
            if p == NONE {
                ix.roots.push(i as u32);
            } else {
                ix.entries[i].next_sibling = ix.entries[p as usize].first_child;
                ix.entries[p as usize].first_child = i as u32;
            }
        }
        ix.roots.reverse();
        if by_name.len() != n || by_name.iter().any(|&i| i as usize >= n) {
            return Err("corrupt name order".into());
        }
        ix.by_name = by_name;
        ix.rebuild_rank();
        if ix.rank.contains(&NONE) {
            return Err("corrupt name order".into());
        }
        ix.revision = 1;
        Ok(ix)
    }
}

pub struct Children<'a> {
    index: &'a Index,
    next: u32,
}

impl Iterator for Children<'_> {
    type Item = u32;
    fn next(&mut self) -> Option<u32> {
        if self.next == NONE {
            return None;
        }
        let id = self.next;
        self.next = self.index.entries[id as usize].next_sibling;
        Some(id)
    }
}

/// `Path::strip_prefix`, but case-insensitive where the file system is.
pub fn strip_prefix<'p>(path: &'p Path, prefix: &Path) -> Option<&'p Path> {
    if let Ok(rest) = path.strip_prefix(prefix) {
        return Some(rest);
    }
    if !CASE_INSENSITIVE_FS {
        return None;
    }
    let mut rest = path.components();
    for want in prefix.components() {
        let got = rest.next()?;
        let same = match (want, got) {
            (Component::Normal(a), Component::Normal(b)) => {
                a.to_string_lossy().eq_ignore_ascii_case(&b.to_string_lossy())
            }
            (Component::Prefix(a), Component::Prefix(b)) => {
                a.as_os_str().to_string_lossy().eq_ignore_ascii_case(&b.as_os_str().to_string_lossy())
            }
            (a, b) => a == b,
        };
        if !same {
            return None;
        }
    }
    Some(rest.as_path())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Index {
        let mut ix = Index::new();
        let root = ix.add_root(if cfg!(windows) { "C:\\" } else { "/" }, Meta::default());
        let docs = ix.add(root, "Docs", Meta { flags: DIR, ..Meta::default() });
        ix.add(docs, "report.pdf", Meta { size: 100, ..Meta::default() });
        ix.add(docs, "Notes.txt", Meta { size: 20, ..Meta::default() });
        let hidden = ix.add(root, ".cache", Meta { flags: DIR | HIDDEN, ..Meta::default() });
        ix.add(hidden, "blob", Meta { size: 5, ..Meta::default() });
        ix.finish_bulk();
        ix
    }

    #[test]
    fn paths_and_lookup() {
        let ix = sample();
        let sep = SEP;
        let root = if cfg!(windows) { "C:\\" } else { "/" };
        let id = ix.lookup(Path::new(&format!("{root}Docs{sep}report.pdf"))).unwrap();
        assert_eq!(ix.name(id), "report.pdf");
        assert_eq!(ix.path(id), format!("{root}Docs{sep}report.pdf"));
        assert_eq!(ix.parent_path(id), format!("{root}Docs"));
        assert!(ix.lookup(Path::new(&format!("{root}Docs{sep}missing"))).is_none());
    }

    #[test]
    fn sizes_hidden_and_order() {
        let ix = sample();
        assert_eq!(ix.entry(ix.roots()[0]).size, 125);
        let blob = ix.by_name().iter().copied().find(|&i| ix.name(i) == "blob").unwrap();
        assert!(ix.entry(blob).is_hidden());
        let names: Vec<&str> = ix.by_name().iter().map(|&i| ix.name(i)).collect();
        let mut sorted = names.clone();
        sorted.sort_by_key(|n| n.to_ascii_lowercase());
        assert_eq!(names, sorted);
    }

    #[test]
    fn remove_and_merge() {
        let mut ix = sample();
        let root = ix.roots()[0];
        let docs = ix.find_child(root, "Docs").unwrap();
        ix.add(docs, "a.txt", Meta { size: 7, ..Meta::default() });
        ix.add_size(docs, 7);
        ix.remove(ix.find_child(docs, "report.pdf").unwrap());
        ix.merge_pending();
        assert_eq!(ix.entry(root).size, 32);
        let names: Vec<&str> = ix.by_name().iter().map(|&i| ix.name(i)).collect();
        assert!(names.contains(&"a.txt"));
        assert!(!names.contains(&"report.pdf"));
        assert_eq!(ix.by_name().len(), ix.live_count());
    }
}
