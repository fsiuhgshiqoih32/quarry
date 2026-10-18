//! Running queries over the index, sorting the results, and a background worker for the UI.

use std::cmp::Reverse;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, unbounded};
use parking_lot::{Mutex, RwLock};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::index::{Index, NONE};
use crate::query::{Options, Query, Scratch};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SortKey {
    #[default]
    Name,
    Path,
    Size,
    Modified,
}

impl SortKey {
    pub fn parse(s: &str) -> Option<SortKey> {
        Some(match s.to_ascii_lowercase().as_str() {
            "name" => SortKey::Name,
            "path" => SortKey::Path,
            "size" => SortKey::Size,
            "modified" | "date" | "dm" => SortKey::Modified,
            _ => return None,
        })
    }
}

/// Runs a parsed query and returns matching ids in the requested order. Returns `None` if
/// `cancelled` turned true part-way.
pub fn run(
    ix: &Index,
    q: &Query,
    sort: SortKey,
    desc: bool,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Option<Vec<u32>> {
    const CHUNK: usize = 1 << 14;
    let mut ids: Vec<u32> = if q.is_empty() {
        // Everything (minus hidden files, maybe), already in name order.
        let parts: Vec<Option<Vec<u32>>> = ix
            .by_name()
            .par_chunks(CHUNK)
            .map_init(Scratch::default, |s, chunk| {
                (!cancelled()).then(|| chunk.iter().copied().filter(|&id| q.matches(ix, id, s)).collect())
            })
            .collect();
        concat(parts)?
    } else {
        let parts: Vec<Option<Vec<u32>>> = match q.prefilter() {
            Some((needle, case)) => {
                let cands = candidates(ix, needle, case, cancelled)?;
                cands
                    .par_chunks(CHUNK)
                    .map_init(Scratch::default, |s, chunk| {
                        (!cancelled()).then(|| chunk.iter().copied().filter(|&id| q.matches(ix, id, s)).collect())
                    })
                    .collect()
            }
            None => {
                // Walk ids in storage order: sequential memory beats name order by a wide margin.
                let n = ix.len();
                (0..n.div_ceil(CHUNK))
                    .into_par_iter()
                    .map_init(Scratch::default, |s, c| {
                        let (lo, hi) = (c * CHUNK, ((c + 1) * CHUNK).min(n));
                        (!cancelled()).then(|| (lo as u32..hi as u32).filter(|&id| q.matches(ix, id, s)).collect())
                    })
                    .collect()
            }
        };
        let mut hits = concat(parts)?;
        hits.par_sort_unstable_by_key(|&i| ix.rank(i));
        hits.dedup();
        hits
    };
    if cancelled() {
        return None;
    }
    sort_ids(ix, &mut ids, sort, desc);
    Some(ids)
}

fn concat(parts: Vec<Option<Vec<u32>>>) -> Option<Vec<u32>> {
    let mut out = Vec::with_capacity(parts.iter().map(|p| p.as_ref().map_or(0, Vec::len)).sum());
    for p in parts {
        out.extend(p?);
    }
    Some(out)
}

/// Ids whose name contains `needle`, found with one SIMD pass over the name arena per thread.
fn candidates(ix: &Index, needle: &[u8], case: bool, cancelled: &(dyn Fn() -> bool + Sync)) -> Option<Vec<u32>> {
    let hay = if case { ix.names_bytes() } else { ix.folded_bytes() };
    let n = hay.len();
    if n == 0 {
        return Some(Vec::new());
    }
    let chunk = (n / (rayon::current_num_threads() * 8)).max(1 << 20);
    let finder = memchr::memmem::Finder::new(needle);
    let parts: Vec<Option<Vec<u32>>> = (0..n.div_ceil(chunk))
        .into_par_iter()
        .map(|c| {
            if cancelled() {
                return None;
            }
            let (start, end) = (c * chunk, ((c + 1) * chunk).min(n));
            // Let matches that start in this chunk run past its end.
            let window = &hay[start..(end + needle.len() - 1).min(n)];
            let mut out = Vec::new();
            let mut last = NONE;
            let mut cur: Option<(u32, usize)> = None;
            for pos in finder.find_iter(window) {
                let off = start + pos;
                if off >= end {
                    break;
                }
                let (id, name_end) = match cur {
                    Some((id, e)) if off < e => (id, e),
                    _ => {
                        let Some(id) = ix.id_at(off, 0) else { continue };
                        let e = ix.name_end(id);
                        cur = Some((id, e));
                        (id, e)
                    }
                };
                // Skip matches that run from one name into the next.
                if id != last && off + needle.len() <= name_end {
                    out.push(id);
                    last = id;
                }
            }
            Some(out)
        })
        .collect();
    concat(parts)
}

/// Sorts ids that are in name order; ties stay in name order.
pub fn sort_ids(ix: &Index, ids: &mut [u32], sort: SortKey, desc: bool) {
    match sort {
        SortKey::Name => {
            if desc {
                ids.reverse();
            }
        }
        // Ids arrive in name order, so stable sorts on one key keep name order for ties.
        SortKey::Size => {
            if desc {
                ids.par_sort_by_key(|&i| Reverse(ix.entry(i).size));
            } else {
                ids.par_sort_by_key(|&i| ix.entry(i).size);
            }
        }
        SortKey::Modified => {
            if desc {
                ids.par_sort_by_key(|&i| Reverse(ix.entry(i).mtime()));
            } else {
                ids.par_sort_by_key(|&i| ix.entry(i).mtime());
            }
        }
        SortKey::Path => {
            // Rank each distinct folder by its path once, then sort by folder rank.
            let mut parents: Vec<u32> = ids.par_iter().map(|&i| ix.entry(i).parent).collect();
            parents.par_sort_unstable();
            parents.dedup();
            let mut keyed: Vec<(Vec<u8>, u32)> = parents
                .par_iter()
                .map(|&p| (if p == NONE { Vec::new() } else { ix.path(p).to_lowercase().into_bytes() }, p))
                .collect();
            keyed.par_sort_unstable();
            // Roots have no parent; they sort first.
            let mut rank = vec![0u32; ix.len()];
            for (r, (_, p)) in keyed.iter().enumerate() {
                if *p != NONE {
                    rank[*p as usize] = r as u32 + 1;
                }
            }
            let key = |i: u32| match ix.entry(i).parent {
                NONE => 0,
                p => rank[p as usize],
            };
            if desc {
                ids.par_sort_by_key(|&i| Reverse(key(i)));
            } else {
                ids.par_sort_by_key(|&i| key(i));
            }
        }
    }
}

/// A search for the background worker.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    pub text: String,
    pub opts: Options,
    pub sort: SortKey,
    pub desc: bool,
}

pub struct Outcome {
    pub serial: u64,
    pub request: Request,
    pub ids: Arc<Vec<u32>>,
    /// Which index the ids belong to; see [`Index::epoch`].
    pub epoch: u64,
    pub revision: u64,
    pub elapsed: Duration,
    pub error: Option<String>,
    pub highlights: Vec<String>,
}

/// Runs searches on its own thread, always for the most recent request only.
pub struct Searcher {
    tx: Sender<(u64, Request)>,
    latest: Arc<AtomicU64>,
    done: Arc<Mutex<Option<Outcome>>>,
}

impl Searcher {
    pub fn new(index: Arc<RwLock<Index>>, notify: impl Fn() + Send + 'static) -> Searcher {
        let (tx, rx) = unbounded();
        let latest = Arc::new(AtomicU64::new(0));
        let done = Arc::new(Mutex::new(None));
        let (l, d) = (latest.clone(), done.clone());
        std::thread::Builder::new()
            .name("search".into())
            .spawn(move || worker(rx, index, l, d, notify))
            .expect("spawn search thread");
        Searcher { tx, latest, done }
    }

    /// Starts a search, abandoning any still running. Returns its serial number.
    pub fn submit(&self, req: Request) -> u64 {
        let serial = self.latest.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = self.tx.send((serial, req));
        serial
    }

    /// The newest finished search, once.
    pub fn take(&self) -> Option<Outcome> {
        self.done.lock().take()
    }
}

fn worker(
    rx: Receiver<(u64, Request)>,
    index: Arc<RwLock<Index>>,
    latest: Arc<AtomicU64>,
    done: Arc<Mutex<Option<Outcome>>>,
    notify: impl Fn(),
) {
    while let Ok(mut job) = rx.recv() {
        while let Ok(newer) = rx.try_recv() {
            job = newer;
        }
        let (serial, req) = job;
        let start = Instant::now();
        let ix = index.read();
        let cancelled = || latest.load(Ordering::Relaxed) != serial;
        let (ids, error, highlights) = match Query::parse(&req.text, req.opts, Some(&ix)) {
            Ok(q) => match run(&ix, &q, req.sort, req.desc, &cancelled) {
                Some(ids) => (ids, None, q.highlights),
                None => continue,
            },
            Err(e) => (Vec::new(), Some(e), Vec::new()),
        };
        let outcome = Outcome {
            serial,
            request: req,
            ids: Arc::new(ids),
            epoch: ix.epoch,
            revision: ix.revision,
            elapsed: start.elapsed(),
            error,
            highlights,
        };
        drop(ix);
        *done.lock() = Some(outcome);
        notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{DIR, Meta};

    #[test]
    fn sorting() {
        let mut ix = Index::new();
        let r = ix.add_root("/", Meta::default());
        let b = ix.add(r, "b", Meta { flags: DIR, ..Meta::default() });
        let a = ix.add(r, "a", Meta { flags: DIR, ..Meta::default() });
        ix.add(b, "x1", Meta { size: 30, mtime: 5, ..Meta::default() });
        ix.add(a, "x2", Meta { size: 10, mtime: 9, ..Meta::default() });
        ix.add(a, "x3", Meta { size: 20, mtime: 1, ..Meta::default() });
        ix.finish_bulk();
        let q = Query::parse("x", Options::default(), Some(&ix)).unwrap();
        let get = |sort, desc| -> Vec<String> {
            run(&ix, &q, sort, desc, &|| false).unwrap().iter().map(|&i| ix.name(i).to_string()).collect()
        };
        assert_eq!(get(SortKey::Name, false), ["x1", "x2", "x3"]);
        assert_eq!(get(SortKey::Name, true), ["x3", "x2", "x1"]);
        assert_eq!(get(SortKey::Size, false), ["x2", "x3", "x1"]);
        assert_eq!(get(SortKey::Modified, true), ["x2", "x1", "x3"]);
        assert_eq!(get(SortKey::Path, false), ["x2", "x3", "x1"]);
        assert!(run(&ix, &q, SortKey::Name, false, &|| true).is_none());
    }

    #[test]
    fn fast_path_matches_brute_force() {
        let mut ix = Index::new();
        let r = ix.add_root("/", Meta { flags: DIR, ..Meta::default() });
        let words = ["ab", "cd", "abc", "bcd", "Abcd", "xab", "dab", "b", "ABAB", "cdab"];
        let mut dirs = vec![r];
        for i in 0..400u32 {
            let name =
                format!("{}{}", words[(i * 7 % 10) as usize], if i % 3 == 0 { "" } else { words[(i % 10) as usize] });
            let parent = dirs[(i as usize * 13) % dirs.len()];
            let id = ix.add(parent, &name, Meta { flags: if i % 5 == 0 { DIR } else { 0 }, ..Meta::default() });
            if i % 5 == 0 {
                dirs.push(id);
            }
        }
        ix.finish_bulk();
        for text in ["ab", "bc", "dab", "case:AB", "*cd", "b*a", "abc|cd", "ab !cd", "startwith:cd", "ww:ab", "da"] {
            let q = Query::parse(text, Options::default(), Some(&ix)).unwrap();
            let mut s = Scratch::default();
            let brute: Vec<u32> = ix.by_name().iter().copied().filter(|&i| q.matches(&ix, i, &mut s)).collect();
            assert_eq!(run(&ix, &q, SortKey::Name, false, &|| false).unwrap(), brute, "query {text:?}");
        }
    }
}
