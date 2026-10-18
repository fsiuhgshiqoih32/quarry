//! Command-line interface, shared by `quarry` and `quarry-cli`.

use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};

use crate::config::{self, Config, DEFAULT_PORT};
use crate::engine::Engine;
use crate::index::Index;
use crate::query::{Options, Query};
use crate::scan::{self, Excludes, Progress};
use crate::search::{self, SortKey};
use crate::{serve, store};

#[derive(Parser, Debug)]
#[command(
    name = "quarry",
    version,
    about = "Instant file search for Windows, macOS and Linux.",
    after_help = "Run without a command to open the search window.\nSearch syntax: https://github.com/fsiuhgshiqoih32/quarry#search-syntax"
)]
pub struct Cli {
    /// Start in the tray without showing the window (used by start at login).
    #[arg(long)]
    pub hidden: bool,
    /// Open the window with this search filled in (or send it to the window already open).
    #[arg(long, value_name = "TEXT")]
    pub query: Option<String>,
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Print the files and folders that match a query (Everything search syntax).
    Search {
        /// The query, e.g. `ext:pdf invoice dm:thisyear`. Several words are joined with spaces.
        query: Vec<String>,
        /// Maximum number of results; 0 for all.
        #[arg(short = 'n', long, default_value_t = 100)]
        limit: usize,
        /// Sort by name, path, size or modified.
        #[arg(short, long, default_value = "name")]
        sort: String,
        /// Reverse the order.
        #[arg(short, long)]
        desc: bool,
        /// One JSON object per line: name, path, folder, size, modified.
        #[arg(long)]
        json: bool,
        /// Print only the number of matches.
        #[arg(short, long)]
        count: bool,
        /// Rebuild the index first instead of using the saved one.
        #[arg(long)]
        rescan: bool,
        /// Match full paths, not just names.
        #[arg(short = 'p', long)]
        match_path: bool,
    },
    /// Rebuild the index now and save it.
    Index,
    /// Serve the HTTP API without opening a window (keeps the index live).
    Serve {
        #[arg(long, default_value_t = DEFAULT_PORT)]
        port: u16,
    },
    /// Print where the settings and the index are stored.
    Paths,
    /// Check scanning, searching, saving, live updates and the API in a temporary folder.
    #[command(hide = true)]
    SelfTest {
        /// Also open the window for a few frames (the binary handles this part).
        #[arg(long)]
        gui: bool,
    },
}

/// Runs a command and returns the process exit code.
pub fn run(cmd: Command) -> i32 {
    let result = match cmd {
        Command::Search { query, limit, sort, desc, json, count, rescan, match_path } => {
            search_cmd(&query.join(" "), limit, &sort, desc, json, count, rescan, match_path)
        }
        Command::Index => index_cmd(),
        Command::Serve { port } => serve_cmd(port),
        Command::Paths => {
            println!("settings: {}", config::config_path().display());
            println!("index:    {}", config::index_path().display());
            Ok(())
        }
        Command::SelfTest { .. } => self_test(),
    };
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("quarry: {e}");
            1
        }
    }
}

fn build_index(cfg: &Config) -> Index {
    let roots = cfg.effective_roots();
    let ex = Excludes::new(&cfg.excludes, config::builtin_excludes(), &roots);
    let progress = Progress::default();
    let start = Instant::now();
    eprint!("quarry: indexing {} ...", roots.iter().map(|r| r.display().to_string()).collect::<Vec<_>>().join(", "));
    let ix = scan::scan(&roots, &ex, &progress);
    eprintln!(" {} items in {:.1?}", crate::fmt::count(ix.live_count()), start.elapsed());
    if let Err(e) = store::save(&ix, &config::index_path(), cfg.fingerprint()) {
        eprintln!("quarry: could not save the index: {e}");
    }
    ix
}

#[allow(clippy::too_many_arguments)]
fn search_cmd(
    text: &str,
    limit: usize,
    sort: &str,
    desc: bool,
    json: bool,
    count: bool,
    rescan: bool,
    match_path: bool,
) -> Result<(), String> {
    let cfg = Config::load(&config::config_path());
    let sort =
        SortKey::parse(sort).ok_or_else(|| format!("unknown sort {sort:?}; use name, path, size or modified"))?;
    let ix = match (rescan, store::load(&config::index_path(), cfg.fingerprint())) {
        (false, Some(ix)) => ix,
        _ => build_index(&cfg),
    };
    let opts = Options { match_path: match_path || cfg.match_path, show_hidden: cfg.show_hidden };
    let q = Query::parse(text, opts, Some(&ix))?;
    let ids = search::run(&ix, &q, sort, desc, &|| false).unwrap_or_default();
    let mut out = BufWriter::new(std::io::stdout().lock());
    // A closed pipe (`quarry search x | head`) just ends the output.
    let _ = (|| -> std::io::Result<()> {
        if count {
            writeln!(out, "{}", ids.len())?;
            return Ok(());
        }
        let take = if limit == 0 { ids.len() } else { limit };
        for &id in ids.iter().take(take) {
            if json {
                writeln!(out, "{}", serve::entry_json(&ix, id))?;
            } else {
                writeln!(out, "{}", ix.path(id))?;
            }
        }
        out.flush()
    })();
    Ok(())
}

fn index_cmd() -> Result<(), String> {
    let cfg = Config::load(&config::config_path());
    build_index(&cfg);
    println!("saved {}", config::index_path().display());
    Ok(())
}

fn serve_cmd(port: u16) -> Result<(), String> {
    let cfg = Config::load(&config::config_path());
    let opts = Options { match_path: cfg.match_path, show_hidden: cfg.show_hidden };
    let engine = Engine::start(cfg, config::index_path(), || {});
    let api = serve::Api::start(engine.shared.clone(), port, opts)?;
    println!("quarry: serving http://127.0.0.1:{}/search?q=... (Ctrl+C to stop)", api.port);
    api.join();
    Ok(())
}

fn check(ok: bool, what: &str) -> Result<(), String> {
    if ok {
        println!("  ok   {what}");
        Ok(())
    } else {
        Err(format!("self-test failed: {what}"))
    }
}

fn wait_for(what: &str, timeout: Duration, mut cond: impl FnMut() -> bool) -> Result<(), String> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if cond() {
            println!("  ok   {what} ({:.0?})", start.elapsed());
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(format!("self-test failed: {what} (waited {timeout:?})"))
}

fn find(ix: &Index, text: &str) -> Vec<String> {
    let q = Query::parse(text, Options::default(), Some(ix)).expect("query");
    search::run(ix, &q, SortKey::Name, false, &|| false).unwrap().iter().map(|&i| ix.name(i).to_string()).collect()
}

/// Exercises the whole pipeline on a temporary folder. Used by CI on every platform.
pub fn self_test() -> Result<(), String> {
    let base = std::env::temp_dir().join(format!("quarry-self-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("docs/deep")).map_err(|e| e.to_string())?;
    // Canonical, so file system events (which report real paths, e.g. /private/var on macOS) match.
    let root = std::fs::canonicalize(&base).map_err(|e| e.to_string())?;
    let root = PathBuf::from(root.to_string_lossy().trim_start_matches(r"\\?\"));
    let result = self_test_in(&root);
    let _ = std::fs::remove_dir_all(&base);
    if result.is_ok() {
        println!("self-test passed");
    }
    result
}

fn self_test_in(root: &Path) -> Result<(), String> {
    let w = |p: &str, data: &[u8]| std::fs::write(root.join(p), data).map_err(|e| e.to_string());
    w("docs/Report 2026.pdf", &[0u8; 2048])?;
    w("docs/deep/notes.txt", b"hello")?;
    w("song.mp3", &[0u8; 100])?;
    println!("self-test in {}", root.display());

    let ix = scan::scan(&[root.to_path_buf()], &Excludes::default(), &Progress::default());
    check(ix.live_count() == 6, "scan finds every file and folder")?;
    check(find(&ix, "report") == ["Report 2026.pdf"], "substring search")?;
    check(find(&ix, "ext:mp3;txt") == ["notes.txt", "song.mp3"], "ext: filter")?;
    check(find(&ix, "size:>1kb file:") == ["Report 2026.pdf"], "size: filter")?;
    check(find(&ix, "deep/ | *.mp3").len() == 2, "path terms, wildcards and OR")?;

    let saved = root.join("index.bin");
    store::save(&ix, &saved, 1)?;
    let back = store::load(&saved, 1).ok_or("saved index did not load")?;
    check(back.live_count() == ix.live_count(), "index survives save and load")?;
    std::fs::remove_file(&saved).map_err(|e| e.to_string())?;

    let cfg = Config {
        roots: vec![root.to_string_lossy().into_owned()],
        live_updates: true,
        rescan_on_start: true,
        ..Config::default()
    };
    let changes = Arc::new(AtomicU64::new(0));
    let c = changes.clone();
    let state_dir = std::env::temp_dir().join(format!("quarry-self-test-state-{}", std::process::id()));
    let mut engine = Engine::start(cfg.clone(), state_dir.join("index.bin"), move || {
        c.fetch_add(1, Ordering::Relaxed);
    });
    let shared = engine.shared.clone();
    let names = |text: &str| find(&shared.index.read(), text);
    wait_for("engine finishes its first scan", Duration::from_secs(30), || {
        !shared.status.lock().scanning && shared.index.read().live_count() == 6
    })?;
    let watching = shared.status.lock().watching.clone();
    println!("       live updates: {watching:?}");
    // Give the watcher a moment to arm before changing files.
    std::thread::sleep(Duration::from_millis(500));
    w("docs/fresh-file.log", b"new")?;
    wait_for("live update picks up a new file", Duration::from_secs(20), || names("fresh-file") == ["fresh-file.log"])?;
    std::fs::rename(root.join("docs/fresh-file.log"), root.join("docs/renamed.log")).map_err(|e| e.to_string())?;
    wait_for("live update follows a rename", Duration::from_secs(20), || {
        names("fresh-file").is_empty() && names("renamed") == ["renamed.log"]
    })?;
    std::fs::remove_file(root.join("song.mp3")).map_err(|e| e.to_string())?;
    wait_for("live update drops a deleted file", Duration::from_secs(20), || names("song").is_empty())?;

    let api = serve::Api::start(shared.clone(), 0, Options::default())?;
    let body = http_get(api.port, "/search?q=renamed")?;
    check(body.contains("renamed.log") && body.contains("\"total\":1"), "HTTP API answers searches")?;
    let refused = http_get_host(api.port, "/status", "evil.example")?;
    check(refused.contains("loopback"), "HTTP API refuses foreign Host headers")?;
    drop(api);
    engine.shutdown();

    // Changes made while Quarry is closed show up once it starts again from the saved index.
    w("docs/while-closed.txt", b"x")?;
    std::fs::remove_file(root.join("docs/renamed.log")).map_err(|e| e.to_string())?;
    let mut engine = Engine::start(cfg, state_dir.join("index.bin"), || {});
    let shared = engine.shared.clone();
    wait_for("start-up catches up with changes made while closed", Duration::from_secs(30), || {
        let ix = shared.index.read();
        find(&ix, "while-closed") == ["while-closed.txt"] && find(&ix, "renamed").is_empty()
    })?;
    check(shared.status.lock().loaded_from_disk, "start-up reuses the saved index")?;
    engine.shutdown();
    let _ = std::fs::remove_dir_all(&state_dir);
    Ok(())
}

fn http_get(port: u16, path: &str) -> Result<String, String> {
    http_get_host(port, path, &format!("127.0.0.1:{port}"))
}

fn http_get_host(port: u16, path: &str, host: &str) -> Result<String, String> {
    use std::io::Read;
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).map_err(|e| e.to_string())?;
    s.set_read_timeout(Some(Duration::from_secs(10))).map_err(|e| e.to_string())?;
    write!(s, "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").map_err(|e| e.to_string())?;
    let mut out = String::new();
    s.read_to_string(&mut out).map_err(|e| e.to_string())?;
    Ok(out)
}
