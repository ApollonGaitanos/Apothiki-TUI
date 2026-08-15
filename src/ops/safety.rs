//! The denylist and risk tiers (spec §6.1, §6.2).
//!
//! This module's job is to make certain removals **structurally impossible**
//! rather than merely discouraged. There is no `--force`, no override flag, no
//! confirmation strong enough to get past the denylist. If someone genuinely
//! needs to remove `glibc`, they know how to use pacman directly; a
//! friendly-looking TUI is the last place that should be possible.

use std::collections::{HashMap, HashSet};

use crate::data::graph::{Graph, PkgIdx, RemovalPlan};

/// Package names that can never be removed through this tool.
///
/// The bar for inclusion is **"removing this breaks the next boot, the display,
/// or pacman itself"** — not "this looks important". An over-broad denylist is
/// not the safe direction: it turns "cannot remove this" into "cannot remove
/// anything", and a tool for removing things that refuses to remove things is
/// simply broken. Keyrings and mirrorlists are here because losing them breaks
/// pacman's ability to install anything, including a replacement.
const PROTECTED_NAMES: &[&str] = &[
    "base",
    "base-devel",
    "systemd",
    "systemd-libs",
    "glibc",
    "gcc-libs",
    "pacman",
    "bash",
    "coreutils",
    "filesystem",
    "mesa",
    "grub",
    "limine",
    "refind",
    "efibootmgr",
    // Losing these leaves pacman unable to verify or fetch anything.
    "archlinux-keyring",
    "cachyos-keyring",
    "cachyos-mirrorlist",
    "cachyos-v3-mirrorlist",
    "cachyos-v4-mirrorlist",
    "cachyos-settings",
    "cachyos-hooks",
    // The active desktop, and the display manager that starts it.
    "plasma-meta",
    "plasma-desktop",
    "plasma-workspace",
    "sddm",
];

/// Name prefixes that can never be removed.
///
/// Kept narrow on purpose. A blanket `cachyos-` or `kde-` prefix protected 57%
/// of this machine — including wallpapers and `kde-cli-tools` — because their
/// dependency closures cover most of a KDE install. `nvidia` as a bare prefix
/// swept in `nvidia-settings`, a configuration GUI whose removal breaks nothing.
const PROTECTED_PREFIXES: &[&str] = &[
    // Kernels, their headers, and firmware. `linux` does not match `linguist`.
    "linux",
    // The actual drivers, not the surrounding utilities.
    "nvidia-utils",
    "nvidia-open",
    "nvidia-dkms",
    "nvidia-lts",
    "lib32-nvidia",
    // The Vulkan loader itself, not `vulkan-tools`.
    "vulkan-icd-loader",
];

/// Groups whose every member is protected.
const PROTECTED_GROUPS: &[&str] = &["base", "base-devel"];

/// Desktop metapackages: `kde-*-meta` per spec §6.1. Matched as a pair so that
/// `kde-cli-tools` — an ordinary removable package — is not caught.
const PROTECTED_META: (&str, &str) = ("kde-", "-meta");

/// Why a package cannot be removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Protection {
    /// Matched the denylist directly.
    Direct(String),
    /// Required, transitively, by something on the denylist. Removing it would
    /// break a protected package just as surely as removing that package.
    RequiredBy(String),
}

impl Protection {
    pub fn explain(&self) -> String {
        match self {
            Protection::Direct(why) => {
                format!("This is part of the system, not something you installed ({why}).")
            }
            Protection::RequiredBy(root) => {
                format!("This is part of the system: {root} needs it to work.")
            }
        }
    }
}

pub struct Denylist {
    protected: HashMap<PkgIdx, Protection>,
}

impl Denylist {
    /// Computes the protected set: the named roots, plus everything they depend
    /// on transitively.
    ///
    /// The transitive step is the point. Protecting `systemd` while leaving its
    /// dependencies removable would let a user break the boot by deleting
    /// something two levels down that pacman would happily take.
    pub fn build(graph: &Graph) -> Self {
        let mut protected: HashMap<PkgIdx, Protection> = HashMap::new();
        let mut roots: Vec<(PkgIdx, String)> = Vec::new();

        for (i, pkg) in graph.db.packages.iter().enumerate() {
            let i = i as u32;
            let name = pkg.name.as_str();

            let reason = if PROTECTED_NAMES.contains(&name) {
                Some("core system package".to_string())
            } else if let Some(g) = pkg
                .groups
                .iter()
                .find(|g| PROTECTED_GROUPS.contains(&g.as_str()))
            {
                Some(format!("in the {g} group"))
            } else if PROTECTED_PREFIXES.iter().any(|p| name.starts_with(p)) {
                Some("kernel, driver or desktop component".to_string())
            } else if name.starts_with(PROTECTED_META.0) && name.ends_with(PROTECTED_META.1) {
                Some("desktop metapackage".to_string())
            } else {
                None
            };

            if let Some(reason) = reason {
                protected.insert(i, Protection::Direct(reason));
                roots.push((i, pkg.name.clone()));
            }
        }

        // Everything the roots depend on is equally load-bearing.
        for (root, root_name) in roots {
            for dep in graph.closure([root]) {
                protected
                    .entry(dep)
                    .or_insert_with(|| Protection::RequiredBy(root_name.clone()));
            }
        }

        Denylist { protected }
    }

    pub fn protection(&self, pkg: PkgIdx) -> Option<&Protection> {
        self.protected.get(&pkg)
    }

    pub fn is_protected(&self, pkg: PkgIdx) -> bool {
        self.protected.contains_key(&pkg)
    }

    pub fn len(&self) -> usize {
        self.protected.len()
    }

    pub fn is_empty(&self) -> bool {
        self.protected.is_empty()
    }
}

/// How much confirmation a removal demands (spec §6.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Risk {
    /// Leaf. Nothing depends on it, nothing optionally wants it.
    Safe,
    /// Has reverse dependencies, is an optional dependency of something
    /// installed, or the cascade reaches 5 or more packages.
    Caution,
    /// The cascade would take a package that backs a visible application, or
    /// frees more than 500 MB. Requires typing the name to confirm.
    Dangerous,
    /// On the denylist. Never offered at all.
    Blocked,
}

impl Risk {
    pub fn symbol(&self) -> &'static str {
        match self {
            Risk::Safe => "safe",
            Risk::Caution => "caution",
            Risk::Dangerous => "DANGEROUS",
            Risk::Blocked => "BLOCKED",
        }
    }

    /// Whether the user must type the package name to proceed.
    pub fn needs_typed_confirmation(&self) -> bool {
        *self == Risk::Dangerous
    }
}

const DANGEROUS_BYTES: u64 = 500 * 1024 * 1024;
const CAUTION_CASCADE: usize = 5;

/// Assesses a removal plan.
///
/// `app_packages` names the packages that back visible applications — losing one
/// of those is what makes a removal dangerous in the way a user actually cares
/// about, as distinct from merely large.
pub fn assess(
    graph: &Graph,
    denylist: &Denylist,
    plan: &RemovalPlan,
    app_packages: &HashSet<String>,
) -> Risk {
    let removed = plan.all_removed();

    if removed.iter().any(|&p| denylist.is_protected(p)) {
        return Risk::Blocked;
    }

    // Any application disappearing is the headline risk, whatever the size.
    let takes_an_app = removed
        .iter()
        .any(|&p| app_packages.contains(graph.name(p)));
    if takes_an_app || plan.freed_bytes > DANGEROUS_BYTES {
        return Risk::Dangerous;
    }

    let has_dependents = plan
        .target
        .iter()
        .any(|&t| !graph.required_by(t).is_empty() || !graph.optional_for(t).is_empty());
    if has_dependents || removed.len() >= CAUTION_CASCADE || !plan.optdep_losses.is_empty() {
        return Risk::Caution;
    }

    Risk::Safe
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::local::{LocalDb, Reason};
    use std::sync::Arc;

    fn db_of(specs: &[(&str, Reason, &[&str], &[&str])]) -> Arc<LocalDb> {
        let mut packages: Vec<_> = specs
            .iter()
            .map(|(name, reason, deps, groups)| {
                let mut text = format!("%NAME%\n{name}\n\n%VERSION%\n1-1\n\n%SIZE%\n1000\n");
                if *reason == Reason::Dependency {
                    text.push_str("\n%REASON%\n1\n");
                }
                if !deps.is_empty() {
                    text.push_str(&format!("\n%DEPENDS%\n{}\n", deps.join("\n")));
                }
                if !groups.is_empty() {
                    text.push_str(&format!("\n%GROUPS%\n{}\n", groups.join("\n")));
                }
                crate::data::local::parse_desc(&text, name).unwrap()
            })
            .collect();
        packages.sort_by(|a, b| a.name.cmp(&b.name));
        Arc::new(LocalDb {
            packages,
            errors: Vec::new(),
            root: Default::default(),
        })
    }

    #[test]
    fn core_packages_are_protected_by_name() {
        let db = db_of(&[
            ("glibc", Reason::Dependency, &[], &[]),
            ("pacman", Reason::Explicit, &[], &[]),
            ("ripgrep", Reason::Explicit, &[], &[]),
        ]);
        let g = Graph::build(db);
        let d = Denylist::build(&g);

        assert!(d.is_protected(g.index_of("glibc").unwrap()));
        assert!(d.is_protected(g.index_of("pacman").unwrap()));
        assert!(!d.is_protected(g.index_of("ripgrep").unwrap()));
    }

    #[test]
    fn kernels_and_drivers_are_protected_by_prefix() {
        let db = db_of(&[
            ("linux-cachyos", Reason::Explicit, &[], &[]),
            ("linux-cachyos-headers", Reason::Explicit, &[], &[]),
            ("nvidia-open-dkms", Reason::Explicit, &[], &[]),
            ("vulkan-icd-loader", Reason::Dependency, &[], &[]),
            ("linguist", Reason::Explicit, &[], &[]),
        ]);
        let g = Graph::build(db);
        let d = Denylist::build(&g);

        for name in [
            "linux-cachyos",
            "linux-cachyos-headers",
            "nvidia-open-dkms",
            "vulkan-icd-loader",
        ] {
            assert!(d.is_protected(g.index_of(name).unwrap()), "{name}");
        }
        // `linguist` starts with "lin" but not "linux" — prefix matching must
        // not be sloppy enough to protect unrelated packages.
        assert!(!d.is_protected(g.index_of("linguist").unwrap()));
    }

    #[test]
    fn the_denylist_does_not_swallow_the_distro_namespace() {
        // Regression guard. Blanket `cachyos-` / `kde-` / `nvidia` prefixes
        // protected 57% of the real machine — wallpapers and config GUIs
        // included — which makes a removal tool that cannot remove anything.
        let db = db_of(&[
            ("cachyos-wallpapers", Reason::Explicit, &[], &[]),
            ("cachyos-hello", Reason::Explicit, &[], &[]),
            ("cachyos-keyring", Reason::Explicit, &[], &[]),
            ("kde-cli-tools", Reason::Dependency, &[], &[]),
            ("kde-applications-meta", Reason::Explicit, &[], &[]),
            ("nvidia-settings", Reason::Explicit, &[], &[]),
            ("nvidia-utils", Reason::Dependency, &[], &[]),
            ("vulkan-tools", Reason::Explicit, &[], &[]),
            ("vulkan-icd-loader", Reason::Dependency, &[], &[]),
        ]);
        let g = Graph::build(db);
        let d = Denylist::build(&g);

        for removable in [
            "cachyos-wallpapers",
            "cachyos-hello",
            "kde-cli-tools",
            "nvidia-settings",
            "vulkan-tools",
        ] {
            assert!(
                !d.is_protected(g.index_of(removable).unwrap()),
                "{removable} must stay removable"
            );
        }
        for protected in [
            "cachyos-keyring",
            "kde-applications-meta",
            "nvidia-utils",
            "vulkan-icd-loader",
        ] {
            assert!(
                d.is_protected(g.index_of(protected).unwrap()),
                "{protected} must be protected"
            );
        }
    }

    #[test]
    fn protection_is_transitive() {
        // Protecting systemd while leaving what it needs removable would let a
        // user break the boot from two levels down.
        let db = db_of(&[
            ("systemd", Reason::Explicit, &["libcap"], &[]),
            ("libcap", Reason::Dependency, &["deep"], &[]),
            ("deep", Reason::Dependency, &[], &[]),
            ("unrelated", Reason::Explicit, &[], &[]),
        ]);
        let g = Graph::build(db);
        let d = Denylist::build(&g);

        assert!(d.is_protected(g.index_of("libcap").unwrap()));
        assert!(d.is_protected(g.index_of("deep").unwrap()));
        assert!(!d.is_protected(g.index_of("unrelated").unwrap()));

        match d.protection(g.index_of("deep").unwrap()).unwrap() {
            Protection::RequiredBy(root) => assert_eq!(root, "systemd"),
            other => panic!("expected RequiredBy, got {other:?}"),
        }
    }

    #[test]
    fn group_membership_protects() {
        let db = db_of(&[
            ("findutils", Reason::Explicit, &[], &["base"]),
            ("gcc", Reason::Explicit, &[], &["base-devel"]),
            ("neovim", Reason::Explicit, &[], &[]),
        ]);
        let g = Graph::build(db);
        let d = Denylist::build(&g);

        assert!(d.is_protected(g.index_of("findutils").unwrap()));
        assert!(d.is_protected(g.index_of("gcc").unwrap()));
        assert!(!d.is_protected(g.index_of("neovim").unwrap()));
    }

    #[test]
    fn a_protected_package_is_blocked_whatever_the_plan_looks_like() {
        let db = db_of(&[
            ("glibc", Reason::Dependency, &[], &[]),
            ("app", Reason::Explicit, &[], &[]),
        ]);
        let g = Graph::build(db);
        let d = Denylist::build(&g);
        let plan = g.plan_removal(&[g.index_of("glibc").unwrap()]);

        assert_eq!(
            assess(&g, &d, &plan, &HashSet::new()),
            Risk::Blocked,
            "the denylist must win regardless of cascade size"
        );
    }

    #[test]
    fn losing_an_application_is_always_dangerous() {
        let db = db_of(&[
            ("gimp", Reason::Explicit, &[], &[]),
            ("other", Reason::Explicit, &[], &[]),
        ]);
        let g = Graph::build(db);
        let d = Denylist::build(&g);
        let plan = g.plan_removal(&[g.index_of("gimp").unwrap()]);

        let apps: HashSet<String> = ["gimp".to_string()].into_iter().collect();
        // Tiny package, no cascade — dangerous purely because an app vanishes.
        assert_eq!(assess(&g, &d, &plan, &apps), Risk::Dangerous);
        assert!(Risk::Dangerous.needs_typed_confirmation());
    }

    #[test]
    fn a_true_leaf_is_safe() {
        let db = db_of(&[
            ("leaf", Reason::Explicit, &[], &[]),
            ("other", Reason::Explicit, &[], &[]),
        ]);
        let g = Graph::build(db);
        let d = Denylist::build(&g);
        let plan = g.plan_removal(&[g.index_of("leaf").unwrap()]);

        assert_eq!(assess(&g, &d, &plan, &HashSet::new()), Risk::Safe);
        assert!(!Risk::Safe.needs_typed_confirmation());
    }

    #[test]
    fn having_dependents_raises_to_caution() {
        let db = db_of(&[
            ("app", Reason::Explicit, &["lib"], &[]),
            ("lib", Reason::Dependency, &[], &[]),
        ]);
        let g = Graph::build(db);
        let d = Denylist::build(&g);
        let plan = g.plan_removal(&[g.index_of("lib").unwrap()]);
        assert_eq!(assess(&g, &d, &plan, &HashSet::new()), Risk::Caution);
    }
}
