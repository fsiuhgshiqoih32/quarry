//! One window per user: a second launch asks the running one to come forward, then exits.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use quarry::config;

pub enum Acquire {
    First(Listener),
    Running,
    /// The lock couldn't be checked; run anyway.
    Unknown,
}

pub struct Listener {
    _lock: File,
    socket: TcpListener,
}

const LOCK: &str = "instance.lock";
const PORT: &str = "instance.port";

pub fn acquire() -> Acquire {
    let dir = config::config_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return Acquire::Unknown;
    }
    let Ok(file) = OpenOptions::new().create(true).truncate(false).write(true).open(dir.join(LOCK)) else {
        return Acquire::Unknown;
    };
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Acquire::Running,
        Err(TryLockError::Error(_)) => return Acquire::Unknown,
    }
    let Ok(socket) = TcpListener::bind(("127.0.0.1", 0)) else { return Acquire::Unknown };
    if let Ok(addr) = socket.local_addr() {
        let _ = std::fs::write(dir.join(PORT), addr.port().to_string());
    }
    Acquire::First(Listener { _lock: file, socket })
}

/// Tells the running instance to show its window, with a new search if `query` is given.
pub fn signal_show(query: Option<&str>) {
    let Ok(text) = std::fs::read_to_string(config::config_dir().join(PORT)) else { return };
    let Ok(port) = text.trim().parse::<u16>() else { return };
    if let Ok(mut s) = TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_secs(2)) {
        let line = match query {
            Some(q) => format!("query {}\n", q.replace(['\n', '\r'], " ")),
            None => "show\n".to_string(),
        };
        let _ = s.write_all(line.as_bytes());
    }
}

/// Calls `on_show` whenever another launch asks for the window (with its search, if it sent one).
/// Holds the lock for the process's life.
pub fn serve(l: Listener, on_show: impl Fn(Option<String>) + Send + 'static) {
    let _ = std::thread::Builder::new().name("instance".into()).spawn(move || {
        let Listener { _lock, socket } = l;
        for stream in socket.incoming().flatten() {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let mut line = String::new();
            if BufReader::new(stream).read_line(&mut line).is_err() {
                continue;
            }
            let line = line.trim_end_matches(['\n', '\r']);
            if line == "show" {
                on_show(None);
            } else if let Some(q) = line.strip_prefix("query ") {
                on_show(Some(q.to_string()));
            }
        }
    });
}
