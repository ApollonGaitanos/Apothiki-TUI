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
pub fn config_name() -> Option<String> {
    let out = Command::new("snapper").arg("list-configs").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let names: Vec<String> = text
        .lines()
        .skip(2) // header and separator
        .filter_map(|l| l.split('|').next())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    // `root` covers `/`, which is what a package removal touches. Any other
    // config is a fallback rather than a guess at the right subvolume.
    if names.iter().any(|n| n == "root") {
        return Some("root".to_string());
    }
    names.into_iter().next()
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
    fn availability_check_does_not_panic_without_snapper() {
        // Whatever the machine, this must return a bool rather than blow up.
        let _ = is_available();
    }
}
