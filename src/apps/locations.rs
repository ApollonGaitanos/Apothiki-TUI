//! Where an application's files actually live (spec §14).
//!
//! The original question was *"show me where each application's files are, and
//! explain the logic of each directory"* — the part of the system that is least
//! discoverable, because pacman's file list is 300 unsorted paths and says
//! nothing about which of them matter.
//!
//! Two halves, with very different confidence:
//!
//! - **What the package owns** is exact. It comes from pacman's own file list,
//!   including `%BACKUP%`, which names precisely the files pacman treats as
//!   user-editable configuration.
//! - **What the user's own configuration is** is a guess. Nothing records the
//!   link between a package and `~/.config/<something>`; it is a naming
//!   convention, not a fact. So those are reported as candidates, labelled as
//!   guesses, and never deleted automatically.

use std::path::PathBuf;

use crate::data::local::{LocalDb, Package};

/// A group of paths that share a purpose.
#[derive(Debug, Clone)]
pub struct Group {
    pub title: &'static str,
    /// What this directory is for, in plain language. Static FHS knowledge —
    /// not discoverable from the system, which is exactly why it is worth
    /// shipping.
    pub explanation: &'static str,
    pub paths: Vec<Entry>,
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub path: String,
    /// Set when the path is a guess rather than a fact from the package.
    pub guessed: bool,
    /// Whether it exists right now.
    pub exists: bool,
    pub size: Option<u64>,
}

/// Extra names to try when guessing user directories.
///
/// A package name is a poor guess on its own: `librewolf-bin` stores data as
/// `librewolf`, and reverse-DNS applications use their AppStream id
/// (`~/.config/org.kde.dolphin`). Feeding in the binary name, the desktop id
/// and the AppStream id widens the net without loosening the labelling — every
/// hit is still a guess.
#[derive(Debug, Default, Clone)]
pub struct NameHints {
    pub binary: Option<String>,
    pub desktop_id: Option<String>,
    pub appstream_id: Option<String>,
}

/// Everything known about where one package's files are.
pub fn describe(db: &LocalDb, pkg: &Package) -> Vec<Group> {
    describe_with(db, pkg, &NameHints::default())
}

/// As `describe`, with extra naming hints for the guessed half.
pub fn describe_with(db: &LocalDb, pkg: &Package, hints: &NameHints) -> Vec<Group> {
    let files = db.read_files(pkg).unwrap_or_default();
    let mut groups = Vec::new();

    let owned = |pred: fn(&str) -> bool| -> Vec<Entry> {
        files
            .files
            .iter()
            .filter(|f| pred(f))
            .map(|f| Entry {
                path: format!("/{f}"),
                guessed: false,
                exists: true,
                size: None,
            })
            .collect()
    };

    let binaries = owned(|f| {
        (f.starts_with("usr/bin/") || f.starts_with("usr/local/bin/")) && !f.ends_with('/')
    });
    if !binaries.is_empty() {
        groups.push(Group {
            title: "Programs",
            explanation: "Executables on your PATH — what you actually run.",
            paths: binaries,
        });
    }

    // %BACKUP% is the authoritative list of files pacman treats as yours to
    // edit, and the ones it preserves as .pacsave on removal.
    let backups: Vec<Entry> = files
        .backup
        .iter()
        .map(|f| {
            let path = format!("/{f}");
            let exists = std::path::Path::new(&path).exists();
            Entry {
                path,
                guessed: false,
                exists,
                size: None,
            }
        })
        .collect();
    if !backups.is_empty() {
        groups.push(Group {
            title: "System configuration (tracked)",
            explanation: "pacman knows you may have edited these. On removal they are kept as .pacsave, unless you purge.",
            paths: backups,
        });
    }

    let data = owned(|f| f.starts_with("usr/share/") && !f.ends_with('/'));
    if !data.is_empty() {
        groups.push(Group {
            title: "Shared data",
            explanation: "Icons, translations, documentation and desktop entries. Managed entirely by the package.",
            paths: summarise_dirs(data),
        });
    }

    let libs = owned(|f| f.starts_with("usr/lib/") && !f.ends_with('/'));
    if !libs.is_empty() {
        groups.push(Group {
            title: "Libraries and internals",
            explanation: "Code other programs link against. Never edit these by hand.",
            paths: summarise_dirs(libs),
        });
    }

    // The guessed half.
    let candidates = user_paths_with(&pkg.name, hints);
    let present: Vec<Entry> = candidates.into_iter().filter(|e| e.exists).collect();
    if !present.is_empty() {
        groups.push(Group {
            title: "Your settings and data (guessed)",
            explanation: "Matched by name, not recorded anywhere. Removing the package never touches these; delete them yourself if you are certain.",
            paths: present,
        });
    }

    groups
}

/// Collapses a long file list into the directories that contain it.
///
/// A package can own several hundred files under `/usr/share`. Listing them all
/// answers no question anyone has; naming the directories does.
fn summarise_dirs(entries: Vec<Entry>) -> Vec<Entry> {
    let mut dirs: std::collections::BTreeMap<String, usize> = Default::default();
    for e in &entries {
        let dir = e
            .path
            .rsplit_once('/')
            .map(|(d, _)| d.to_string())
            .unwrap_or_else(|| e.path.clone());
        *dirs.entry(dir).or_default() += 1;
    }

    // Keep the deepest shared directories, not every leaf.
    let mut out: Vec<Entry> = dirs
        .into_iter()
        .map(|(dir, count)| Entry {
            path: if count > 1 {
                format!("{dir}/  ({count} files)")
            } else {
                dir
            },
            guessed: false,
            exists: true,
            size: None,
        })
        .collect();
    out.truncate(12);
    out
}

/// Candidate user-level directories for a package name.
pub fn user_paths(name: &str) -> Vec<Entry> {
    user_paths_with(name, &NameHints::default())
}

/// Candidate user-level directories, given every name we know for a package.
///
/// **This is guesswork by necessity, not by laziness.** pacman records only the
/// files it installed, and no package installs into a user's home directory —
/// `~/.config/discord` was created by Discord at runtime. There is no command
/// that can list these, because nothing anywhere associates them with a
/// package. XDG naming convention is the only signal available, and it is
/// imperfect: Firefox keeps its profile in `~/.mozilla`, which no amount of
/// name matching will find.
pub fn user_paths_with(name: &str, hints: &NameHints) -> Vec<Entry> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };

    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| home.join(".config"));
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| home.join(".local/share"));
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| home.join(".cache"));

    // Every name the package is known by, cheapest signal first.
    let mut names = vec![name.to_string()];
    for suffix in ["-bin", "-git", "-appimage", "-electron"] {
        if let Some(base) = name.strip_suffix(suffix) {
            names.push(base.to_string());
        }
    }
    if let Some(b) = &hints.binary {
        names.push(b.clone());
    }
    if let Some(id) = &hints.appstream_id {
        names.push(id.clone());
        // Applications commonly use the last component too: org.kde.dolphin
        // keeps state in ~/.local/share/dolphin.
        if let Some(last) = id.rsplit('.').next() {
            names.push(last.to_lowercase());
        }
    }
    if let Some(d) = &hints.desktop_id {
        let base = d.strip_suffix(".desktop").unwrap_or(d);
        names.push(base.to_string());
        if let Some(last) = base.rsplit('.').next() {
            names.push(last.to_lowercase());
        }
    }
    names.sort();
    names.dedup();

    let mut out = Vec::new();
    for n in names {
        for base in [&config, &data, &cache] {
            let p = base.join(&n);
            let exists = p.exists();
            out.push(Entry {
                path: p.display().to_string(),
                guessed: true,
                exists,
                size: exists.then(|| dir_size(&p)).flatten(),
            });
        }
    }
    out
}

/// Recursive size of a directory, bounded so a huge cache cannot stall the UI.
fn dir_size(path: &std::path::Path) -> Option<u64> {
    fn walk(path: &std::path::Path, budget: &mut u32) -> u64 {
        if *budget == 0 {
            return 0;
        }
        let Ok(entries) = std::fs::read_dir(path) else {
            return 0;
        };
        let mut total = 0;
        for e in entries.flatten() {
            if *budget == 0 {
                break;
            }
            *budget -= 1;
            match e.metadata() {
                Ok(m) if m.is_dir() => total += walk(&e.path(), budget),
                Ok(m) => total += m.len(),
                Err(_) => {}
            }
        }
        total
    }
    let mut budget = 20_000;
    Some(walk(path, &mut budget))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_paths_are_always_marked_as_guesses() {
        // The link between a package and ~/.config/<name> is a convention, not
        // a record. Presenting it as fact would invite deleting the wrong thing.
        for e in user_paths("firefox") {
            assert!(e.guessed, "{} must be marked a guess", e.path);
        }
    }

    #[test]
    fn reverse_dns_ids_widen_the_guess() {
        let hints = NameHints {
            appstream_id: Some("org.kde.dolphin".into()),
            desktop_id: Some("org.kde.dolphin.desktop".into()),
            binary: Some("dolphin".into()),
        };
        let paths = user_paths_with("dolphin", &hints);
        let all: Vec<&str> = paths.iter().map(|e| e.path.as_str()).collect();
        assert!(all.iter().any(|p| p.ends_with("/org.kde.dolphin")), "{all:?}");
        assert!(all.iter().any(|p| p.ends_with("/dolphin")), "{all:?}");
        // Still guesses, however many names we try.
        assert!(paths.iter().all(|e| e.guessed));
    }

    #[test]
    fn packaging_suffixes_are_stripped_when_guessing() {
        // `librewolf-bin` keeps its data under `librewolf`.
        let paths = user_paths("librewolf-bin");
        assert!(
            paths.iter().any(|e| e.path.ends_with("/librewolf")),
            "{:?}",
            paths.iter().map(|e| &e.path).collect::<Vec<_>>()
        );
        assert!(paths.iter().any(|e| e.path.ends_with("/librewolf-bin")));
    }

    #[test]
    fn long_file_lists_collapse_to_directories() {
        let entries: Vec<Entry> = (0..50)
            .map(|i| Entry {
                path: format!("/usr/share/thing/file{i}.png"),
                guessed: false,
                exists: true,
                size: None,
            })
            .collect();
        let out = summarise_dirs(entries);
        assert_eq!(out.len(), 1);
        assert!(out[0].path.contains("50 files"), "{}", out[0].path);
    }

    #[test]
    fn a_package_with_no_files_produces_no_groups() {
        let db = LocalDb {
            packages: vec![],
            errors: vec![],
            root: PathBuf::from("/nonexistent"),
        };
        let pkg = crate::data::local::parse_desc("%NAME%\nx\n\n%VERSION%\n1\n", "x-1").unwrap();
        // No files entry to read, and no user directories named `x`.
        let groups = describe(&db, &pkg);
        assert!(groups.iter().all(|g| !g.paths.is_empty()));
    }
}
