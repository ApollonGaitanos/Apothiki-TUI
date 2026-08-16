//! Reader for `/var/lib/pacman/sync/<repo>.db` — the package lists of the
//! configured repositories.
//!
//! Each `.db` is a gzipped tar whose members are `<pkg>-<ver>/desc` in the same
//! `%KEY%` format as the local database, so parsing is shared with
//! [`crate::data::descfmt`].
//!
//! Two jobs:
//!
//! 1. **Origin and foreign detection.** A locally installed package whose name
//!    appears in no sync database is foreign (AUR or hand-built). This is the
//!    only reliable test — `%INSTALLED_DB%` is absent on anything installed by
//!    an older pacman, so its absence proves nothing (see [`super::local`]).
//! 2. **The install/search corpus** for spec §7. Roughly 15k entries with names
//!    and descriptions, held in memory so search never touches disk.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Seek};
use std::path::{Path, PathBuf};

use crate::data::dep::Dep;
use crate::data::descfmt::{deps, many, one, parse_sections};

// Mirrors the on-disk record rather than only the parts currently consumed.
// Dropping a field would mean the parser silently discards it, and the next
// reader of this struct would have no way to tell that a sync database entry carries it at all.
#[allow(dead_code)]
/// A package as described by a repository, i.e. available rather than installed.
#[derive(Debug, Clone)]
pub struct SyncPackage {
    pub name: String,
    pub version: String,
    pub desc: Option<String>,
    pub url: Option<String>,
    /// Repo name, taken from the database filename rather than the entry.
    pub repo: String,
    /// Download size (`%CSIZE%`), bytes.
    pub csize: Option<u64>,
    /// Installed size (`%ISIZE%`), bytes.
    pub isize: Option<u64>,
    pub groups: Vec<String>,
    pub provides: Vec<Dep>,
    pub depends: Vec<Dep>,
}

#[derive(Debug, Default)]
pub struct SyncDb {
    /// All entries across all repos, in the order the repos were read.
    pub packages: Vec<SyncPackage>,
    /// Repo names in the order discovered.
    pub repos: Vec<String>,
    pub errors: Vec<(String, String)>,
    /// name → index into `packages`. When several repos carry the same name
    /// (routine on CachyOS, which shadows Arch packages), this holds the first
    /// one read; `all_providers_of` gives the full set.
    by_name: HashMap<String, usize>,
    /// name → every index carrying that name.
    all_by_name: HashMap<String, Vec<usize>>,
}

impl SyncDb {
    pub const DEFAULT_ROOT: &'static str = "/var/lib/pacman/sync";

    /// Reads every `*.db` under `root`.
    ///
    /// A repo that fails to read is recorded in `errors` and skipped rather than
    /// aborting the load: a missing repo degrades origin detection for some
    /// packages, but an unreadable one must not take down the whole tool.
    pub fn load(root: impl AsRef<Path>) -> anyhow::Result<Self> {
        let root = root.as_ref();
        let mut db = SyncDb::default();

        let mut db_files: Vec<PathBuf> = match fs::read_dir(root) {
            Ok(entries) => entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "db"))
                .collect(),
            Err(e) => {
                // No sync databases at all is survivable: everything installed
                // simply reads as origin-unknown.
                db.errors.push((root.display().to_string(), e.to_string()));
                return Ok(db);
            }
        };
        db_files.sort();

        for path in db_files {
            let repo = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            match Self::read_one(&path, &repo) {
                Ok(pkgs) => {
                    db.repos.push(repo);
                    for p in pkgs {
                        let idx = db.packages.len();
                        db.by_name.entry(p.name.clone()).or_insert(idx);
                        db.all_by_name.entry(p.name.clone()).or_default().push(idx);
                        db.packages.push(p);
                    }
                }
                Err(e) => db.errors.push((repo, e.to_string())),
            }
        }

        Ok(db)
    }

    fn read_one(path: &Path, repo: &str) -> anyhow::Result<Vec<SyncPackage>> {
        let reader = open_decompressed(path)?;
        let mut archive = tar::Archive::new(reader);
        let mut out = Vec::new();
        let mut buf = String::new();

        for entry in archive.entries()? {
            let mut entry = entry?;
            // Members are `<pkg>-<ver>/desc`, plus `depends` and `files` in some
            // repo layouts. Only `desc` is needed.
            let is_desc = entry
                .path()
                .map(|p| p.file_name().is_some_and(|n| n == "desc"))
                .unwrap_or(false);
            if !is_desc {
                continue;
            }
            buf.clear();
            if entry.read_to_string(&mut buf).is_err() {
                // Non-UTF8 in one member should not kill the repo.
                continue;
            }
            if let Some(p) = parse_sync_desc(&buf, repo) {
                out.push(p);
            }
        }
        Ok(out)
    }

    /// True when no repository lists this name — the reliable foreign test.
    pub fn is_foreign(&self, name: &str) -> bool {
        !self.by_name.contains_key(name)
    }

    pub fn get(&self, name: &str) -> Option<&SyncPackage> {
        self.by_name.get(name).map(|&i| &self.packages[i])
    }

    /// Every repo entry carrying this name. More than one means the package is
    /// shadowed — e.g. CachyOS's optimised build of an Arch package — which the
    /// detail pane must disclose (spec §11).
    pub fn all_providers_of(&self, name: &str) -> Vec<&SyncPackage> {
        self.all_by_name
            .get(name)
            .map(|v| v.iter().map(|&i| &self.packages[i]).collect())
            .unwrap_or_default()
    }
}

/// The compression a `.db` file actually uses.
///
/// **Do not assume gzip.** The extension is always `.db` and says nothing:
/// Arch's own repos ship gzip, while CachyOS ships zstd. Guessing from the
/// filename silently drops entire repositories, which then reads downstream as
/// "these packages are foreign" — a wrong answer that looks plausible.
/// Sniff the magic bytes instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    Gzip,
    Zstd,
    /// Anything else. pacman also accepts an uncompressed tar, but its magic
    /// lives at offset 257 and cannot be seen in the four bytes sniffed here;
    /// no repository ships one, so it is reported rather than guessed at.
    Unknown([u8; 4]),
}

pub fn sniff(magic: &[u8]) -> Compression {
    match magic {
        [0x1f, 0x8b, ..] => Compression::Gzip,
        [0x28, 0xb5, 0x2f, 0xfd, ..] => Compression::Zstd,
        _ => {
            let mut m = [0u8; 4];
            for (i, b) in magic.iter().take(4).enumerate() {
                m[i] = *b;
            }
            Compression::Unknown(m)
        }
    }
}

fn open_decompressed(path: &Path) -> anyhow::Result<Box<dyn Read>> {
    let mut file = File::open(path)?;
    let mut magic = [0u8; 4];
    let n = file.read(&mut magic)?;
    file.seek(std::io::SeekFrom::Start(0))?;

    match sniff(&magic[..n]) {
        Compression::Gzip => Ok(Box::new(flate2::read::GzDecoder::new(file))),
        Compression::Zstd => Ok(Box::new(zstd::stream::read::Decoder::new(file)?)),
        Compression::Unknown(m) => Err(anyhow::anyhow!(
            "unrecognised compression (magic {m:02x?}); expected gzip or zstd"
        )),
    }
}

fn parse_sync_desc(text: &str, repo: &str) -> Option<SyncPackage> {
    let m = parse_sections(text);
    Some(SyncPackage {
        name: one(&m, "NAME")?,
        version: one(&m, "VERSION").unwrap_or_default(),
        desc: one(&m, "DESC"),
        url: one(&m, "URL"),
        repo: repo.to_string(),
        csize: one(&m, "CSIZE").and_then(|s| s.parse().ok()),
        isize: one(&m, "ISIZE").and_then(|s| s.parse().ok()),
        groups: many(&m, "GROUPS"),
        provides: deps(&m, "PROVIDES"),
        depends: deps(&m, "DEPENDS"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_sync_desc_entry() {
        let text = "%FILENAME%\nbzip2-1.0.8-6.1-x86_64_v3.pkg.tar.zst\n\n\
                    %NAME%\nbzip2\n\n%VERSION%\n1.0.8-6.1\n\n\
                    %DESC%\nA high-quality data compression program\n\n\
                    %CSIZE%\n52000\n\n%ISIZE%\n200000\n\n\
                    %PROVIDES%\nlibbz2.so=1.0-64\n\n%DEPENDS%\nglibc\n";
        let p = parse_sync_desc(text, "cachyos-core-v3").unwrap();
        assert_eq!(p.name, "bzip2");
        assert_eq!(p.repo, "cachyos-core-v3");
        assert_eq!(p.isize, Some(200000));
        assert_eq!(p.provides[0].name, "libbz2.so");
        assert_eq!(p.depends[0].name, "glibc");
    }

    #[test]
    fn sniffs_both_compressions_used_by_real_repos() {
        // Regression guard: assuming gzip silently dropped all four CachyOS
        // repos, which then made 67 packages look foreign instead of 11.
        assert_eq!(sniff(&[0x1f, 0x8b, 0x08, 0x00]), Compression::Gzip);
        assert_eq!(sniff(&[0x28, 0xb5, 0x2f, 0xfd]), Compression::Zstd);
        assert!(matches!(
            sniff(&[0x00, 0x01, 0x02, 0x03]),
            Compression::Unknown(_)
        ));
        assert!(matches!(sniff(&[]), Compression::Unknown(_)));
    }

    #[test]
    fn entry_without_name_is_skipped() {
        assert!(parse_sync_desc("%VERSION%\n1\n", "core").is_none());
    }

    #[test]
    fn missing_sync_dir_is_survivable() {
        let db = SyncDb::load("/nonexistent/apothiki-test").unwrap();
        assert!(db.packages.is_empty());
        assert_eq!(db.errors.len(), 1);
        // With no sync data everything reads as foreign; callers must treat an
        // empty SyncDb as "origin unknown" rather than trusting this.
        assert!(db.is_foreign("bzip2"));
    }
}
