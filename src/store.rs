//! Saving the index between runs, so results are there the moment the window opens.
//!
//! Layout (little-endian): magic, version, settings fingerprint, entry count, name bytes, then one
//! record per entry (name length, flags, parent, size, mtime), the names, and the name order.
//! Tombstones are dropped on the way out.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use crate::index::{DELETED, Index, NONE};

const MAGIC: &[u8; 8] = b"QUARRYIX";
const VERSION: u32 = 1;
const RECORD: usize = 2 + 2 + 4 + 8 + 8;

pub fn save(ix: &Index, path: &Path, fingerprint: u64) -> Result<(), String> {
    let (entries, by_name) = ix.raw_parts();
    let mut remap = vec![NONE; entries.len()];
    let mut live = 0u32;
    let mut names_len = 0u64;
    for (i, e) in entries.iter().enumerate() {
        if e.flags & DELETED == 0 {
            remap[i] = live;
            live += 1;
            names_len += ix.name(i as u32).len() as u64;
        }
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("bin.tmp");
    let file = File::create(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    let mut w = BufWriter::with_capacity(1 << 20, file);
    let io = |e: std::io::Error| e.to_string();
    w.write_all(MAGIC).map_err(io)?;
    w.write_all(&VERSION.to_le_bytes()).map_err(io)?;
    w.write_all(&fingerprint.to_le_bytes()).map_err(io)?;
    w.write_all(&live.to_le_bytes()).map_err(io)?;
    w.write_all(&names_len.to_le_bytes()).map_err(io)?;
    for (i, e) in entries.iter().enumerate() {
        if remap[i] == NONE {
            continue;
        }
        let parent = if e.parent == NONE { NONE } else { remap[e.parent as usize] };
        w.write_all(&(ix.name(i as u32).len() as u16).to_le_bytes()).map_err(io)?;
        w.write_all(&e.flags.to_le_bytes()).map_err(io)?;
        w.write_all(&parent.to_le_bytes()).map_err(io)?;
        w.write_all(&e.size.to_le_bytes()).map_err(io)?;
        w.write_all(&e.mtime().to_le_bytes()).map_err(io)?;
    }
    for (i, &m) in remap.iter().enumerate() {
        if m != NONE {
            w.write_all(ix.name(i as u32).as_bytes()).map_err(io)?;
        }
    }
    for &id in by_name {
        let m = remap[id as usize];
        if m != NONE {
            w.write_all(&m.to_le_bytes()).map_err(io)?;
        }
    }
    w.flush().map_err(io)?;
    drop(w);
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

/// Loads a saved index if it exists, is intact, and was built with the same settings.
pub fn load(path: &Path, fingerprint: u64) -> Option<Index> {
    let data = std::fs::read(path).ok()?;
    match parse(&data, fingerprint) {
        Ok(ix) => Some(ix),
        Err(e) => {
            eprintln!("quarry: not using saved index ({e})");
            None
        }
    }
}

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self.pos.checked_add(n).ok_or("truncated")?;
        let s = self.data.get(self.pos..end).ok_or("truncated")?;
        self.pos = end;
        Ok(s)
    }
    fn u16(&mut self) -> Result<u16, String> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
}

fn parse(data: &[u8], fingerprint: u64) -> Result<Index, String> {
    let mut c = Cursor { data, pos: 0 };
    if c.take(8)? != MAGIC {
        return Err("not an index file".into());
    }
    if c.u32()? != VERSION {
        return Err("index from another version".into());
    }
    if c.u64()? != fingerprint {
        return Err("settings changed".into());
    }
    let count = c.u32()? as usize;
    let names_len = c.u64()? as usize;
    if count.checked_mul(RECORD + 4).is_none_or(|n| n > data.len()) || names_len > data.len() {
        return Err("truncated".into());
    }
    let mut records = Vec::with_capacity(count);
    for _ in 0..count {
        let len = c.u16()?;
        let flags = c.u16()?;
        let parent = c.u32()?;
        let size = c.u64()?;
        let mtime = c.u64()? as i64;
        records.push((len, flags, parent, size, mtime));
    }
    let names = String::from_utf8(c.take(names_len)?.to_vec()).map_err(|_| "corrupt names")?;
    let mut by_name = Vec::with_capacity(count);
    for _ in 0..count {
        by_name.push(c.u32()?);
    }
    if c.pos != data.len() {
        return Err("trailing data".into());
    }
    Index::from_parts(records, names, by_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{DIR, Meta};

    #[test]
    fn round_trip_drops_tombstones() {
        let mut ix = Index::new();
        let r = ix.add_root("/", Meta { flags: DIR, ..Meta::default() });
        let d = ix.add(r, "dir", Meta { flags: DIR, ..Meta::default() });
        ix.add(d, "keep.txt", Meta { size: 3, mtime: 42, ..Meta::default() });
        let gone = ix.add(d, "gone.txt", Meta { size: 9, ..Meta::default() });
        ix.add(r, "zeta", Meta::default());
        ix.finish_bulk();
        ix.remove(gone);
        ix.merge_pending();
        let path = std::env::temp_dir().join(format!("quarry-store-{}.bin", std::process::id()));
        save(&ix, &path, 7).unwrap();
        assert!(load(&path, 8).is_none());
        let back = load(&path, 7).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(back.live_count(), 4);
        let names: Vec<&str> = back.by_name().iter().map(|&i| back.name(i)).collect();
        assert_eq!(names, ["/", "dir", "keep.txt", "zeta"]);
        let keep = back.lookup(Path::new("/dir/keep.txt")).unwrap();
        assert_eq!(back.entry(keep).mtime(), 42);
        assert_eq!(back.entry(back.roots()[0]).size, 3);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse(b"QUARRYIX\x01\0\0\0", 0).is_err());
        assert!(parse(b"nope", 0).is_err());
    }
}
