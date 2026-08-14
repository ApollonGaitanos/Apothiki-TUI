//! Parser for `/var/lib/pacman/local/<pkg>-<ver>/{desc,files}`.
//!
//! Hand-rolled rather than via the `alpm` crate: see spec §5.1. The format is a
//! sequence of `%KEY%` lines, each followed by one value per line and terminated
//! by a blank line. It is stable and trivially parsed; FFI would couple our
//! ability to compile to pacman's soname churn.

use std::fs;
use std::path::{Path, PathBuf};

use crate::data::dep::Dep;
use crate::data::descfmt::{deps, many, one, parse_sections, Sections};

/// Why a package is on the system. `%REASON% 1` means pacman pulled it in to
/// satisfy something else; the field is absent for a deliberate install.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Explicit,
    Dependency,
}

/// An optional dependency, with the reason string pacman stores alongside it.
///
/// These are deliberately *not* graph edges (spec §5.2). They are documentation,
/// but removing one silently degrades an app, so the reason text must survive
/// parsing and reach the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptDep {
    pub dep: Dep,
    pub reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Package {
    pub name: String,
    pub version: String,
    /// `%BASE%` — the pkgbase. Split packages from one PKGBUILD share it.
    pub base: Option<String>,
    pub desc: Option<String>,
    pub url: Option<String>,
    pub arch: Option<String>,
    /// `%INSTALLED_DB%` — the repo this was installed from, when pacman
    /// recorded it.
    ///
    /// **Absence proves nothing.** Only newer pacman versions write this field,
    /// so anything installed before that lacks it: on the dev machine 520 of
    /// 1656 packages have no `%INSTALLED_DB%` while only 11 are genuinely
    /// foreign. Treat a present value as authoritative and fall back to sync-db
    /// lookup (`data::sync`) otherwise. Do not use this to detect AUR packages.
    pub repo: Option<String>,
    pub packager: Option<String>,
    pub build_date: Option<i64>,
    pub install_date: Option<i64>,
    /// Installed size in bytes.
    pub size: Option<u64>,
    pub groups: Vec<String>,
    pub license: Vec<String>,
    pub validation: Option<String>,
    pub reason: Reason,
    pub depends: Vec<Dep>,
    pub optdepends: Vec<OptDep>,
    pub provides: Vec<Dep>,
    pub conflicts: Vec<Dep>,
    pub replaces: Vec<Dep>,
    /// Directory name under `local/`, e.g. `filelight-26.04.3-1.1`.
    pub dir_name: String,
}

impl Package {
    pub fn size_bytes(&self) -> u64 {
        self.size.unwrap_or(0)
    }
}

/// `%OPTDEPENDS%` lines are `pkgname: free text reason`, the reason optional.
/// Shared with the sync-db parser, which uses the identical format.
pub fn parse_optdepends(m: &Sections) -> Vec<OptDep> {
    m.get("OPTDEPENDS")
        .map(|v| {
            v.iter()
                .map(|line| match line.split_once(':') {
                    Some((dep, reason)) => OptDep {
                        dep: Dep::parse(dep.trim()),
                        reason: Some(reason.trim().to_string()).filter(|r| !r.is_empty()),
                    },
                    None => OptDep {
                        dep: Dep::parse(line.trim()),
                        reason: None,
                    },
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parses one `desc` file body into a `Package`.
///
/// Returns `None` only when `%NAME%` or `%VERSION%` is missing — an entry
/// without those is not usable as a graph node.
pub fn parse_desc(text: &str, dir_name: &str) -> Option<Package> {
    let m = parse_sections(text);

    let optdepends = parse_optdepends(&m);

    Some(Package {
        name: one(&m, "NAME")?,
        version: one(&m, "VERSION")?,
        base: one(&m, "BASE"),
        desc: one(&m, "DESC"),
        url: one(&m, "URL"),
        arch: one(&m, "ARCH"),
        repo: one(&m, "INSTALLED_DB"),
        packager: one(&m, "PACKAGER"),
        build_date: one(&m, "BUILDDATE").and_then(|s| s.parse().ok()),
        install_date: one(&m, "INSTALLDATE").and_then(|s| s.parse().ok()),
        size: one(&m, "SIZE").and_then(|s| s.parse().ok()),
        groups: many(&m, "GROUPS"),
        license: many(&m, "LICENSE"),
        validation: one(&m, "VALIDATION"),
        // Any %REASON% present means "installed as a dependency"; pacman only
        // ever writes `1` here, and omits the key entirely for explicit installs.
        reason: if m.contains_key("REASON") {
            Reason::Dependency
        } else {
            Reason::Explicit
        },
        depends: deps(&m, "DEPENDS"),
        optdepends,
        provides: deps(&m, "PROVIDES"),
        conflicts: deps(&m, "CONFLICTS"),
        replaces: deps(&m, "REPLACES"),
        dir_name: dir_name.to_string(),
    })
}

/// The file list of one package, as stored in its `files` entry.
#[derive(Debug, Clone, Default)]
pub struct FileList {
    /// Paths relative to `/`, exactly as pacman stores them. Directory entries
    /// keep their trailing slash.
    pub files: Vec<String>,
    /// `%BACKUP%` — config files pacman tracks for user modification. Retained
    /// because it is the foundation of the deferred feature in spec §14 and
    /// because removal must warn about `.pacsave` leftovers (§6.3).
    pub backup: Vec<String>,
}

pub fn parse_files(text: &str) -> FileList {
    let m: Sections = parse_sections(text);
    FileList {
        files: many(&m, "FILES"),
        // Backup lines are `path\thash`; we only need the path.
        backup: m
            .get("BACKUP")
            .map(|v| {
                v.iter()
                    .map(|l| l.split('\t').next().unwrap_or(l).to_string())
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// A parsed local database, plus any entries we failed to read.
#[derive(Debug, Default)]
pub struct LocalDb {
    pub packages: Vec<Package>,
    /// Directories that could not be parsed, with the reason. Surfaced in the UI
    /// rather than silently dropped — a package we cannot see is a hole in the
    /// dependency graph, and silent holes produce wrong orphan results.
    pub errors: Vec<(String, String)>,
    pub root: PathBuf,
}

impl LocalDb {
    /// The conventional location. Separate from `load` so tests can point at a
    /// fixture tree.
    pub const DEFAULT_ROOT: &'static str = "/var/lib/pacman/local";

    pub fn load(root: impl AsRef<Path>) -> anyhow::Result<Self> {
        let root = root.as_ref();
        let mut db = LocalDb {
            root: root.to_path_buf(),
            ..Default::default()
        };

        let entries = fs::read_dir(root)
            .map_err(|e| anyhow::anyhow!("cannot read pacman local db at {}: {e}", root.display()))?;

        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    db.errors.push((root.display().to_string(), e.to_string()));
                    continue;
                }
            };
            let path = entry.path();
            if !path.is_dir() {
                // `ALPM_DB_VERSION` and the lock file live here too.
                continue;
            }
            let dir_name = entry.file_name().to_string_lossy().into_owned();
            let desc_path = path.join("desc");
            match fs::read_to_string(&desc_path) {
                Ok(text) => match parse_desc(&text, &dir_name) {
                    Some(pkg) => db.packages.push(pkg),
                    None => db
                        .errors
                        .push((dir_name, "desc missing %NAME% or %VERSION%".into())),
                },
                Err(e) => db.errors.push((dir_name, format!("desc: {e}"))),
            }
        }

        db.packages.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(db)
    }

    /// Reads one package's file list on demand. The full index is built
    /// separately (`data::fileindex`) because reading all of them is the
    /// expensive part of startup.
    pub fn read_files(&self, pkg: &Package) -> anyhow::Result<FileList> {
        let path = self.root.join(&pkg.dir_name).join("files");
        match fs::read_to_string(&path) {
            Ok(text) => Ok(parse_files(&text)),
            // A package legitimately may have no `files` entry.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(FileList::default()),
            Err(e) => Err(anyhow::anyhow!("{}: {e}", path.display())),
        }
    }

    pub fn explicit_count(&self) -> usize {
        self.packages
            .iter()
            .filter(|p| p.reason == Reason::Explicit)
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILELIGHT: &str = "\
%NAME%
filelight

%VERSION%
26.04.3-1.1

%BASE%
filelight

%DESC%
View disk usage information

%URL%
https://apps.kde.org/filelight/

%ARCH%
x86_64_v3

%INSTALLED_DB%
cachyos-extra-v3

%BUILDDATE%
1782999509

%INSTALLDATE%
1783277044

%SIZE%
1580620

%GROUPS%
kde-applications
kde-utilities

%LICENSE%
GPL-2.0-or-later
LGPL-2.0-or-later

%DEPENDS%
glibc
kconfig
qt6-base

%XDATA%
pkgtype=pkg
";

    #[test]
    fn parses_a_real_desc() {
        let p = parse_desc(FILELIGHT, "filelight-26.04.3-1.1").unwrap();
        assert_eq!(p.name, "filelight");
        assert_eq!(p.version, "26.04.3-1.1");
        assert_eq!(p.desc.as_deref(), Some("View disk usage information"));
        assert_eq!(p.repo.as_deref(), Some("cachyos-extra-v3"));
        assert_eq!(p.size, Some(1580620));
        assert_eq!(p.groups, ["kde-applications", "kde-utilities"]);
        assert_eq!(p.license.len(), 2);
        assert_eq!(p.depends.len(), 3);
        assert_eq!(p.install_date, Some(1783277044));
    }

    #[test]
    fn absent_reason_means_explicit() {
        let p = parse_desc(FILELIGHT, "d").unwrap();
        assert_eq!(p.reason, Reason::Explicit);

        let dep = format!("{FILELIGHT}\n%REASON%\n1\n");
        assert_eq!(parse_desc(&dep, "d").unwrap().reason, Reason::Dependency);
    }

    #[test]
    fn missing_installed_db_is_not_evidence_of_being_foreign() {
        // Regression guard for a wrong assumption: %INSTALLED_DB% is absent on
        // anything installed by an older pacman. On the dev machine 520 of 1656
        // packages lack it while only 11 are actually foreign. Origin must come
        // from a sync-db lookup; all we may conclude here is "unknown".
        let text = FILELIGHT.replace("%INSTALLED_DB%\ncachyos-extra-v3\n\n", "");
        let p = parse_desc(&text, "d").unwrap();
        assert!(p.repo.is_none());
    }

    #[test]
    fn optdepends_keep_their_reason_text() {
        let text = format!(
            "{FILELIGHT}\n%OPTDEPENDS%\nffmpeg: video thumbnails\nkdegraphics-thumbnailers\n"
        );
        let p = parse_desc(&text, "d").unwrap();
        assert_eq!(p.optdepends.len(), 2);
        assert_eq!(p.optdepends[0].dep.name, "ffmpeg");
        assert_eq!(p.optdepends[0].reason.as_deref(), Some("video thumbnails"));
        assert_eq!(p.optdepends[1].dep.name, "kdegraphics-thumbnailers");
        assert_eq!(p.optdepends[1].reason, None);
    }

    #[test]
    fn missing_name_is_rejected_not_panicked() {
        assert!(parse_desc("%VERSION%\n1.0\n", "d").is_none());
        assert!(parse_desc("", "d").is_none());
    }

    #[test]
    fn tolerates_missing_trailing_blank_line_and_crlf() {
        let p = parse_desc("%NAME%\r\nfoo\r\n\r\n%VERSION%\r\n1.0", "d").unwrap();
        assert_eq!(p.name, "foo");
        assert_eq!(p.version, "1.0");
    }

    #[test]
    fn parses_files_and_backup() {
        let fl = parse_files(
            "%FILES%\nusr/\nusr/bin/filelight\netc/foo.conf\n\n%BACKUP%\netc/foo.conf\tabc123\n",
        );
        assert_eq!(fl.files.len(), 3);
        assert_eq!(fl.backup, ["etc/foo.conf"]);
    }
}
