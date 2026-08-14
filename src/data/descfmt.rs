//! The `desc` container format, shared by the local database and the sync
//! databases.
//!
//! A document is a sequence of `%KEY%` lines, each followed by one value per
//! line, terminated by a blank line. Both `/var/lib/pacman/local/*/desc` and the
//! `desc` members inside `/var/lib/pacman/sync/*.db` use it, so it is parsed in
//! exactly one place.

use std::collections::HashMap;

pub type Sections<'a> = HashMap<&'a str, Vec<&'a str>>;

/// Splits a `desc` document into its `%KEY%` → values mapping.
///
/// Tolerant by design: unknown keys are kept, a missing trailing blank line is
/// fine, CRLF is accepted, and text before the first key is ignored. One
/// malformed entry must never abort a scan of 1600 others.
pub fn parse_sections(text: &str) -> Sections<'_> {
    let mut out: Sections = HashMap::new();
    let mut current: Option<&str> = None;

    for line in text.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            current = None;
        } else if line.len() >= 2 && line.starts_with('%') && line.ends_with('%') {
            let key = &line[1..line.len() - 1];
            current = Some(key);
            out.entry(key).or_default();
        } else if let Some(key) = current {
            out.entry(key).or_default().push(line);
        }
    }
    out
}

pub fn one(m: &Sections, key: &str) -> Option<String> {
    m.get(key)?.first().map(|s| s.to_string())
}

pub fn many(m: &Sections, key: &str) -> Vec<String> {
    m.get(key)
        .map(|v| v.iter().map(|s| s.to_string()).collect())
        .unwrap_or_default()
}

pub fn deps(m: &Sections, key: &str) -> Vec<crate::data::dep::Dep> {
    m.get(key)
        .map(|v| v.iter().map(|s| crate::data::dep::Dep::parse(s)).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_keys_and_multi_values() {
        let m = parse_sections("%NAME%\nfoo\n\n%GROUPS%\na\nb\n\n");
        assert_eq!(m["NAME"], ["foo"]);
        assert_eq!(m["GROUPS"], ["a", "b"]);
    }

    #[test]
    fn key_present_with_no_values_is_still_recorded() {
        // `%REASON%` presence alone is meaningful, so an empty section must not
        // vanish.
        let m = parse_sections("%REASON%\n\n");
        assert!(m.contains_key("REASON"));
        assert!(m["REASON"].is_empty());
    }

    #[test]
    fn ignores_leading_junk_and_handles_crlf() {
        let m = parse_sections("junk\r\n%NAME%\r\nfoo\r\n");
        assert_eq!(m["NAME"], ["foo"]);
    }
}
