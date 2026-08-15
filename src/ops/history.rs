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

/// Whether every package in an entry can be restored from cache.
///
/// Checked *before* offering undo: promising a restore and then failing partway
/// through is worse than saying up front that it is unavailable.
pub fn can_undo(entry: &Entry) -> bool {
    !entry.packages.is_empty()
        && entry
            .packages
            .iter()
            .all(|(n, v)| cached_package(n, v).is_some())
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
    fn an_empty_entry_is_never_undoable() {
        let mut e = entry();
        e.packages.clear();
        assert!(!can_undo(&e));
    }
}
