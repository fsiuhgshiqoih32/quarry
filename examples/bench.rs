//! Scans the default roots and times a few searches.
//!
//!     cargo run --release --example bench [root...]

use std::path::PathBuf;
use std::time::Instant;

use quarry::config::{self, Config};
use quarry::query::{Options, Query};
use quarry::scan::{self, Excludes, Progress};
use quarry::search::{self, SortKey};

fn main() {
    let args: Vec<PathBuf> = std::env::args().skip(1).map(PathBuf::from).collect();
    let roots = if args.is_empty() { Config::default().effective_roots() } else { args };
    let ex = Excludes::new(&[], config::builtin_excludes(), &roots);
    let t = Instant::now();
    let p = Progress::default();
    let ix = scan::scan(&roots, &ex, &p);
    println!(
        "scanned {:?}: {} entries in {:.2?} ({} unreadable folders)",
        roots,
        ix.live_count(),
        t.elapsed(),
        p.unreadable.load(std::sync::atomic::Ordering::Relaxed)
    );
    let path = std::env::temp_dir().join("quarry-bench.bin");
    let t = Instant::now();
    quarry::store::save(&ix, &path, 1).unwrap();
    let size = std::fs::metadata(&path).unwrap().len();
    println!("saved {} MB in {:.2?}", size / 1_000_000, t.elapsed());
    let t = Instant::now();
    let back = quarry::store::load(&path, 1).unwrap();
    println!("loaded {} entries in {:.2?}", back.live_count(), t.elapsed());
    std::fs::remove_file(&path).ok();
    for q in [
        "",
        "a",
        "report",
        "*.dll",
        "ext:png;jpg size:>1mb",
        r"windows\system32 ntdll",
        r"regex:^lib.*\.so$",
        "dm:today",
    ] {
        for sort in [SortKey::Name, SortKey::Size, SortKey::Path] {
            let t = Instant::now();
            let query = Query::parse(q, Options::default(), Some(&ix)).unwrap();
            let ids = search::run(&ix, &query, sort, false, &|| false).unwrap();
            println!("{:<34} {:>9} hits  {:>10.2?}  sort={sort:?}", format!("{q:?}"), ids.len(), t.elapsed());
        }
    }
}
