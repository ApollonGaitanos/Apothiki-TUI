//! Layer 4: Flatpak (spec §4.2).
//!
//! The easy half of Layer 4 — unlike AppImages, Flatpak has a real registry and
//! a stable CLI. We drive it rather than reading `/var/lib/flatpak` ourselves,
//! because the CLI is the supported interface and the on-disk layout is not.
//!
//! Flatpak apps already appear in the `.desktop` scan (their exports live in
//! `XDG_DATA_DIRS`), so this module's job is not discovery but *attribution*:
//! marking those entries as Flatpak-owned rather than "no pacman package owns
//! this", and attaching the size and origin only Flatpak knows.

use std::collections::HashMap;
use std::process::Command;

#[derive(Debug, Clone)]
pub struct FlatpakApp {
    /// Application id, e.g. `app.zen_browser.zen`. Matches the exported
    /// `.desktop` file's basename.
    pub id: String,
    pub name: String,
    /// Human-readable size as Flatpak reports it (e.g. `395,7 MB`). Kept as
    /// text: the CLI localises the decimal separator, so parsing it into bytes
    /// would be wrong in exactly the locales where it looks parseable.
    pub size: String,
    /// Remote it came from, e.g. `flathub`.
    pub origin: String,
}

/// Lists installed Flatpak applications.
///
/// Returns an empty list when Flatpak is absent or the call fails — an optional
/// subsystem must never be able to take down the catalog.
pub fn list() -> Vec<FlatpakApp> {
    // `--app` excludes runtimes, which are Flatpak's equivalent of dependencies
    // and would swamp the list.
    let out = Command::new("flatpak")
        .args([
            "list",
            "--app",
            "--columns=application,name,size,origin",
        ])
        .output();

    let Ok(out) = out else { return Vec::new() };
    if !out.status.success() {
        return Vec::new();
    }

    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(parse_line)
        .collect()
}

fn parse_line(line: &str) -> Option<FlatpakApp> {
    let mut cols = line.split('\t');
    let id = cols.next()?.trim().to_string();
    if id.is_empty() {
        return None;
    }
    Some(FlatpakApp {
        name: cols.next().unwrap_or("").trim().to_string(),
        size: cols.next().unwrap_or("").trim().to_string(),
        origin: cols.next().unwrap_or("").trim().to_string(),
        id,
    })
}

/// Indexes Flatpak apps by the desktop file id they export, so Layer 2 entries
/// can be attributed without a second scan.
pub fn by_desktop_id(apps: &[FlatpakApp]) -> HashMap<String, &FlatpakApp> {
    apps.iter()
        .map(|a| (format!("{}.desktop", a.id), a))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_column_output() {
        let a = parse_line("app.zen_browser.zen\tZen\t395,7 MB\tflathub").unwrap();
        assert_eq!(a.id, "app.zen_browser.zen");
        assert_eq!(a.name, "Zen");
        // Localised decimal separator preserved verbatim rather than misparsed.
        assert_eq!(a.size, "395,7 MB");
        assert_eq!(a.origin, "flathub");
    }

    #[test]
    fn tolerates_short_and_empty_rows() {
        assert!(parse_line("").is_none());
        assert!(parse_line("\t\t").is_none());
        let a = parse_line("com.example.App").unwrap();
        assert_eq!(a.id, "com.example.App");
        assert!(a.name.is_empty());
    }

    #[test]
    fn desktop_ids_match_the_exported_filename() {
        let apps = vec![FlatpakApp {
            id: "it.mijorus.gearlever".into(),
            name: "Gear Lever".into(),
            size: "22,2 MB".into(),
            origin: "flathub".into(),
        }];
        let map = by_desktop_id(&apps);
        assert!(map.contains_key("it.mijorus.gearlever.desktop"));
    }
}
