//! Mutating operations: planning, safety, and execution (spec §6, §7.4).
//!
//! **We never write to the pacman database.** Every mutation shells out to
//! pacman itself. This module builds and checks plans; `exec` runs them.

pub mod bundle;
pub mod exec;
pub mod history;
pub mod restore;
pub mod safety;
pub mod snapshot;
pub mod update;

use std::collections::HashSet;

use crate::data::graph::{Graph, PkgIdx, RemovalPlan};

/// How to remove a package.
///
/// Deliberately excludes two pacman modes:
///
/// - **`-Rc` (cascade)** removes everything that *depends on* the target. On a
///   desktop system that reaches the desktop metapackage with alarming ease, and
///   the user asked to delete one program, not everything built on it.
/// - **`-Rdd`** skips dependency checks entirely, leaving a system that pacman
///   believes is consistent while it is not.
///
/// Neither is offered, and there is no flag to turn them on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemovalMode {
    /// `-R` — only the named packages. Their now-unused dependencies stay.
    JustThis,
    /// `-Rs` — the package plus dependencies nothing else needs. The default,
    /// and what the impact preview simulates.
    WithUnusedDeps,
    /// `-Rns` — as above, and also delete tracked config files instead of
    /// leaving them as `.pacsave`. Irreversible in a way the others are not:
    /// the reinstall-from-cache undo cannot bring configs back.
    Purge,
}

impl RemovalMode {
    pub const ALL: [RemovalMode; 3] = [
        RemovalMode::WithUnusedDeps,
        RemovalMode::JustThis,
        RemovalMode::Purge,
    ];

    pub fn flags(&self) -> &'static str {
        match self {
            RemovalMode::JustThis => "-R",
            RemovalMode::WithUnusedDeps => "-Rs",
            RemovalMode::Purge => "-Rns",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            RemovalMode::JustThis => "Remove only this package",
            RemovalMode::WithUnusedDeps => "Remove with unused dependencies",
            RemovalMode::Purge => "Purge: also delete config files",
        }
    }

    pub fn detail(&self) -> &'static str {
        match self {
            RemovalMode::JustThis => "Dependencies it pulled in stay behind, possibly orphaned.",
            RemovalMode::WithUnusedDeps => "Standard removal. Matches the impact preview.",
            RemovalMode::Purge => "Config files are deleted, not kept as .pacsave. Cannot be undone.",
        }
    }

    /// Whether this mode cascades into dependencies.
    pub fn takes_dependencies(&self) -> bool {
        matches!(self, RemovalMode::WithUnusedDeps | RemovalMode::Purge)
    }

    /// Builds the pacman argument list.
    ///
    /// `--noconfirm` is included because our own confirmation dialog already
    /// ran, with a fuller impact preview than pacman's prompt provides. It is
    /// never used to skip a confirmation the user did not give.
    pub fn args(&self, packages: &[String]) -> Vec<String> {
        let mut v: Vec<String> = vec![self.flags().to_string(), "--noconfirm".to_string()];
        v.extend(packages.iter().cloned());
        v
    }

    /// The read-only dry-run form of the same operation.
    ///
    /// **`-n` is dropped here.** pacman refuses `--nosave` together with
    /// `--print` ("may not be used together"), so a purge dry-run errors out
    /// and aborts the removal. Dropping it is correct rather than a workaround:
    /// `-n` governs whether *config files* are kept as `.pacsave`, and has no
    /// effect on which packages are removed — which is the only thing the
    /// dry-run is asked to confirm.
    pub fn dry_run_args(&self, packages: &[String]) -> Vec<String> {
        let flags = match self {
            RemovalMode::Purge => "-Rs",
            other => other.flags(),
        };
        let mut v: Vec<String> = vec![flags.to_string(), "--print".to_string()];
        v.extend(packages.iter().cloned());
        v
    }
}

/// How to install a package.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallSource {
    /// From a configured repository, via pacman.
    Repo,
    /// From the AUR, via a helper that builds it.
    Aur,
}

/// An install the user has asked for but not yet confirmed.
#[derive(Debug, Clone)]
pub struct InstallRequest {
    pub package: String,
    pub version: String,
    pub source: InstallSource,
    /// The AUR helper to drive, when one is needed.
    pub helper: Option<String>,
    /// Risk notes to show before confirming.
    pub warnings: Vec<String>,
}

impl InstallRequest {
    /// `--noconfirm` is optional for the same reason it is on upgrades: an AUR
    /// helper told not to ask refuses outright when a conflict needs deciding
    /// ("can not install conflicting packages with --noconfirm") rather than
    /// picking something reasonable.
    pub fn args(&self, noconfirm: bool) -> Vec<String> {
        let mut v = vec!["-S".to_string()];
        if noconfirm {
            v.push("--noconfirm".to_string());
        }
        v.push(self.package.clone());
        v
    }

    pub fn command_line(&self, noconfirm: bool) -> String {
        match self.source {
            InstallSource::Repo => format!("sudo pacman {}", self.args(noconfirm).join(" ")),
            InstallSource::Aur => format!(
                "{} {}",
                self.helper.clone().unwrap_or_else(|| "paru".into()),
                self.args(noconfirm).join(" ")
            ),
        }
    }
}

/// Finds an AUR helper, preferring `paru` (spec §11).
///
/// We drive a helper rather than reimplementing AUR builds: `.SRCINFO` parsing,
/// dependency resolution and makepkg orchestration are explicitly out of scope
/// (§2), and getting them subtly wrong is worse than not having them.
pub fn find_aur_helper() -> Option<String> {
    ["paru", "yay", "pikaur"]
        .iter()
        .find(|h| which(h))
        .map(|h| h.to_string())
}

fn which(program: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|d| d.join(program).is_file()))
        .unwrap_or(false)
}

/// A removal the user has asked for but not yet confirmed.
pub struct RemovalRequest {
    pub targets: Vec<PkgIdx>,
    pub mode: RemovalMode,
    pub plan: RemovalPlan,
    pub risk: safety::Risk,
    /// Set when the denylist blocks it, with the explanation to show.
    pub blocked_by: Option<safety::Protection>,
    /// Applications that would disappear, by name.
    pub apps_lost: Vec<String>,
    pub snapshot: bool,
}

impl RemovalRequest {
    pub fn build(
        graph: &Graph,
        denylist: &safety::Denylist,
        targets: Vec<PkgIdx>,
        mode: RemovalMode,
        app_packages: &HashSet<String>,
        apps_by_package: &std::collections::HashMap<String, Vec<String>>,
    ) -> Self {
        // `-R` takes only the named packages, so the cascade is not part of the
        // plan and the preview must not imply otherwise.
        let plan = if mode.takes_dependencies() {
            graph.plan_removal(&targets)
        } else {
            let mut p = graph.plan_removal(&targets);
            p.freed_bytes = targets
                .iter()
                .map(|&t| graph.db.packages[t as usize].size_bytes())
                .sum();
            p.cascade.clear();
            p
        };

        let blocked_by = plan
            .all_removed()
            .iter()
            .find_map(|&p| denylist.protection(p).cloned());

        let risk = if blocked_by.is_some() {
            safety::Risk::Blocked
        } else {
            safety::assess(graph, denylist, &plan, app_packages)
        };

        let mut apps_lost: Vec<String> = Vec::new();
        for p in plan.all_removed() {
            if let Some(names) = apps_by_package.get(graph.name(p)) {
                for n in names {
                    if !apps_lost.contains(n) {
                        apps_lost.push(n.clone());
                    }
                }
            }
        }

        RemovalRequest {
            targets,
            mode,
            plan,
            risk,
            blocked_by,
            apps_lost,
            snapshot: snapshot::is_available(),
        }
    }

    pub fn package_names(&self, graph: &Graph) -> Vec<String> {
        self.targets
            .iter()
            .map(|&t| graph.name(t).to_string())
            .collect()
    }

    /// The command as it would be run, for display. Users are entitled to see
    /// exactly what is about to happen to their system.
    pub fn command_line(&self, graph: &Graph) -> String {
        format!(
            "sudo pacman {}",
            self.mode.args(&self.package_names(graph)).join(" ")
        )
    }

    pub fn is_blocked(&self) -> bool {
        self.risk == safety::Risk::Blocked || self.plan.is_blocked()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_map_to_the_right_pacman_flags() {
        assert_eq!(RemovalMode::JustThis.flags(), "-R");
        assert_eq!(RemovalMode::WithUnusedDeps.flags(), "-Rs");
        assert_eq!(RemovalMode::Purge.flags(), "-Rns");
    }

    #[test]
    fn dangerous_pacman_modes_are_not_representable() {
        // -Rc and -Rdd must not be constructible at all: excluded by the type,
        // not by a runtime check someone can bypass later.
        for m in RemovalMode::ALL {
            let f = m.flags();
            assert!(!f.contains("c"), "{f} must not cascade to dependents");
            assert!(!f.contains("dd"), "{f} must not skip dependency checks");
        }
        assert_eq!(RemovalMode::ALL.len(), 3);
    }

    #[test]
    fn argument_lists_are_built_correctly() {
        let pkgs = vec!["godot".to_string()];
        assert_eq!(
            RemovalMode::WithUnusedDeps.args(&pkgs),
            ["-Rs", "--noconfirm", "godot"]
        );
        // The purge dry-run must not carry -n: pacman rejects --nosave with
        // --print, which aborted every purge before it ever ran.
        assert_eq!(
            RemovalMode::Purge.dry_run_args(&pkgs),
            ["-Rs", "--print", "godot"]
        );
        // The real command still purges.
        assert_eq!(
            RemovalMode::Purge.args(&pkgs),
            ["-Rns", "--noconfirm", "godot"]
        );
    }

    #[test]
    fn no_dry_run_combines_flags_pacman_rejects() {
        // Guards the whole family: --print is incompatible with --nosave.
        for m in RemovalMode::ALL {
            let args = m.dry_run_args(&["x".to_string()]);
            let flags = &args[0];
            assert!(
                !flags.contains('n'),
                "{flags} with --print is rejected by pacman"
            );
        }
    }

    #[test]
    fn only_recursive_modes_take_dependencies() {
        assert!(!RemovalMode::JustThis.takes_dependencies());
        assert!(RemovalMode::WithUnusedDeps.takes_dependencies());
        assert!(RemovalMode::Purge.takes_dependencies());
    }
}
