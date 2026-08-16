//! Layer 2: the `.desktop` scan (spec §4.2).
//!
//! This is the workhorse of app discovery. It is also exactly what KDE's
//! `kbuildsycoca6` and GNOME's `GAppInfo` do — same paths, same filters — which
//! is why the user's own application launcher is the correctness oracle for
//! this module. Anything in their start menu that is missing here is a bug in
//! our filters.
//!
//! The filters are not optional polish: on the dev machine **139 of 232** system
//! entries are `NoDisplay=true`. Skipping that one rule alone would more than
//! double the app list with entries no launcher ever shows.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

// Mirrors the on-disk record rather than only the parts currently consumed.
// Dropping a field would mean the parser silently discards it, and the next
// reader of this struct would have no way to tell that a `.desktop` file carries it at all.
#[allow(dead_code)]
/// A parsed `[Desktop Entry]` group.
///
/// Only the keys app discovery needs are lifted into fields; the rest stay in
/// `raw` so the detail pane can show them without a second parse.
#[derive(Debug, Clone, Default)]
pub struct DesktopEntry {
    /// The desktop file id, e.g. `org.kde.filelight.desktop`. Subdirectories
    /// become dashes, per the XDG spec, so `kde/foo.desktop` is `kde-foo.desktop`.
    pub id: String,
    pub path: PathBuf,
    pub entry_type: Option<String>,
    pub name: Option<String>,
    pub generic_name: Option<String>,
    pub comment: Option<String>,
    pub exec: Option<String>,
    pub try_exec: Option<String>,
    pub icon: Option<String>,
    pub categories: Vec<String>,
    pub keywords: Vec<String>,
    pub only_show_in: Vec<String>,
    pub not_show_in: Vec<String>,
    pub no_display: bool,
    pub hidden: bool,
    pub terminal: bool,
    pub raw: HashMap<String, String>,
}

/// Why an entry was excluded, kept so the UI can explain any absence rather
/// than leaving the user wondering where a program went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejection {
    NotAnApplication,
    NoDisplay,
    /// `Hidden=true` is a tombstone: the user deleted this entry.
    Hidden,
    /// `TryExec` names a binary that is not in PATH — how packages ship entries
    /// for components that may not be installed. Every launcher hides these.
    TryExecMissing(String),
    NotShownInThisDesktop,
    Noise(String),
    NoName,
}

/// The outcome of classifying one entry.
#[derive(Debug, Clone)]
pub enum Classified {
    /// A launchable application.
    App(DesktopEntry),
    /// `Terminal=true` — a CLI program that ships a desktop entry. Routed to
    /// Tools rather than Apps (spec §4.2).
    Tool(DesktopEntry),
    Rejected(DesktopEntry, Rejection),
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('s') => out.push(' '),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Splits a `;`-terminated list, honouring `\;` escapes.
fn split_list(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut escaped = false;
    for c in s.chars() {
        if escaped {
            if c != ';' {
                cur.push('\\');
            }
            cur.push(c);
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == ';' {
            if !cur.is_empty() {
                out.push(unescape(&cur));
            }
            cur.clear();
        } else {
            cur.push(c);
        }
    }
    if !cur.is_empty() {
        out.push(unescape(&cur));
    }
    out
}

/// The locale suffixes to prefer for localised keys, best first.
///
/// `el_GR.UTF-8` yields `el_GR` then `el`, so a file offering only `Name[el]`
/// still localises.
fn locale_candidates() -> Vec<String> {
    let raw = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|k| std::env::var(k).ok())
        .unwrap_or_default();
    let base = raw.split('.').next().unwrap_or("").trim().to_string();
    if base.is_empty() || base == "C" || base == "POSIX" {
        return Vec::new();
    }
    let mut out = vec![base.clone()];
    if let Some((lang, _)) = base.split_once('_') {
        out.push(lang.to_string());
    }
    out
}

/// Parses the `[Desktop Entry]` group of a desktop file.
///
/// Other groups (`[Desktop Action ...]`) are ignored: they describe context-menu
/// actions, not the application itself.
pub fn parse(text: &str, id: &str, path: &Path) -> DesktopEntry {
    let locales = locale_candidates();
    let mut raw: HashMap<String, String> = HashMap::new();
    let mut localised: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut in_group = false;

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            in_group = line == "[Desktop Entry]";
            continue;
        }
        if !in_group {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());

        match key.split_once('[') {
            Some((base, rest)) => {
                let locale = rest.trim_end_matches(']');
                localised
                    .entry(base.trim().to_string())
                    .or_default()
                    .insert(locale.to_string(), value.to_string());
            }
            None => {
                raw.insert(key.to_string(), value.to_string());
            }
        }
    }

    // Prefer the best-matching locale, falling back to the unlocalised value.
    let get = |key: &str| -> Option<String> {
        for loc in &locales {
            if let Some(v) = localised.get(key).and_then(|m| m.get(loc)) {
                return Some(unescape(v));
            }
        }
        raw.get(key).map(|v| unescape(v))
    };
    let flag = |key: &str| raw.get(key).map(|v| v == "true").unwrap_or(false);

    DesktopEntry {
        id: id.to_string(),
        path: path.to_path_buf(),
        entry_type: raw.get("Type").cloned(),
        name: get("Name"),
        generic_name: get("GenericName"),
        comment: get("Comment"),
        exec: raw.get("Exec").map(|v| unescape(v)),
        try_exec: raw.get("TryExec").map(|v| unescape(v)),
        icon: raw.get("Icon").cloned(),
        categories: raw.get("Categories").map(|v| split_list(v)).unwrap_or_default(),
        keywords: get("Keywords").map(|v| split_list(&v)).unwrap_or_default(),
        only_show_in: raw.get("OnlyShowIn").map(|v| split_list(v)).unwrap_or_default(),
        not_show_in: raw.get("NotShowIn").map(|v| split_list(v)).unwrap_or_default(),
        no_display: flag("NoDisplay"),
        hidden: flag("Hidden"),
        terminal: flag("Terminal"),
        raw,
    }
}

/// The desktops this session identifies as.
///
/// `$XDG_CURRENT_DESKTOP` is a **colon-separated list**, not one value: Cinnamon
/// reports `X-Cinnamon`, Ubuntu's GNOME reports `ubuntu:GNOME`. Matching only
/// the whole string silently hides entries.
pub fn current_desktops() -> Vec<String> {
    std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .split(':')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

fn in_path(binary: &str) -> bool {
    // An absolute TryExec is checked directly.
    if binary.contains('/') {
        return Path::new(binary).exists();
    }
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|d| d.join(binary).exists()))
        .unwrap_or(false)
}

/// Applies the mandatory filter table from spec §4.2.
pub fn classify(entry: DesktopEntry, desktops: &[String], noise: &[String]) -> Classified {
    if entry.entry_type.as_deref() != Some("Application") {
        return Classified::Rejected(entry, Rejection::NotAnApplication);
    }
    if entry.hidden {
        return Classified::Rejected(entry, Rejection::Hidden);
    }
    if entry.no_display {
        return Classified::Rejected(entry, Rejection::NoDisplay);
    }
    if entry.name.is_none() {
        return Classified::Rejected(entry, Rejection::NoName);
    }
    if let Some(pattern) = noise.iter().find(|p| glob_match(p, &entry.id)) {
        let p = pattern.clone();
        return Classified::Rejected(entry, Rejection::Noise(p));
    }
    if let Some(bin) = entry.try_exec.clone() {
        if !in_path(&bin) {
            return Classified::Rejected(entry, Rejection::TryExecMissing(bin));
        }
    }
    if !entry.not_show_in.is_empty() && entry.not_show_in.iter().any(|d| desktops.contains(d)) {
        return Classified::Rejected(entry, Rejection::NotShownInThisDesktop);
    }
    // `OnlyShowIn` deliberately does *not* hard-skip. Under a tiling WM
    // $XDG_CURRENT_DESKTOP is often unset or unrecognised, and hiding on an
    // unknown desktop would silently drop half the list (spec §4.2a). These are
    // de-prioritised at presentation time instead.

    if entry.terminal {
        return Classified::Tool(entry);
    }
    Classified::App(entry)
}

/// Minimal `*` globbing, enough for the noise denylist (`org.kde.kwin.*`).
fn glob_match(pattern: &str, text: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == text,
        Some((pre, post)) => {
            text.len() >= pre.len() + post.len()
                && text.starts_with(pre)
                && text.ends_with(post)
        }
    }
}

/// The directories to scan, in increasing order of precedence.
///
/// Reads `$XDG_DATA_HOME` and `$XDG_DATA_DIRS` rather than hardcoding, since
/// Plasma, Flatpak and Home-Manager all add paths — on the dev machine
/// `XDG_DATA_DIRS` already carries both Flatpak export directories. Earlier
/// entries in `XDG_DATA_DIRS` take precedence, so the list is reversed here and
/// `$XDG_DATA_HOME` placed last, letting a later scan overwrite an earlier one.
pub fn search_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();

    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_string());

    for d in data_dirs.split(':').rev() {
        if !d.is_empty() {
            dirs.push(Path::new(d).join("applications"));
        }
    }

    // Snap installs outside XDG_DATA_DIRS on some systems.
    let snap = PathBuf::from("/var/lib/snapd/desktop/applications");
    if snap.is_dir() {
        dirs.push(snap);
    }

    let home_share = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    if let Some(h) = home_share {
        dirs.push(h.join("applications"));
    }

    dirs
}

#[derive(Debug, Default)]
pub struct Scan {
    /// Launchable applications, keyed by desktop file id.
    pub apps: HashMap<String, DesktopEntry>,
    /// `Terminal=true` entries, destined for the Tools view.
    pub tools: HashMap<String, DesktopEntry>,
    /// Everything filtered out, with the reason. Retained so the UI can answer
    /// "why isn't X here?" — the tool must be able to explain itself.
    pub rejected: Vec<(DesktopEntry, Rejection)>,
}

/// Scans all XDG application directories and classifies every entry.
pub fn scan(dirs: &[PathBuf], noise: &[String]) -> Scan {
    let desktops = current_desktops();
    let mut out = Scan::default();

    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().is_none_or(|x| x != "desktop") {
                continue;
            }
            let Some(id) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
                continue;
            };
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };

            // Later directories have higher precedence and overwrite earlier
            // ones, including moving an entry between categories.
            out.apps.remove(&id);
            out.tools.remove(&id);

            match classify(parse(&text, &id, &path), &desktops, noise) {
                Classified::App(e) => {
                    out.apps.insert(id, e);
                }
                Classified::Tool(e) => {
                    out.tools.insert(id, e);
                }
                Classified::Rejected(e, why) => out.rejected.push((e, why)),
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(body: &str) -> DesktopEntry {
        parse(body, "test.desktop", Path::new("/tmp/test.desktop"))
    }

    #[test]
    fn parses_the_desktop_entry_group_only() {
        let e = entry(
            "# comment\n[Desktop Entry]\nType=Application\nName=Filelight\n\
             Comment=View disk usage\nExec=filelight %U\nIcon=filelight\n\
             Categories=Qt;KDE;System;Filesystem;\n\
             [Desktop Action new]\nName=Should Not Win\n",
        );
        assert_eq!(e.name.as_deref(), Some("Filelight"));
        assert_eq!(e.exec.as_deref(), Some("filelight %U"));
        assert_eq!(e.categories, ["Qt", "KDE", "System", "Filesystem"]);
    }

    #[test]
    fn escaped_semicolons_stay_in_one_field() {
        assert_eq!(split_list(r"a\;b;c;"), ["a;b", "c"]);
        assert_eq!(unescape(r"one\stwo"), "one two");
    }

    #[test]
    fn nodisplay_and_hidden_are_filtered() {
        // The single most important filter: 139 of 232 system entries on the
        // dev machine set NoDisplay.
        let d = current_desktops();
        for (body, want) in [
            ("[Desktop Entry]\nType=Application\nName=X\nNoDisplay=true\n", Rejection::NoDisplay),
            ("[Desktop Entry]\nType=Application\nName=X\nHidden=true\n", Rejection::Hidden),
            ("[Desktop Entry]\nType=Link\nName=X\n", Rejection::NotAnApplication),
            ("[Desktop Entry]\nType=Application\n", Rejection::NoName),
        ] {
            match classify(entry(body), &d, &[]) {
                Classified::Rejected(_, why) => assert_eq!(why, want),
                other => panic!("expected rejection {want:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn terminal_entries_become_tools_not_apps() {
        let e = entry("[Desktop Entry]\nType=Application\nName=htop\nTerminal=true\n");
        assert!(matches!(
            classify(e, &current_desktops(), &[]),
            Classified::Tool(_)
        ));
    }

    #[test]
    fn unresolvable_tryexec_is_filtered() {
        let e = entry(
            "[Desktop Entry]\nType=Application\nName=X\nTryExec=/nonexistent/binary-xyz\n",
        );
        match classify(e, &current_desktops(), &[]) {
            Classified::Rejected(_, Rejection::TryExecMissing(b)) => {
                assert_eq!(b, "/nonexistent/binary-xyz")
            }
            other => panic!("expected TryExecMissing, got {other:?}"),
        }
    }

    #[test]
    fn notshowin_hides_but_onlyshowin_does_not() {
        let desktops = vec!["KDE".to_string()];

        let hidden = entry("[Desktop Entry]\nType=Application\nName=X\nNotShowIn=KDE;\n");
        assert!(matches!(
            classify(hidden, &desktops, &[]),
            Classified::Rejected(_, Rejection::NotShownInThisDesktop)
        ));

        // OnlyShowIn for another desktop must NOT hard-skip: under a tiling WM
        // XDG_CURRENT_DESKTOP is often unset, and hiding would drop half the list.
        let other = entry("[Desktop Entry]\nType=Application\nName=X\nOnlyShowIn=GNOME;\n");
        assert!(matches!(classify(other, &desktops, &[]), Classified::App(_)));
    }

    #[test]
    fn noise_patterns_match_with_a_wildcard() {
        assert!(glob_match("org.kde.kwin.*", "org.kde.kwin.desktop"));
        assert!(glob_match("*-url-handler.desktop", "foo-url-handler.desktop"));
        assert!(!glob_match("org.kde.kwin.*", "org.kde.dolphin.desktop"));
        assert!(glob_match("exact.desktop", "exact.desktop"));

        let e = entry("[Desktop Entry]\nType=Application\nName=X\n");
        let noise = vec!["test.*".to_string()];
        assert!(matches!(
            classify(e, &current_desktops(), &noise),
            Classified::Rejected(_, Rejection::Noise(_))
        ));
    }

    #[test]
    fn xdg_data_home_takes_precedence_over_system_dirs() {
        let dirs = search_dirs();
        let home = dirs.last().unwrap().to_string_lossy().into_owned();
        assert!(home.contains(".local/share") || home.contains("XDG"), "{home}");
        // /usr/share is lowest precedence, so it must come first.
        assert!(dirs[0].starts_with("/usr/share") || dirs.len() == 1, "{dirs:?}");
    }
}
