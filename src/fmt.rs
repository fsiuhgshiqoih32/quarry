//! Human-readable sizes, dates and counts.

use chrono::{Local, TimeZone};

pub fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if v < 10.0 { format!("{v:.1} {}", UNITS[u]) } else { format!("{v:.0} {}", UNITS[u]) }
}

pub fn date(secs: i64) -> String {
    match Local.timestamp_opt(secs, 0).single() {
        Some(t) if secs != 0 => t.format("%Y-%m-%d %H:%M").to_string(),
        _ => String::new(),
    }
}

pub fn count(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn formats() {
        assert_eq!(super::size(512), "512 B");
        assert_eq!(super::size(1536), "1.5 KB");
        assert_eq!(super::size(50 * 1024 * 1024), "50 MB");
        assert_eq!(super::count(1234567), "1,234,567");
        assert_eq!(super::count(12), "12");
    }
}
