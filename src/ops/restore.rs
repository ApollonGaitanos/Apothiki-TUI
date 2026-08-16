//! Undo: reinstalling a just-removed set from the package cache (spec §6.5).
//!
//! `/var/cache/pacman/pkg/` keeps the `.pkg.tar.zst` files of everything that
//! has been installed, so an offline reinstall of an exact version is usually
//! possible. *Usually* is doing real work in that sentence — `paccache` prunes
//! the cache on a timer on many systems — so availability is checked **before**
//! the offer is made. Promising a restore and failing halfway through is worse
//! than saying up front that it cannot be done.
//!
//! This is not a substitute for the snapper snapshot. It restores packages, not
//! configuration: a `-Rns` purge deletes config files that no reinstall brings
//! back.

use std::path::PathBuf;

use crate::ops::history::{self, Entry};

#[derive(Debug, Clone)]
pub struct RestorePlan {
    /// The transaction being undone.
    pub operation: String,
    pub timestamp: i64,
    /// Packages whose exact version is present in the cache.
    pub available: Vec<(String, String, PathBuf)>,
    /// Packages that cannot be restored, because the cache no longer has them.
    pub missing: Vec<(String, String)>,
    /// True if the original removal purged config files, which no reinstall
    /// can undo.
    pub configs_were_purged: bool,
}

impl RestorePlan {
    /// Only a fully restorable transaction is offered.
    ///
    /// A partial restore leaves the system in a state neither the user nor the
    /// tool can describe — some packages back, some not — which is a worse
    /// place to be than simply not having undone anything.
    pub fn is_complete(&self) -> bool {
        !self.available.is_empty() && self.missing.is_empty()
    }


    /// `pacman -U` arguments: explicit file paths, so nothing is fetched.
    pub fn args(&self) -> Vec<String> {
        let mut v = vec!["-U".to_string(), "--noconfirm".to_string()];
        v.extend(
            self.available
                .iter()
                .map(|(_, _, p)| p.display().to_string()),
        );
        v
    }

    pub fn command_line(&self) -> String {
        format!("sudo pacman {}", self.args().join(" "))
    }
}

/// Builds a restore plan from a history entry.
pub fn plan_from(entry: &Entry) -> RestorePlan {
    let mut available = Vec::new();
    let mut missing = Vec::new();

    for (name, version) in &entry.packages {
        match history::cached_package(name, version) {
            Some(path) => available.push((name.clone(), version.clone(), path)),
            None => missing.push((name.clone(), version.clone())),
        }
    }

    RestorePlan {
        operation: entry.operation.clone(),
        timestamp: entry.timestamp,
        available,
        missing,
        // `-Rns` deleted the config files outright rather than leaving
        // `.pacsave` copies, and reinstalling the package does not bring a
        // user's configuration back.
        configs_were_purged: entry.operation.contains('n'),
    }
}

/// The most recent successful removal, if there is one.
pub fn last_undoable() -> Option<Entry> {
    let entries = history::read_all().ok()?;
    entries
        .into_iter()
        .rev()
        .find(|e| e.success && !e.packages.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(op: &str, pkgs: &[(&str, &str)]) -> Entry {
        Entry {
            timestamp: 1_783_277_044,
            operation: op.into(),
            packages: pkgs
                .iter()
                .map(|(n, v)| (n.to_string(), v.to_string()))
                .collect(),
            success: true,
            snapshot: None,
        }
    }

    #[test]
    fn a_plan_with_missing_packages_is_not_offered() {
        // Partial restores leave a state nobody can describe.
        let plan = RestorePlan {
            operation: "-Rs".into(),
            timestamp: 0,
            available: vec![("a".into(), "1".into(), PathBuf::from("/x/a.pkg.tar.zst"))],
            missing: vec![("b".into(), "2".into())],
            configs_were_purged: false,
        };
        assert!(!plan.is_complete());
    }

    #[test]
    fn an_empty_plan_is_not_complete() {
        let plan = RestorePlan {
            operation: "-Rs".into(),
            timestamp: 0,
            available: vec![],
            missing: vec![],
            configs_were_purged: false,
        };
        assert!(!plan.is_complete());
    }

    #[test]
    fn restore_installs_from_files_not_the_network() {
        let plan = RestorePlan {
            operation: "-Rs".into(),
            timestamp: 0,
            available: vec![(
                "godot".into(),
                "4.7.1-1.1".into(),
                PathBuf::from("/var/cache/pacman/pkg/godot-4.7.1-1.1-x86_64.pkg.tar.zst"),
            )],
            missing: vec![],
            configs_were_purged: false,
        };
        let args = plan.args();
        assert_eq!(args[0], "-U");
        assert!(args.last().unwrap().ends_with(".pkg.tar.zst"));
        assert!(plan.is_complete());
    }

    #[test]
    fn a_purge_is_flagged_as_only_partly_undoable() {
        // -Rns deleted config files; reinstalling cannot bring them back, and
        // the dialog has to say so.
        assert!(plan_from(&entry("-Rns", &[])).configs_were_purged);
        assert!(!plan_from(&entry("-Rs", &[])).configs_were_purged);
        assert!(!plan_from(&entry("-R", &[])).configs_were_purged);
    }
}
