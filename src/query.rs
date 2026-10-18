//! The search syntax, modelled on Everything's:
//!
//! - `foo bar` matches names containing both; `foo|bar` either; `!foo` excludes; `<a|b> c` groups.
//! - `"two words"` keeps spaces; `*` and `?` are wildcards matched against the whole name.
//! - A term with a path separator matches the full path instead of the name.
//! - Modifiers: `case:`, `nocase:`, `path:`, `nopath:`, `ww:` (whole word), `file:`, `folder:`, `regex:`.
//! - Functions: `ext:pdf;docx`, `size:>10mb`, `size:1mb..5mb`, `dm:today`, `dm:last7days`,
//!   `dm:2026-01-01..2026-03-31`, `parent:<folder>`, `startwith:`, `endwith:`.

use std::path::Path;

use chrono::{DateTime, Datelike, Local, Months, NaiveDate, TimeDelta, TimeZone};
use memchr::memmem::Finder;
use regex::{Regex, RegexBuilder};

use crate::index::{Entry, Index, fold};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    /// Match full paths for plain terms (Everything's "Match path").
    pub match_path: bool,
    /// Include hidden files and anything inside hidden folders.
    pub show_hidden: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options { match_path: false, show_hidden: true }
    }
}

pub struct Query {
    root: Node,
    show_hidden: bool,
    /// `size:` normally tests files only; asking for folders makes it test folders too.
    folders_requested: bool,
    /// Lowercase literal fragments of positive name terms, for highlighting matches.
    pub highlights: Vec<String>,
}

enum Node {
    All,
    And(Vec<Node>),
    Or(Vec<Node>),
    Not(Box<Node>),
    Leaf(Leaf),
}

struct Leaf {
    test: Test,
    path: bool,
    /// Bytes every matching name must contain (ASCII-folded unless the bool, case-sensitive, is
    /// set). Lets a search skip straight to candidates with one pass over the name arena.
    lit: Option<(Vec<u8>, bool)>,
}

enum Test {
    /// Substring. The needle is ASCII-folded unless `case`; `unicode` holds the lowercased needle
    /// when it has non-ASCII letters, which need full Unicode case folding.
    Substr {
        finder: Box<Finder<'static>>,
        case: bool,
        unicode: Option<String>,
        whole_word: bool,
    },
    Prefix {
        needle: Vec<u8>,
        case: bool,
    },
    Suffix {
        needle: Vec<u8>,
        case: bool,
    },
    Regex(Regex),
    Ext(Vec<Vec<u8>>),
    Size(u64, u64),
    Date(i64, i64),
    Files,
    Folders,
    Parent(Option<u32>),
}

#[derive(Debug, PartialEq)]
enum Tok {
    Word(String),
    Or,
    Not,
    Open,
    Close,
}

fn lex(input: &str) -> Vec<Tok> {
    let chars: Vec<char> = input.chars().collect();
    let mut toks = Vec::new();
    let mut depth = 0usize;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        let single = match c {
            '|' => Some(Tok::Or),
            '!' => Some(Tok::Not),
            '<' => {
                depth += 1;
                Some(Tok::Open)
            }
            '>' if depth > 0 => {
                depth -= 1;
                Some(Tok::Close)
            }
            _ => None,
        };
        if let Some(t) = single {
            toks.push(t);
            i += 1;
            continue;
        }
        let mut w = String::new();
        while i < chars.len() {
            let c = chars[i];
            if c == '"' {
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    w.push(chars[i]);
                    i += 1;
                }
                i += 1;
                continue;
            }
            if c.is_whitespace() || c == '|' {
                break;
            }
            // `<` and `>` right after a colon are comparisons (`size:>1mb`), otherwise grouping.
            let comparison = w.ends_with(':') || w.ends_with(":>") || w.ends_with(":<");
            if (c == '<' || (c == '>' && depth > 0)) && !comparison {
                break;
            }
            w.push(c);
            i += 1;
        }
        match w.as_str() {
            "OR" => toks.push(Tok::Or),
            "NOT" => toks.push(Tok::Not),
            "AND" | "" => {}
            _ => toks.push(Tok::Word(w)),
        }
    }
    toks
}

struct Compiler<'a> {
    toks: Vec<Tok>,
    pos: usize,
    index: Option<&'a Index>,
    opts: Options,
    now: DateTime<Local>,
    folders_requested: bool,
    highlights: Vec<String>,
    negated: usize,
}

impl Query {
    /// Parses a query. `index` resolves `parent:` folders; without one they match nothing.
    pub fn parse(text: &str, opts: Options, index: Option<&Index>) -> Result<Query, String> {
        Self::parse_at(text, opts, index, Local::now())
    }

    pub fn parse_at(text: &str, opts: Options, index: Option<&Index>, now: DateTime<Local>) -> Result<Query, String> {
        let mut c = Compiler {
            toks: lex(text),
            pos: 0,
            index,
            opts,
            now,
            folders_requested: false,
            highlights: Vec::new(),
            negated: 0,
        };
        let root = c.parse_and(false)?;
        let mut highlights = c.highlights;
        highlights.sort_by_key(|h| std::cmp::Reverse(h.len()));
        highlights.dedup();
        Ok(Query { root, show_hidden: opts.show_hidden, folders_requested: c.folders_requested, highlights })
    }

    /// True if the query has no terms (an empty search box lists everything).
    pub fn is_empty(&self) -> bool {
        matches!(self.root, Node::All)
    }

    /// The longest literal that every match's name must contain, and whether it is case-sensitive.
    pub fn prefilter(&self) -> Option<(&[u8], bool)> {
        fn walk<'a>(n: &'a Node, best: &mut Option<(&'a [u8], bool)>) {
            match n {
                Node::Leaf(Leaf { lit: Some((b, case)), .. }) => {
                    if !b.is_empty() && best.is_none_or(|(cur, _)| b.len() > cur.len()) {
                        *best = Some((b, *case));
                    }
                }
                Node::And(v) => v.iter().for_each(|c| walk(c, best)),
                _ => {}
            }
        }
        let mut best = None;
        walk(&self.root, &mut best);
        best
    }

    pub fn matches(&self, ix: &Index, id: u32, scratch: &mut Scratch) -> bool {
        let e = ix.entry(id);
        if e.is_deleted() || (!self.show_hidden && e.is_hidden()) {
            return false;
        }
        scratch.id = u32::MAX;
        self.eval(&self.root, ix, id, e, scratch)
    }

    fn eval(&self, node: &Node, ix: &Index, id: u32, e: &Entry, s: &mut Scratch) -> bool {
        match node {
            Node::All => true,
            Node::And(v) => v.iter().all(|n| self.eval(n, ix, id, e, s)),
            Node::Or(v) => v.iter().any(|n| self.eval(n, ix, id, e, s)),
            Node::Not(n) => !self.eval(n, ix, id, e, s),
            Node::Leaf(l) => self.test(l, ix, id, e, s),
        }
    }

    fn test(&self, l: &Leaf, ix: &Index, id: u32, e: &Entry, s: &mut Scratch) -> bool {
        match &l.test {
            Test::Files => return !e.is_dir(),
            Test::Folders => return e.is_dir(),
            Test::Size(lo, hi) => return (!e.is_dir() || self.folders_requested) && e.size >= *lo && e.size < *hi,
            Test::Date(lo, hi) => return e.mtime() >= *lo && e.mtime() < *hi,
            Test::Parent(p) => return Some(e.parent) == *p,
            _ => {}
        }
        let (orig, folded): (&str, &[u8]) = if l.path {
            if s.id != id {
                ix.path_into(id, &mut s.path);
                s.folded.clear();
                s.folded.extend(s.path.bytes().map(|b| b.to_ascii_lowercase()));
                s.id = id;
            }
            (&s.path, &s.folded)
        } else {
            (ix.name(id), ix.folded_name(id))
        };
        match &l.test {
            Test::Substr { finder, case, unicode, whole_word } => {
                if let Some(u) = unicode {
                    let lower = orig.to_lowercase();
                    return if *whole_word {
                        find_word(lower.as_bytes(), u.as_bytes())
                    } else {
                        lower.contains(u.as_str())
                    };
                }
                let hay = if *case { orig.as_bytes() } else { folded };
                if *whole_word { find_word(hay, finder.needle()) } else { finder.find(hay).is_some() }
            }
            Test::Prefix { needle, case } => (if *case { orig.as_bytes() } else { folded }).starts_with(needle),
            Test::Suffix { needle, case } => (if *case { orig.as_bytes() } else { folded }).ends_with(needle),
            Test::Regex(re) => re.is_match(orig),
            Test::Ext(exts) => {
                let name = ix.folded_name(id);
                let ext: &[u8] = match memchr::memrchr(b'.', name) {
                    Some(p) => &name[p + 1..],
                    None => b"",
                };
                exts.iter().any(|x| x.as_slice() == ext)
            }
            _ => unreachable!(),
        }
    }
}

/// Reusable buffers for matching paths, one per search thread.
pub struct Scratch {
    id: u32,
    path: String,
    folded: Vec<u8>,
}

impl Default for Scratch {
    fn default() -> Self {
        Scratch { id: u32::MAX, path: String::new(), folded: Vec::new() }
    }
}

fn find_word(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    let word = |b: u8| b.is_ascii_alphanumeric() || b >= 0x80;
    memchr::memmem::find_iter(hay, needle).any(|at| {
        let before = at == 0 || !word(hay[at - 1]);
        let end = at + needle.len();
        let after = end == hay.len() || !word(hay[end]);
        before && after
    })
}

impl Compiler<'_> {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn parse_and(&mut self, nested: bool) -> Result<Node, String> {
        let mut items = Vec::new();
        while let Some(t) = self.peek() {
            if *t == Tok::Close {
                if nested {
                    break;
                }
                self.pos += 1;
                continue;
            }
            if let Some(n) = self.parse_or()? {
                items.push(n);
            }
        }
        Ok(match items.len() {
            0 => Node::All,
            1 => items.pop().unwrap(),
            _ => Node::And(items),
        })
    }

    fn parse_or(&mut self) -> Result<Option<Node>, String> {
        let mut alts = Vec::new();
        if let Some(n) = self.parse_unary()? {
            alts.push(n);
        }
        while self.peek() == Some(&Tok::Or) {
            self.pos += 1;
            if let Some(n) = self.parse_unary()? {
                alts.push(n);
            }
        }
        Ok(match alts.len() {
            0 => None,
            1 => alts.pop(),
            _ => Some(Node::Or(alts)),
        })
    }

    fn parse_unary(&mut self) -> Result<Option<Node>, String> {
        let Some(t) = self.toks.get(self.pos) else { return Ok(None) };
        match t {
            Tok::Not => {
                self.pos += 1;
                self.negated += 1;
                let inner = self.parse_unary();
                self.negated -= 1;
                Ok(inner?.map(|n| Node::Not(Box::new(n))))
            }
            Tok::Open => {
                self.pos += 1;
                let n = self.parse_and(true)?;
                if self.peek() == Some(&Tok::Close) {
                    self.pos += 1;
                }
                Ok(Some(n))
            }
            Tok::Word(w) => {
                let w = w.clone();
                self.pos += 1;
                self.term(&w).map(Some)
            }
            Tok::Or | Tok::Close => Ok(None),
        }
    }

    fn term(&mut self, word: &str) -> Result<Node, String> {
        let mut case = false;
        let mut path = self.opts.match_path;
        let mut whole_word = false;
        let mut kind: Option<Test> = None;
        let mut rest = word;
        let mut function: Option<(String, &str)> = None;
        while let Some((head, tail)) = rest.split_once(':') {
            let h = head.to_ascii_lowercase();
            match h.as_str() {
                "case" => case = true,
                "nocase" => case = false,
                "path" => path = true,
                "nopath" => path = false,
                "ww" | "wholeword" => whole_word = true,
                "file" | "files" => kind = Some(Test::Files),
                "folder" | "folders" => {
                    kind = Some(Test::Folders);
                    self.folders_requested = true;
                }
                "regex" | "ext" | "size" | "dm" | "datemodified" | "parent" | "infolder" | "startwith" | "endwith" => {
                    function = Some((h, tail));
                    break;
                }
                _ => break,
            }
            rest = tail;
        }
        let mut parts: Vec<Node> = Vec::new();
        if let Some(k) = kind {
            parts.push(Node::Leaf(Leaf { test: k, path: false, lit: None }));
        }
        let main = match function {
            Some((f, value)) => Some(self.function(&f, value, case, path)?),
            None if rest.is_empty() => None,
            None => Some(self.text(rest, case, path, whole_word)?),
        };
        if let Some(m) = main {
            parts.push(Node::Leaf(m));
        }
        Ok(match parts.len() {
            0 => Node::All,
            1 => parts.pop().unwrap(),
            _ => Node::And(parts),
        })
    }

    fn text(&mut self, t: &str, case: bool, path: bool, whole_word: bool) -> Result<Leaf, String> {
        let has_sep = t.contains('/') || (cfg!(windows) && t.contains('\\'));
        let path = path || has_sep;
        let t: String = if cfg!(windows) && path { t.replace('/', "\\") } else { t.to_string() };
        if t.contains('*') || t.contains('?') {
            if !path && self.negated == 0 {
                for frag in t.split(['*', '?']).filter(|f| !f.is_empty()) {
                    self.highlights.push(frag.to_lowercase());
                }
            }
            let mut pat = String::from("^");
            for ch in t.chars() {
                match ch {
                    '*' => pat.push_str(".*"),
                    '?' => pat.push('.'),
                    c => pat.push_str(&regex::escape(&c.to_string())),
                }
            }
            pat.push('$');
            let longest = t.split(['*', '?']).max_by_key(|f| f.len()).unwrap_or("");
            let lit = (!path && !longest.is_empty() && (case || longest.is_ascii()))
                .then(|| (if case { longest.as_bytes().to_vec() } else { fold(longest) }, case));
            return Ok(Leaf { test: Test::Regex(build_regex(&pat, case)?), path, lit });
        }
        if !path && self.negated == 0 {
            self.highlights.push(t.to_lowercase());
        }
        let unicode = (!case && !t.is_ascii() && t.to_lowercase() != t.to_ascii_lowercase()).then(|| t.to_lowercase());
        let needle = if case { t.as_bytes().to_vec() } else { fold(&t) };
        let lit = (!path && unicode.is_none()).then(|| (needle.clone(), case));
        let finder = Box::new(Finder::new(&needle).into_owned());
        Ok(Leaf { test: Test::Substr { finder, case, unicode, whole_word }, path, lit })
    }

    fn function(&mut self, f: &str, value: &str, case: bool, path: bool) -> Result<Leaf, String> {
        let leaf = |test| Ok(Leaf { test, path, lit: None });
        match f {
            "regex" => leaf(Test::Regex(build_regex(value, case)?)),
            "ext" => {
                let exts = value.split([';', ',']).map(|x| fold(x.trim().trim_start_matches('.'))).collect();
                leaf(Test::Ext(exts))
            }
            "size" => {
                let (lo, hi) = parse_size(value).ok_or_else(|| format!("can't read size {value:?}"))?;
                leaf(Test::Size(lo, hi))
            }
            "dm" | "datemodified" => {
                let (lo, hi) = parse_date(value, self.now).ok_or_else(|| format!("can't read date {value:?}"))?;
                leaf(Test::Date(lo, hi))
            }
            "parent" | "infolder" => {
                let id = self.index.and_then(|ix| ix.lookup(Path::new(value.trim_end_matches(['/', '\\']).trim())));
                let id = id.or_else(|| self.index.and_then(|ix| ix.lookup(Path::new(value))));
                leaf(Test::Parent(id))
            }
            "startwith" | "endwith" => {
                if !path && self.negated == 0 {
                    self.highlights.push(value.to_lowercase());
                }
                let needle = if case { value.as_bytes().to_vec() } else { fold(value) };
                let lit = (!path && (case || value.is_ascii())).then(|| (needle.clone(), case));
                let test = if f == "startwith" { Test::Prefix { needle, case } } else { Test::Suffix { needle, case } };
                Ok(Leaf { test, path, lit })
            }
            _ => unreachable!(),
        }
    }
}

fn build_regex(pat: &str, case: bool) -> Result<Regex, String> {
    RegexBuilder::new(pat)
        .case_insensitive(!case)
        .size_limit(1 << 22)
        .build()
        .map_err(|e| format!("bad regex: {}", e.to_string().lines().last().unwrap_or("")))
}

const KB: u64 = 1024;
const MB: u64 = KB * 1024;
const GB: u64 = MB * 1024;
const TB: u64 = GB * 1024;

/// One size with an optional unit, plus the unit's size (for `size:1mb` meaning "about 1 MB").
fn size_value(s: &str) -> Option<(u64, u64)> {
    let s = s.trim().to_ascii_lowercase();
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: f64 = num.parse().ok()?;
    let mult = match unit.trim() {
        "" | "b" | "bytes" => 1,
        "k" | "kb" | "kib" => KB,
        "m" | "mb" | "mib" => MB,
        "g" | "gb" | "gib" => GB,
        "t" | "tb" | "tib" => TB,
        _ => return None,
    };
    Some(((n * mult as f64).round() as u64, mult))
}

/// `size:` values as a half-open byte range.
pub fn parse_size(v: &str) -> Option<(u64, u64)> {
    let v = v.trim().to_ascii_lowercase();
    let named = match v.as_str() {
        "empty" => Some((0, 1)),
        "tiny" => Some((0, 10 * KB + 1)),
        "small" => Some((10 * KB + 1, 100 * KB + 1)),
        "medium" => Some((100 * KB + 1, MB + 1)),
        "large" => Some((MB + 1, 16 * MB + 1)),
        "huge" => Some((16 * MB + 1, 128 * MB + 1)),
        "gigantic" => Some((128 * MB + 1, u64::MAX)),
        _ => None,
    };
    if named.is_some() {
        return named;
    }
    if let Some((a, b)) = v.split_once("..").or_else(|| v.split_once('-')) {
        let lo = if a.trim().is_empty() { 0 } else { size_value(a)?.0 };
        let hi = if b.trim().is_empty() { u64::MAX } else { size_value(b)?.0.saturating_add(1) };
        return Some((lo, hi));
    }
    for (op, f) in [
        (">=", (|x: u64, _u: u64| (x, u64::MAX)) as fn(u64, u64) -> (u64, u64)),
        ("<=", |x, _| (0, x.saturating_add(1))),
        (">", |x, _| (x.saturating_add(1), u64::MAX)),
        ("<", |x, _| (0, x)),
        ("=", |x, u| (x, x.saturating_add(u))),
    ] {
        if let Some(rest) = v.strip_prefix(op) {
            let (x, u) = size_value(rest)?;
            return Some(f(x, u));
        }
    }
    let (x, u) = size_value(&v)?;
    Some((x, x.saturating_add(u)))
}

fn local_ts(d: NaiveDate) -> i64 {
    let ndt = d.and_hms_opt(0, 0, 0).unwrap();
    match Local.from_local_datetime(&ndt).earliest() {
        Some(t) => t.timestamp(),
        None => ndt.and_utc().timestamp(),
    }
}

/// A date or named period as a half-open day range.
fn date_period(s: &str, now: DateTime<Local>) -> Option<(NaiveDate, NaiveDate)> {
    let today = now.date_naive();
    let week_start = today - TimeDelta::days(today.weekday().num_days_from_monday() as i64);
    let month_start = today.with_day(1)?;
    let year_start = NaiveDate::from_ymd_opt(today.year(), 1, 1)?;
    let day = TimeDelta::days(1);
    Some(match s {
        "today" => (today, today + day),
        "yesterday" => (today - day, today),
        "thisweek" => (week_start, week_start + TimeDelta::days(7)),
        "lastweek" => (week_start - TimeDelta::days(7), week_start),
        "thismonth" => (month_start, month_start.checked_add_months(Months::new(1))?),
        "lastmonth" => (month_start.checked_sub_months(Months::new(1))?, month_start),
        "thisyear" => (year_start, year_start.checked_add_months(Months::new(12))?),
        "lastyear" => (year_start.checked_sub_months(Months::new(12))?, year_start),
        _ => {
            let parts: Vec<&str> = s.split(['-', '/', '.']).collect();
            let nums: Option<Vec<u32>> = parts.iter().map(|p| p.parse().ok()).collect();
            let nums = nums?;
            match nums.as_slice() {
                [y] if parts[0].len() == 4 => {
                    let a = NaiveDate::from_ymd_opt(*y as i32, 1, 1)?;
                    (a, a.checked_add_months(Months::new(12))?)
                }
                [y, m] if parts[0].len() == 4 => {
                    let a = NaiveDate::from_ymd_opt(*y as i32, *m, 1)?;
                    (a, a.checked_add_months(Months::new(1))?)
                }
                [y, m, d] if parts[0].len() == 4 => {
                    let a = NaiveDate::from_ymd_opt(*y as i32, *m, *d)?;
                    (a, a + day)
                }
                _ => return None,
            }
        }
    })
}

/// `last3days`, `past2weeks`, `last1hour`: from that long ago until now (and later).
fn relative(s: &str, now: DateTime<Local>) -> Option<i64> {
    let rest = s.strip_prefix("last").or_else(|| s.strip_prefix("past"))?;
    let split = rest.find(|c: char| !c.is_ascii_digit())?;
    let (num, unit) = rest.split_at(split);
    let n: i64 = if num.is_empty() { 1 } else { num.parse().ok()? };
    let unit = unit.trim_end_matches('s');
    let secs = match unit {
        "sec" | "second" => 1,
        "min" | "minute" => 60,
        "hour" => 3600,
        "day" => 86400,
        "week" => 7 * 86400,
        "month" => 30 * 86400,
        "year" => 365 * 86400,
        _ => return None,
    };
    if num.is_empty() {
        return None;
    }
    Some(now.timestamp() - n * secs)
}

/// `dm:` values as a half-open range of Unix seconds.
pub fn parse_date(v: &str, now: DateTime<Local>) -> Option<(i64, i64)> {
    let v = v.trim().to_ascii_lowercase();
    if let Some(start) = relative(&v, now) {
        return Some((start, i64::MAX));
    }
    if let Some((a, b)) = v.split_once("..") {
        let lo = if a.is_empty() { i64::MIN } else { local_ts(date_period(a, now)?.0) };
        let hi = if b.is_empty() { i64::MAX } else { local_ts(date_period(b, now)?.1) };
        return Some((lo, hi));
    }
    for op in [">=", "<=", ">", "<", "="] {
        if let Some(rest) = v.strip_prefix(op) {
            let (a, b) = date_period(rest, now)?;
            let (a, b) = (local_ts(a), local_ts(b));
            return Some(match op {
                ">=" => (a, i64::MAX),
                "<=" => (i64::MIN, b),
                ">" => (b, i64::MAX),
                "<" => (i64::MIN, a),
                _ => (a, b),
            });
        }
    }
    let (a, b) = date_period(&v, now)?;
    Some((local_ts(a), local_ts(b)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{DIR, Meta};

    fn ix() -> Index {
        let mut ix = Index::new();
        let root = ix.add_root(ROOT, Meta::default());
        let music = ix.add(root, "Music", Meta { flags: DIR, ..Meta::default() });
        ix.add(music, "Song One.mp3", Meta { size: 5 * MB, mtime: 1_700_000_000, ..Meta::default() });
        ix.add(music, "song_two.FLAC", Meta { size: 30 * MB, mtime: 1_600_000_000, ..Meta::default() });
        let docs = ix.add(root, "Docs", Meta { flags: DIR, ..Meta::default() });
        ix.add(docs, "Report 2026.pdf", Meta { size: 200 * KB, ..Meta::default() });
        ix.add(docs, "Größe.txt", Meta { size: 10, ..Meta::default() });
        ix.add(docs, "notes", Meta { size: 0, ..Meta::default() });
        ix.add(root, ".hidden", Meta { flags: crate::index::HIDDEN, ..Meta::default() });
        ix.finish_bulk();
        ix
    }

    const ROOT: &str = if cfg!(windows) { "C:\\" } else { "/" };

    fn names(ix: &Index, q: &str) -> Vec<String> {
        names_opts(ix, q, Options::default())
    }

    fn names_opts(ix: &Index, q: &str, opts: Options) -> Vec<String> {
        let query = Query::parse(q, opts, Some(ix)).unwrap();
        let mut s = Scratch::default();
        ix.by_name().iter().filter(|&&id| query.matches(ix, id, &mut s)).map(|&id| ix.name(id).to_string()).collect()
    }

    #[test]
    fn lexing() {
        assert_eq!(lex("a b"), vec![Tok::Word("a".into()), Tok::Word("b".into())]);
        assert_eq!(lex("a|b"), vec![Tok::Word("a".into()), Tok::Or, Tok::Word("b".into())]);
        assert_eq!(lex("size:>1mb"), vec![Tok::Word("size:>1mb".into())]);
        assert_eq!(
            lex("<size:>=1mb|x>"),
            vec![Tok::Open, Tok::Word("size:>=1mb".into()), Tok::Or, Tok::Word("x".into()), Tok::Close]
        );
        assert_eq!(lex("\"a b\" !c"), vec![Tok::Word("a b".into()), Tok::Not, Tok::Word("c".into())]);
        assert_eq!(lex("path:\"C:\\Program Files\""), vec![Tok::Word("path:C:\\Program Files".into())]);
    }

    #[test]
    fn boolean_logic() {
        assert_eq!(names(&ix(), "song"), vec!["Song One.mp3", "song_two.FLAC"]);
        assert_eq!(names(&ix(), "song one"), vec!["Song One.mp3"]);
        assert_eq!(names(&ix(), "song !one"), vec!["song_two.FLAC"]);
        assert_eq!(names(&ix(), "report|notes"), vec!["notes", "Report 2026.pdf"]);
        assert_eq!(names(&ix(), "<report|notes> !pdf"), vec!["notes"]);
        assert_eq!(names(&ix(), "song OR report NOT one").len(), 2);
        assert_eq!(names(&ix(), "").len(), 9);
    }

    #[test]
    fn modifiers_and_wildcards() {
        assert_eq!(names(&ix(), "case:Song"), vec!["Song One.mp3"]);
        assert_eq!(names(&ix(), "*.mp3"), vec!["Song One.mp3"]);
        assert_eq!(names(&ix(), "song*"), vec!["Song One.mp3", "song_two.FLAC"]);
        assert_eq!(names(&ix(), "s?ng_*"), vec!["song_two.FLAC"]);
        assert_eq!(names(&ix(), "folder:"), vec![ROOT, "Docs", "Music"]);
        assert_eq!(names(&ix(), "file: o"), vec!["notes", "Report 2026.pdf", "Song One.mp3", "song_two.FLAC"]);
        assert_eq!(names(&ix(), "ww:one"), vec!["Song One.mp3"]);
        assert_eq!(names(&ix(), "ww:on"), Vec::<String>::new());
        assert_eq!(names(&ix(), "GRÖßE"), vec!["Größe.txt"]);
        assert_eq!(names(&ix(), "größe"), vec!["Größe.txt"]);
        assert_eq!(names(&ix(), "regex:^song_\\w+\\.flac$"), vec!["song_two.FLAC"]);
        assert_eq!(names(&ix(), "startwith:rep"), vec!["Report 2026.pdf"]);
        assert_eq!(names(&ix(), "endwith:.txt"), vec!["Größe.txt"]);
    }

    #[test]
    fn paths() {
        let sep = if cfg!(windows) { "\\" } else { "/" };
        assert_eq!(names(&ix(), &format!("music{sep}song")), vec!["Song One.mp3", "song_two.FLAC"]);
        assert_eq!(names(&ix(), "path:docs notes"), vec!["notes"]);
        assert_eq!(names(&ix(), "c/song_"), vec!["song_two.FLAC"]);
        let root = if cfg!(windows) { "C:\\Docs" } else { "/Docs" };
        assert_eq!(names(&ix(), &format!("parent:\"{root}\"")), vec!["Größe.txt", "notes", "Report 2026.pdf"]);
        assert_eq!(names_opts(&ix(), "docs", Options { match_path: true, ..Options::default() }).len(), 4);
    }

    #[test]
    fn functions() {
        assert_eq!(names(&ix(), "ext:mp3;flac"), vec!["Song One.mp3", "song_two.FLAC"]);
        assert_eq!(names(&ix(), "ext:"), vec![ROOT, "Docs", "Music", "notes"]);
        assert_eq!(names(&ix(), "size:>10mb"), vec!["song_two.FLAC"]);
        assert_eq!(names(&ix(), "size:1mb..10mb"), vec!["Song One.mp3"]);
        assert_eq!(names(&ix(), "size:empty"), vec![".hidden", "notes"]);
        assert_eq!(names(&ix(), "size:<1kb file:").len(), 3);
        assert_eq!(names(&ix(), "folder: size:>1mb"), vec![ROOT, "Music"]);
        let old = names(&ix(), "dm:<2021");
        assert!(old.contains(&"song_two.FLAC".to_string()) && !old.contains(&"Song One.mp3".to_string()));
        assert_eq!(names(&ix(), "dm:2023 file:"), vec!["Song One.mp3"]);
        assert_eq!(names(&ix(), "dm:2020-09-01..2020-09-30"), vec!["song_two.FLAC"]);
        assert!(Query::parse("size:lots", Options::default(), None).is_err());
        assert!(Query::parse("regex:(", Options::default(), None).is_err());
    }

    #[test]
    fn hidden() {
        let opts = Options { show_hidden: false, ..Options::default() };
        assert!(!names_opts(&ix(), "", opts).contains(&".hidden".to_string()));
        assert!(names(&ix(), "").contains(&".hidden".to_string()));
    }

    #[test]
    fn sizes_and_dates() {
        assert_eq!(parse_size(">1kb"), Some((1025, u64::MAX)));
        assert_eq!(parse_size("1.5mb"), Some((1572864, 1572864 + MB)));
        assert_eq!(parse_size("..2kb"), Some((0, 2049)));
        let now = Local.with_ymd_and_hms(2026, 10, 17, 12, 0, 0).unwrap();
        let (a, b) = parse_date("today", now).unwrap();
        assert_eq!(b - a, 86400);
        let (a, _) = parse_date("last2days", now).unwrap();
        assert_eq!(now.timestamp() - a, 2 * 86400);
        let (a, b) = parse_date("thisweek", now).unwrap();
        assert_eq!(a, local_ts(NaiveDate::from_ymd_opt(2026, 10, 12).unwrap()));
        assert_eq!(b, local_ts(NaiveDate::from_ymd_opt(2026, 10, 19).unwrap()));
        assert_eq!(parse_date(">2026-01-01", now).unwrap().0, local_ts(NaiveDate::from_ymd_opt(2026, 1, 2).unwrap()));
        assert!(parse_date("someday", now).is_none());
    }

    #[test]
    fn prefilters() {
        let lit =
            |q: &str| Query::parse(q, Options::default(), None).unwrap().prefilter().map(|(b, c)| (b.to_vec(), c));
        assert_eq!(lit("Report"), Some((b"report".to_vec(), false)));
        assert_eq!(lit("case:Report x"), Some((b"Report".to_vec(), true)));
        assert_eq!(lit("*.dll"), Some((b".dll".to_vec(), false)));
        assert_eq!(lit("a|b"), None);
        assert_eq!(lit("!a"), None);
        assert_eq!(lit("path:x"), None);
        assert_eq!(lit("ab file:abc"), Some((b"abc".to_vec(), false)));
    }

    #[test]
    fn highlights() {
        let q = Query::parse("Song !one *.mp3 path:x", Options::default(), None).unwrap();
        assert_eq!(q.highlights, vec!["song", ".mp3"]);
    }
}
