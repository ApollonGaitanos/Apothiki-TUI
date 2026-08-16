//! Detecting and applying updates.
//!
//! Spec §2 excluded a system updater from v1, and that exclusion was about
//! *scope*, not about pretending updates do not exist. Showing which installed
//! packages have a newer version is pure information and belongs in a tool whose
//! job is explaining the system.
//!
//! **Applying them is where care is needed.** On Arch, upgrading one package
//! while leaving the rest behind is a *partial upgrade*: the new build links
//! against library versions the rest of the system does not have yet, and the
//! result ranges from a broken program to a broken login. It is unsupported
//! upstream and it is the single most common way people destroy a rolling
//! install.
//!
//! A single-package upgrade is therefore offered but never the default, and is
//! labelled as risky everywhere it appears. Refusing outright would be the
//! safer engineering choice; it would also be paternalistic, and the user asked
//! for it knowing the trade-off. Making the safe path the obvious one and
//! naming the risk on the other is the honest middle.

use crate::data::aur::AurIndex;
use crate::data::local::LocalDb;
use crate::ops::exec;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Update {
    pub name: String,
    pub installed: String,
    pub available: String,
    pub source: UpdateSource,
    /// How the package presents itself to the user, for grouping.
    pub kind: Kind,
    /// The application name, when this package backs one.
    pub display_name: Option<String>,
}

/// What the package is, from the user's point of view. Ordered so `derive(Ord)`
/// sorts applications before tools before plumbing — the order the user asked
/// for, and the order of decreasing "I know what this is".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    App,
    Tool,
    Package,
}

impl Kind {
    pub fn label(&self) -> &'static str {
        match self {
            Kind::App => "app",
            Kind::Tool => "tool",
            Kind::Package => "package",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateSource {
    Repo,
    Aur,
}

#[derive(Debug, Clone, Default)]
pub struct UpdatePlan {
    pub repo: Vec<Update>,
    pub aur: Vec<Update>,
}

impl UpdatePlan {
    /// Every update, applications first, then tools, then plumbing.
    pub fn sorted(&self) -> Vec<Update> {
        let mut all: Vec<Update> = self.repo.iter().chain(self.aur.iter()).cloned().collect();
        all.sort_by(|a, b| {
            a.kind
                .cmp(&b.kind)
                .then_with(|| a.display_name.cmp(&b.display_name))
                .then_with(|| a.name.cmp(&b.name))
        });
        all
    }

    /// Upgrading a single package.
    ///
    /// Offered because the user asked for it, and flagged wherever it appears:
    /// for a repository package this is a partial upgrade, which is the classic
    /// way to break a rolling install. An AUR rebuild is safer in itself but can
    /// still pull repository dependencies forward, so the warning stands.
    pub fn single_args(update: &Update, noconfirm: bool) -> Vec<String> {
        let mut v = vec!["-S".to_string()];
        if noconfirm {
            v.push("--noconfirm".to_string());
        }
        v.push(update.name.clone());
        v
    }

    pub fn total(&self) -> usize {
        self.repo.len() + self.aur.len()
    }

    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }

    /// A full system upgrade. Never a subset — see the module note.
    ///
    /// `--noconfirm` is optional and off by default here, unlike everywhere
    /// else. Our dialog already confirms *whether* to upgrade, but a system
    /// upgrade raises questions we cannot answer in advance — replacing a
    /// package with its successor, choosing between providers, resolving a
    /// conflict. `--noconfirm` answers each with pacman's default, and for a
    /// conflict that default is "no", which aborts the entire transaction.
    pub fn system_upgrade_args(noconfirm: bool) -> Vec<String> {
        let mut v = vec!["-Syu".to_string()];
        if noconfirm {
            v.push("--noconfirm".to_string());
        }
        v
    }

    /// AUR upgrades, applied by the helper after the repo upgrade.
    pub fn aur_upgrade_args(noconfirm: bool) -> Vec<String> {
        let mut v = vec!["-Sua".to_string()];
        if noconfirm {
            v.push("--noconfirm".to_string());
        }
        v
    }
}

/// Repository packages with a newer version available.
///
/// Uses `pacman -Qu`, which is read-only, needs no privileges, and is pacman's
/// own answer rather than our reconstruction of it.
pub fn repo_updates() -> Vec<Update> {
    let Ok(lines) = exec::dry_run(&["-Qu".to_string()]) else {
        return Vec::new();
    };
    lines.iter().filter_map(|l| parse_qu_line(l)).collect()
}

/// Parses one `pacman -Qu` line: `name installed -> available`.
fn parse_qu_line(line: &str) -> Option<Update> {
    let mut parts = line.split_whitespace();
    let name = parts.next()?.to_string();
    let installed = parts.next()?.to_string();
    // The arrow is a literal `->`; anything else means a format we do not know.
    if parts.next()? != "->" {
        return None;
    }
    let available = parts.next()?.to_string();
    Some(Update {
        name,
        installed,
        available,
        source: UpdateSource::Repo,
        kind: Kind::Package,
        display_name: None,
    })
}

/// Installed AUR packages whose indexed version is newer.
///
/// Compared with `vercmp`, pacman's own version comparator, rather than a
/// hand-rolled one: Arch version strings carry epochs and pkgrel suffixes whose
/// ordering rules are not obvious, and getting them wrong means either offering
/// a downgrade or hiding a real update.
pub fn aur_updates(db: &LocalDb, aur: &AurIndex, foreign: &[String]) -> Vec<Update> {
    let mut out = Vec::new();
    for name in foreign {
        let Some(local) = db.packages.iter().find(|p| &p.name == name) else {
            continue;
        };
        let Some(remote) = aur.packages.iter().find(|p| &p.name == name) else {
            continue;
        };
        if vercmp(&local.version, &remote.version) == std::cmp::Ordering::Less {
            out.push(Update {
                name: name.clone(),
                installed: local.version.clone(),
                available: remote.version.clone(),
                source: UpdateSource::Aur,
                kind: Kind::Package,
                display_name: None,
            });
        }
    }
    out
}

/// Compares two version strings the way pacman does.
///
/// Falls back to string equality when `vercmp` is unavailable, which errs
/// toward reporting no update rather than a wrong one.
pub fn vercmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let out = std::process::Command::new("vercmp").args([a, b]).output();
    let Ok(out) = out else {
        return Ordering::Equal;
    };
    match String::from_utf8_lossy(&out.stdout).trim().parse::<i32>() {
        Ok(n) if n < 0 => Ordering::Less,
        Ok(n) if n > 0 => Ordering::Greater,
        _ => Ordering::Equal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn updates_sort_applications_before_plumbing() {
        let mk = |name: &str, kind: Kind| Update {
            name: name.into(),
            installed: "1".into(),
            available: "2".into(),
            source: UpdateSource::Repo,
            kind,
            display_name: None,
        };
        let plan = UpdatePlan {
            repo: vec![
                mk("libfoo", Kind::Package),
                mk("ripgrep", Kind::Tool),
                mk("firefox", Kind::App),
            ],
            aur: vec![],
        };
        let order: Vec<String> = plan.sorted().into_iter().map(|u| u.name).collect();
        assert_eq!(order, ["firefox", "ripgrep", "libfoo"]);
    }

    #[test]
    fn parses_pacman_qu_output() {
        let u = parse_qu_line("firefox 145.0-1 -> 146.0-1").unwrap();
        assert_eq!(u.name, "firefox");
        assert_eq!(u.installed, "145.0-1");
        assert_eq!(u.available, "146.0-1");
        assert_eq!(u.source, UpdateSource::Repo);
    }

    #[test]
    fn ignores_lines_in_an_unexpected_shape() {
        assert!(parse_qu_line("").is_none());
        assert!(parse_qu_line("just-a-name").is_none());
        assert!(parse_qu_line("name 1.0 1.0").is_none());
        // `-Qu` with [ignored] suffixes still parses the useful part.
        assert!(parse_qu_line("foo 1.0-1 -> 2.0-1 [ignored]").is_some());
    }

    #[test]
    fn the_system_upgrade_never_narrows_to_a_subset() {
        // -Syu must carry no package names: naming even one turns a full
        // upgrade into a partial one, which is the failure the whole design is
        // arranged around.
        let args = UpdatePlan::system_upgrade_args(true);
        assert_eq!(args[0], "-Syu");
        assert!(
            !args.iter().any(|a| !a.starts_with('-')),
            "no package names may appear: {args:?}"
        );
    }

    #[test]
    fn an_interactive_upgrade_omits_noconfirm() {
        // With --noconfirm pacman answers its own questions with the default,
        // and the default for a conflict is "no" — which aborts everything.
        let interactive = UpdatePlan::system_upgrade_args(false);
        assert_eq!(interactive, ["-Syu"]);
        assert!(UpdatePlan::system_upgrade_args(true).contains(&"--noconfirm".to_string()));
        assert_eq!(UpdatePlan::aur_upgrade_args(false), ["-Sua"]);
    }

    #[test]
    fn a_single_upgrade_names_exactly_one_package() {
        let u = Update {
            name: "firefox".into(),
            installed: "1".into(),
            available: "2".into(),
            source: UpdateSource::Repo,
            kind: Kind::App,
            display_name: Some("Firefox".into()),
        };
        assert_eq!(
            UpdatePlan::single_args(&u, true),
            ["-S", "--noconfirm", "firefox"]
        );
        assert_eq!(UpdatePlan::single_args(&u, false), ["-S", "firefox"]);
    }

    #[test]
    fn version_comparison_matches_pacman() {
        use std::cmp::Ordering;
        // Delegated to vercmp, so this checks the plumbing rather than the rules.
        assert_eq!(vercmp("1.0-1", "1.0-2"), Ordering::Less);
        assert_eq!(vercmp("1.0-2", "1.0-1"), Ordering::Greater);
        assert_eq!(vercmp("1.0-1", "1.0-1"), Ordering::Equal);
        // Epochs win over plain version ordering.
        assert_eq!(vercmp("1:1.0-1", "2.0-1"), Ordering::Greater);
    }

    #[test]
    fn an_empty_plan_reports_nothing_to_do() {
        let plan = UpdatePlan::default();
        assert!(plan.is_empty());
        assert_eq!(plan.total(), 0);
    }
}
