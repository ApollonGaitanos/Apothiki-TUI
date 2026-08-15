//! Snapper pre-transaction snapshots (spec §6.4).
//!
//! The single highest-value safety feature in the product. A snapshot converts
//! every mistake from a disaster into a reboot, which is worth more than all the
//! confirmation dialogs combined. CachyOS ships btrfs + snapper by default, so
//! on the target machine this is available rather than hypothetical.
//!
//! Degrades silently when snapper is absent: this must never become a
//! requirement for using the tool.

use std::process::Command;

/// Whether snapper is installed and has at least one configuration.
///
/// Both checks matter: the binary existing without a configured subvolume means
/// snapshot creation would fail at the worst possible moment, right before a
/// removal the user believes is protected.
pub fn is_available() -> bool {
    config_name().is_some()
}

/// The snapper config covering `/`, conventionally `root`.
///
/// Uses `--machine-readable csv`. The human-readable table separates columns
/// with `│` (U+2502), **not an ASCII pipe** — splitting on `|` silently yields
/// the whole row, so the config name became the literal string `root   │ /`,
/// and the snapshot command built from it failed. Because a failed snapshot
/// aborts the removal by design, that surfaced as an unexplained error at the
/// worst possible moment: right after the user typed their password.
pub fn config_name() -> Option<String> {
    let out = Command::new("snapper")
        .args(["--machine-readable", "csv", "list-configs"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Some(pick_config(&text)?)
}

/// Chooses a config from CSV output, preferring the one covering `/`.
fn pick_config(csv: &str) -> Option<String> {
    let mut names: Vec<(String, String)> = Vec::new();

    for line in csv.lines().skip(1) {
        let mut cols = line.split(',');
        let name = cols.next()?.trim();
        let subvolume = cols.next().unwrap_or("").trim();
        if name.is_empty() {
            continue;
        }
        names.push((name.to_string(), subvolume.to_string()));
    }

    // The config covering `/` is the one a package removal touches.
    if let Some((name, _)) = names.iter().find(|(_, sub)| sub == "/") {
        return Some(name.clone());
    }
    if let Some((name, _)) = names.iter().find(|(name, _)| name == "root") {
        return Some(name.clone());
    }
    names.into_iter().next().map(|(name, _)| name)
}

/// Builds the snapper command for a pre-transaction snapshot.
///
/// Returned rather than executed so it can run through the same privileged
/// pipeline as the removal itself, and so the caller can show the user exactly
/// what will run.
pub fn pre_snapshot_args(config: &str, description: &str) -> Vec<String> {
    vec![
        "-c".into(),
        config.into(),
        "create".into(),
        "--type".into(),
        "pre".into(),
        "--cleanup-algorithm".into(),
        "number".into(),
        "--print-number".into(),
        "--description".into(),
        description.into(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_args_are_well_formed() {
        let args = pre_snapshot_args("root", "apothiki: remove godot");
        assert_eq!(args[0], "-c");
        assert_eq!(args[1], "root");
        assert!(args.contains(&"pre".to_string()));
        // The description must survive as one argument even with spaces.
        assert_eq!(args.last().unwrap(), "apothiki: remove godot");
    }

    #[test]
    fn parses_real_machine_readable_output() {
        // Exactly what `snapper --machine-readable csv list-configs` prints here.
        let csv = "config,subvolume\nroot,/\n";
        assert_eq!(pick_config(csv).as_deref(), Some("root"));
    }

    #[test]
    fn prefers_the_config_covering_root() {
        let csv = "config,subvolume\nhome,/home\nroot,/\n";
        assert_eq!(pick_config(csv).as_deref(), Some("root"));
    }

    #[test]
    fn falls_back_to_the_only_config_when_none_covers_root() {
        let csv = "config,subvolume\ndata,/data\n";
        assert_eq!(pick_config(csv).as_deref(), Some("data"));
    }

    #[test]
    fn a_config_name_never_contains_table_decoration() {
        // Regression guard: the human-readable table uses `│` (U+2502), not an
        // ASCII pipe, so a `split('|')` parser returned "root   │ /" as the
        // config name and every snapshot attempt failed.
        let csv = "config,subvolume\nroot,/\n";
        let name = pick_config(csv).unwrap();
        assert!(!name.contains('│'), "{name:?}");
        assert!(!name.contains('|'), "{name:?}");
        assert!(!name.contains(' '), "{name:?}");
        assert_eq!(name, "root");
    }

    #[test]
    fn empty_output_yields_no_config() {
        assert_eq!(pick_config(""), None);
        assert_eq!(pick_config("config,subvolume\n"), None);
    }

    #[test]
    fn availability_check_does_not_panic_without_snapper() {
        // Whatever the machine, this must return a bool rather than blow up.
        let _ = is_available();
    }
}
