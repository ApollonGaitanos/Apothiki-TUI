//! Synthesising Applications from packages (spec §4).
//!
//! pacman has no concept of an application; it has packages. Closing that gap
//! is the whole product. An `App` is *derived* from layered evidence and stored
//! nowhere.
//!
//! Layers, in descending order of trust, with higher layers winning conflicts:
//!
//! | Layer | Source | Role |
//! |---|---|---|
//! | 0 | pacman local db | ground truth: what is installed |
//! | 1 | `/usr/share/metainfo` | highest trust; owning package known exactly |
//! | 2 | `.desktop` scan | the workhorse; what launchers actually show |
//! | 3 | `/usr/share/swcatalog` | enrichment only, never authority |
//! | 4 | heuristics | Tools, AppImages, Flatpak |
//!
//! **Never filter by `pacman -Qe`.** On this machine 1375 of 1656 packages are
//! marked as dependencies, and most applications arrive through metapackages, so
//! explicit-only would hide half the user's programs (spec §4.3).

pub mod desktop;
pub mod metainfo;

use std::collections::{BTreeMap, HashMap};

use crate::data::fileindex::FileIndex;
use crate::data::local::LocalDb;

/// Where an application came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Backed by one or more pacman packages.
    Pacman,
    /// A launchable with no owning package — Flatpak, AppImage, or something
    /// hand-installed. Layer 4 refines these; until then the distinction that
    /// matters is simply "pacman does not own this".
    Unowned,
}

/// Why we believe this is an application. Every classification must be
/// explainable in the UI — for a tool whose main job is deleting things,
/// "trust me" is not acceptable (spec §16).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Evidence {
    Metainfo(String),
    DesktopEntry(String),
    /// Merged into another app by the conservative suffix rule.
    MergedPackage { package: String, suffix: String },
    /// A `Terminal=true` desktop entry: a CLI program, not a GUI app.
    TerminalEntry,
    /// Explicitly installed, ships a binary, but has no launchable at all.
    ExplicitWithBinary(String),
    /// Explicitly installed with no launchable and no binary of its own.
    ExplicitNoLaunchable,
}

/// Groups whose members are system scaffolding rather than user choices, and so
/// never appear as Tools even though they are explicitly installed.
const SYSTEM_GROUPS: &[&str] = &["base", "base-devel"];

#[derive(Debug, Clone)]
pub struct App {
    pub name: String,
    pub summary: Option<String>,
    pub icon: Option<String>,
    pub categories: Vec<String>,
    pub exec: Option<String>,
    pub desktop_id: Option<String>,
    /// Reverse-DNS AppStream id, when Layer 1 had something to say.
    pub appstream_id: Option<String>,
    /// Backing packages, primary first.
    pub packages: Vec<String>,
    pub source: Source,
    pub evidence: Vec<Evidence>,
}

impl App {
    pub fn primary_package(&self) -> Option<&str> {
        self.packages.first().map(|s| s.as_str())
    }
}

/// Package name suffixes that mark an auxiliary package rather than an app of
/// its own (spec §15.5).
///
/// Merging is deliberately conservative: a package qualifies only if its name is
/// `<app>-<suffix>` **and** it ships no launchable of its own. A duplicate row is
/// an acceptable failure; a wrong merge hides a real application.
pub const DEFAULT_MERGE_SUFFIXES: &[&str] = &[
    "docs", "doc", "data", "common", "icons", "themes", "i18n", "l10n", "help", "lang",
];

/// Desktop file ids that no launcher shows and that only add noise. Kept here as
/// a starting point; it belongs in config, not code (spec §4.2).
pub const DEFAULT_NOISE: &[&str] = &[
    "*-url-handler.desktop",
    "org.kde.kwin.*",
    "xterm.desktop",
    "bssh.desktop",
    "bvnc.desktop",
    "lstopo.desktop",
];

#[derive(Debug, Default)]
pub struct Catalog {
    pub apps: Vec<App>,
    /// `Terminal=true` launchables and explicitly-installed packages with no
    /// launchable evidence.
    pub tools: Vec<App>,
    /// Desktop entries that were filtered out, with the reason, so the UI can
    /// answer "why isn't X in my list?".
    pub filtered: Vec<(String, String)>,
}

/// Builds the application catalog from Layers 1, 2 and 0.
pub fn resolve(
    db: &LocalDb,
    index: &FileIndex,
    merge_suffixes: &[String],
    noise: &[String],
) -> Catalog {
    let scan = desktop::scan(&desktop::search_dirs(), noise);
    let components = metainfo::scan(&metainfo::search_dirs());

    // Layer 1 keyed by the desktop id it claims, so it can enrich Layer 2.
    // Addons, fonts and console applications never claim one.
    let mut by_desktop_id: HashMap<String, &metainfo::Component> = HashMap::new();
    for c in &components {
        if let Some(did) = c.desktop_id() {
            by_desktop_id.insert(did, c);
        }
    }

    let mut catalog = Catalog::default();

    let build = |entry: &desktop::DesktopEntry| -> App {
        let component = by_desktop_id.get(&entry.id);
        let owner = index.owner(&entry.path).map(|s| s.to_string());

        let mut evidence = Vec::new();
        if let Some(c) = component {
            evidence.push(Evidence::Metainfo(c.id.clone()));
        }
        evidence.push(Evidence::DesktopEntry(entry.id.clone()));

        App {
            // Layer 1 outranks Layer 2 for display strings: the upstream author
            // wrote them, and they are not subject to launcher-specific edits.
            name: component
                .and_then(|c| c.name.clone())
                .or_else(|| entry.name.clone())
                .unwrap_or_else(|| entry.id.clone()),
            summary: component
                .and_then(|c| c.summary.clone())
                .or_else(|| entry.comment.clone())
                .or_else(|| entry.generic_name.clone()),
            icon: entry.icon.clone(),
            categories: if entry.categories.is_empty() {
                component.map(|c| c.categories.clone()).unwrap_or_default()
            } else {
                entry.categories.clone()
            },
            exec: entry.exec.clone(),
            desktop_id: Some(entry.id.clone()),
            appstream_id: component.map(|c| c.id.clone()),
            source: if owner.is_some() {
                Source::Pacman
            } else {
                Source::Unowned
            },
            packages: owner.into_iter().collect(),
            evidence,
        }
    };

    for entry in scan.apps.values() {
        catalog.apps.push(build(entry));
    }
    for entry in scan.tools.values() {
        let mut app = build(entry);
        app.evidence.push(Evidence::TerminalEntry);
        catalog.tools.push(app);
    }
    for (entry, why) in &scan.rejected {
        catalog
            .filtered
            .push((entry.id.clone(), format!("{why:?}")));
    }

    merge_auxiliary_packages(&mut catalog.apps, db, merge_suffixes);
    add_explicit_tools(&mut catalog, db, index);

    catalog.apps.sort_by(|a, b| a.name.cmp(&b.name));
    catalog.tools.sort_by(|a, b| a.name.cmp(&b.name));
    catalog
}

/// Layer 4: explicitly-installed packages with no launchable evidence.
///
/// `ripgrep`, `ffmpeg`, `docker` — deliberate user choices that are not
/// applications and must not be buried among 1300 dependencies. Ranked by
/// whether the package owns something in `/usr/bin`, since a package that ships
/// no binary at all is likelier to be a library the user installed by hand.
///
/// `base`/`base-devel` members are excluded: they are system scaffolding that
/// arrives with the install medium, not something anyone chose.
fn add_explicit_tools(catalog: &mut Catalog, db: &LocalDb, index: &FileIndex) {
    use crate::data::local::Reason;

    let binaries = index.binaries_by_package();
    let accounted: std::collections::HashSet<String> = catalog
        .apps
        .iter()
        .chain(catalog.tools.iter())
        .flat_map(|a| a.packages.iter().cloned())
        .collect();

    for pkg in &db.packages {
        if pkg.reason != Reason::Explicit || accounted.contains(&pkg.name) {
            continue;
        }
        // Both the members of these groups and the group metapackages
        // themselves: `base` is not listed as a member of the `base` group, so
        // the group check alone lets it through as a "tool the user chose".
        let is_system = pkg.groups.iter().any(|g| SYSTEM_GROUPS.contains(&g.as_str()))
            || SYSTEM_GROUPS.contains(&pkg.name.as_str());
        if is_system {
            continue;
        }

        let binary = binaries.get(pkg.name.as_str()).map(|b| b.to_string());

        catalog.tools.push(App {
            name: pkg.name.clone(),
            summary: pkg.desc.clone(),
            icon: None,
            categories: Vec::new(),
            exec: binary.clone(),
            desktop_id: None,
            appstream_id: None,
            packages: vec![pkg.name.clone()],
            source: Source::Pacman,
            evidence: vec![match binary {
                Some(b) => Evidence::ExplicitWithBinary(b),
                None => Evidence::ExplicitNoLaunchable,
            }],
        });
    }
}

/// Folds `<app>-<suffix>` packages into the app they belong to.
///
/// Only packages with **no launchable of their own** are candidates: anything
/// that ships a desktop entry is an application in its own right, whatever its
/// name looks like.
fn merge_auxiliary_packages(apps: &mut [App], db: &LocalDb, suffixes: &[String]) {
    let mut owners: BTreeMap<String, usize> = BTreeMap::new();
    for (i, app) in apps.iter().enumerate() {
        if let Some(p) = app.primary_package() {
            owners.insert(p.to_string(), i);
        }
    }

    for pkg in &db.packages {
        // A package that already backs an app is never auxiliary.
        if owners.contains_key(&pkg.name) {
            continue;
        }
        let Some((base, suffix)) = split_suffix(&pkg.name, suffixes) else {
            continue;
        };
        let Some(&i) = owners.get(base) else { continue };
        let suffix = suffix.to_string();

        apps[i].packages.push(pkg.name.clone());
        apps[i].evidence.push(Evidence::MergedPackage {
            package: pkg.name.clone(),
            suffix,
        });
    }
}

/// Splits `gimp-help-en` into (`gimp`, `help`) when the suffix is recognised.
///
/// Handles both `<app>-<suffix>` and `<app>-<suffix>-<variant>` (language codes
/// such as `-help-en` or `-lang-de`).
fn split_suffix<'a>(name: &'a str, suffixes: &'a [String]) -> Option<(&'a str, &'a str)> {
    for suffix in suffixes {
        // Exact tail: `foo-docs`.
        if let Some(base) = name.strip_suffix(&format!("-{suffix}")) {
            if !base.is_empty() {
                return Some((base, suffix));
            }
        }
        // Tail with a variant: `foo-help-en`.
        let marker = format!("-{suffix}-");
        if let Some(pos) = name.rfind(&marker) {
            if pos > 0 {
                return Some((&name[..pos], suffix));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn suffixes() -> Vec<String> {
        DEFAULT_MERGE_SUFFIXES.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn recognises_auxiliary_package_names() {
        let s = suffixes();
        assert_eq!(split_suffix("gimp-help-en", &s), Some(("gimp", "help")));
        assert_eq!(split_suffix("foo-docs", &s), Some(("foo", "docs")));
        assert_eq!(split_suffix("kde-l10n-de", &s), Some(("kde", "l10n")));
        assert_eq!(split_suffix("qt6-base-common", &s), Some(("qt6-base", "common")));
    }

    #[test]
    fn leaves_ordinary_names_alone() {
        let s = suffixes();
        // These must never merge: they are applications in their own right.
        assert_eq!(split_suffix("firefox", &s), None);
        assert_eq!(split_suffix("libreoffice-fresh", &s), None);
        assert_eq!(split_suffix("python-requests", &s), None);
        // A bare suffix with no base is not a merge candidate either.
        assert_eq!(split_suffix("-docs", &s), None);
        assert_eq!(split_suffix("docs", &s), None);
    }

    #[test]
    fn base_metapackages_are_not_tools() {
        // `base` is not a member of the `base` group, so a group-membership
        // check alone lets the metapackage through as a user choice.
        assert!(SYSTEM_GROUPS.contains(&"base"));
        assert!(SYSTEM_GROUPS.contains(&"base-devel"));
    }

    #[test]
    fn a_package_with_its_own_launchable_is_never_merged() {
        // `gimp-help-en` would match the suffix rule, but if it backs an app of
        // its own it must stay separate. A duplicate row is acceptable; losing a
        // real application is not.
        let mut apps = vec![
            App {
                name: "GIMP".into(),
                summary: None,
                icon: None,
                categories: vec![],
                exec: None,
                desktop_id: None,
                appstream_id: None,
                packages: vec!["gimp".into()],
                source: Source::Pacman,
                evidence: vec![],
            },
            App {
                name: "GIMP Help".into(),
                summary: None,
                icon: None,
                categories: vec![],
                exec: None,
                desktop_id: None,
                appstream_id: None,
                packages: vec!["gimp-help-en".into()],
                source: Source::Pacman,
                evidence: vec![],
            },
        ];

        let db = LocalDb {
            packages: vec![],
            errors: vec![],
            root: Default::default(),
        };
        merge_auxiliary_packages(&mut apps, &db, &suffixes());

        assert_eq!(apps[0].packages, ["gimp"]);
        assert_eq!(apps[1].packages, ["gimp-help-en"]);
    }
}
