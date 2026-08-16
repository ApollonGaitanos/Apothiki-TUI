//! Transaction log and undo (spec §6.5).
//!
//! Appended to `$XDG_DATA_HOME/apothiki/history.jsonl`, one JSON object per
//! line, so a partial write can only ever damage the last entry.
//!
//! Undo is possible because `/var/cache/pacman/pkg/` keeps the `.pkg.tar.zst`
//! files: reinstalling an exact version offline is usually available. *Usually*
//! is the operative word — `paccache` prunes it — so the cache is checked before
//! the offer is made rather than failing halfway through.

use std::io::Write;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Entry {
    pub timestamp: i64,
    pub operation: String,
    /// Exact `name-version` pairs, which is what an offline reinstall needs.
    pub packages: Vec<(String, String)>,
    pub success: bool,
    pub snapshot: Option<String>,
}

pub fn history_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
    Some(base.join("apothiki").join("history.jsonl"))
}

/// Minimal JSON string escaping. A whole serialisation stack is not warranted
/// for four fields, but unescaped quotes in a package description would corrupt
/// the log silently.
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

impl Entry {
    pub fn to_json(&self) -> String {
        let pkgs: Vec<String> = self
            .packages
            .iter()
            .map(|(n, v)| format!("{{\"name\":\"{}\",\"version\":\"{}\"}}", esc(n), esc(v)))
            .collect();
        format!(
            "{{\"timestamp\":{},\"operation\":\"{}\",\"success\":{},\"snapshot\":{},\"packages\":[{}]}}",
            self.timestamp,
            esc(&self.operation),
            self.success,
            match &self.snapshot {
                Some(s) => format!("\"{}\"", esc(s)),
                None => "null".to_string(),
            },
            pkgs.join(",")
        )
    }
}

/// Appends an entry. Failure to log is reported but never blocks or reverses an
/// operation that already happened.
pub fn record(entry: &Entry) -> anyhow::Result<()> {
    let Some(path) = history_path() else {
        anyhow::bail!("no data directory available");
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    writeln!(f, "{}", entry.to_json())?;
    Ok(())
}

/// Reads every logged transaction, oldest first.
///
/// Malformed lines are skipped rather than failing the read: the log is
/// append-only and a crash mid-write can only damage the last line, which must
/// not make the whole history unreadable.
pub fn read_all() -> anyhow::Result<Vec<Entry>> {
    let Some(path) = history_path() else {
        anyhow::bail!("no data directory available");
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    Ok(text.lines().filter_map(parse_line).collect())
}

/// Parses one log line.
///
/// Hand-rolled rather than pulling in a JSON parser for four fields, matching
/// the writer. Deliberately forgiving: anything it cannot understand is skipped.
fn parse_line(line: &str) -> Option<Entry> {
    let field = |key: &str| -> Option<&str> {
        let at = line.find(&format!("\"{key}\":"))? + key.len() + 3;
        Some(line[at..].trim_start())
    };

    let timestamp = field("timestamp")?
        .split(|c: char| !c.is_ascii_digit() && c != '-')
        .next()?
        .parse()
        .ok()?;
    let operation = field("operation")?
        .strip_prefix('"')?
        .split('"')
        .next()?
        .to_string();
    let success = field("success")?.starts_with("true");
    let snapshot = field("snapshot")
        .filter(|v| v.starts_with('"'))
        .and_then(|v| v.strip_prefix('"'))
        .and_then(|v| v.split('"').next())
        .map(|s| s.to_string());

    // Packages are `{"name":"x","version":"y"}` objects in an array.
    let mut packages = Vec::new();
    let mut rest = line;
    while let Some(i) = rest.find("{\"name\":\"") {
        rest = &rest[i + 9..];
        let Some(name) = rest.split('"').next() else {
            break;
        };
        let Some(vi) = rest.find("\"version\":\"") else {
            break;
        };
        // `"version":"` is 11 bytes, not 12 — an off-by-one here silently
        // truncates the first digit of every version, which then fails to match
        // any cached package file.
        let after = &rest[vi + 11..];
        let Some(version) = after.split('"').next() else {
            break;
        };
        packages.push((name.to_string(), version.to_string()));
        rest = after;
    }

    Some(Entry {
        timestamp,
        operation,
        packages,
        success,
        snapshot,
    })
}

/// Locates the cached package file for an exact version, if it survived
/// `paccache`.
pub fn cached_package(name: &str, version: &str) -> Option<PathBuf> {
    let dir = PathBuf::from("/var/cache/pacman/pkg");
    let entries = std::fs::read_dir(dir).ok()?;
    let prefix = format!("{name}-{version}-");
    for e in entries.flatten() {
        let f = e.file_name();
        let f = f.to_string_lossy();
        if f.starts_with(&prefix) && f.ends_with(".pkg.tar.zst") {
            return Some(e.path());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> Entry {
        Entry {
            timestamp: 1_783_277_044,
            operation: "-Rs".into(),
            packages: vec![("godot".into(), "4.7.1-1.1".into())],
            success: true,
            snapshot: Some("42".into()),
        }
    }

    #[test]
    fn serialises_to_one_json_line() {
        let j = entry().to_json();
        assert!(j.starts_with('{') && j.ends_with('}'));
        assert!(!j.contains('\n'), "must stay on a single line");
        assert!(j.contains("\"name\":\"godot\""));
        assert!(j.contains("\"version\":\"4.7.1-1.1\""));
        assert!(j.contains("\"snapshot\":\"42\""));
    }

    #[test]
    fn absent_snapshot_serialises_as_null() {
        let mut e = entry();
        e.snapshot = None;
        assert!(e.to_json().contains("\"snapshot\":null"));
    }

    #[test]
    fn quotes_and_control_characters_cannot_corrupt_the_log() {
        let mut e = entry();
        e.operation = "weird \"quoted\"\nnewline\ttab".into();
        let j = e.to_json();
        assert!(!j.contains('\n'));
        assert!(j.contains("\\\"quoted\\\""));
        assert!(j.contains("\\n"));
    }

    #[test]
    fn a_written_entry_reads_back_identically() {
        let e = entry();
        let back = parse_line(&e.to_json()).expect("should parse");
        assert_eq!(back.timestamp, e.timestamp);
        assert_eq!(back.operation, e.operation);
        assert_eq!(back.success, e.success);
        assert_eq!(back.snapshot, e.snapshot);
        assert_eq!(back.packages, e.packages);
    }

    #[test]
    fn multiple_packages_round_trip() {
        let mut e = entry();
        e.packages = vec![
            ("godot".into(), "4.7.1-1.1".into()),
            ("embree".into(), "4.4.1-1.1".into()),
        ];
        let back = parse_line(&e.to_json()).unwrap();
        assert_eq!(back.packages, e.packages);
    }

    #[test]
    fn a_failed_entry_reads_back_as_failed() {
        let mut e = entry();
        e.success = false;
        e.snapshot = None;
        let back = parse_line(&e.to_json()).unwrap();
        assert!(!back.success);
        assert_eq!(back.snapshot, None);
    }

    #[test]
    fn garbage_lines_are_skipped_not_fatal() {
        assert!(parse_line("").is_none());
        assert!(parse_line("{not json").is_none());
        assert!(parse_line("{\"timestamp\":1}").is_none());
    }
}
