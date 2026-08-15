//! Reverse index: filesystem path → owning package.
//!
//! This is what makes app discovery possible at all. Layer 2 finds ~260
//! `.desktop` files and every one must be attributed to a package; Layer 1 does
//! the same for ~95 metainfo files. Shelling out to `pacman -Qo` per file would
//! be ~350 subprocess spawns and turn startup into tens of seconds (spec §13.3).
//!
//! Building the index means reading every package's `files` entry — the single
//! most expensive thing the tool does at startup — so the result is cached to
//! `$XDG_CACHE_HOME/apothiki/` and reused until the local database changes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::data::local::LocalDb;

/// Bumped whenever the on-disk layout of the cache changes.
///
/// Without this a cache written by an older build is *misparsed* rather than
/// rejected, which produces wrong ownership answers rather than an obvious
/// failure (spec §13.14).
const CACHE_FORMAT_VERSION: u32 = 1;

/// Identifies the state of the local database the cache was built from.
///
/// The mtime of `/var/lib/pacman/local` changes whenever a package directory is
/// added or removed. Package count is a cheap second signal that catches the
/// case where an install and a removal land within one mtime granularity tick.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Stamp {
    version: u32,
    /// Seconds since the epoch; `None` if the filesystem could not report it.
    db_mtime: Option<u64>,
    package_count: usize,
}

impl Stamp {
    fn of(db: &LocalDb) -> Self {
        Stamp {
            version: CACHE_FORMAT_VERSION,
            db_mtime: std::fs::metadata(&db.root)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|d| d.as_secs()),
            package_count: db.packages.len(),
        }
    }
}

/// One indexed path, as a slice of the arena plus its owning package.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct Entry {
    offset: u32,
    len: u32,
    pkg: u32,
}

/// Path → package ownership, plus the tracked config files (`%BACKUP%`).
///
/// **Representation matters here.** The obvious `HashMap<String, u32>` costs
/// ~100 ms to rebuild on every cache load — half the entire startup budget —
/// because deserialising it means 450k individual string allocations and hash
/// insertions. Instead all paths live concatenated in one arena string, with a
/// sorted table of (offset, len, package) beside it. Loading is then two bulk
/// allocations, and lookup is a binary search: ~19 comparisons against 450k
/// entries, which is not measurable next to the surrounding I/O.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct FileIndex {
    stamp: Option<Stamp>,
    /// Package names, indexed by `Entry::pkg`.
    packages: Vec<String>,
    /// Every indexed path, concatenated with no separator.
    arena: String,
    /// Sorted by path, so `owner` can binary search. Only regular files appear:
    /// directory entries are owned by every package installing into them, so an
    /// "owner" for one is meaningless, and they are ~a fifth of all entries.
    entries: Vec<Entry>,
    /// Package index → its `%BACKUP%` paths. Kept for removal warnings about
    /// `.pacsave` leftovers (spec §6.3) and the deferred §14 feature.
    backups: HashMap<u32, Vec<String>>,
}

fn slice<'a>(arena: &'a str, e: &Entry) -> &'a str {
    &arena[e.offset as usize..(e.offset + e.len) as usize]
}

impl FileIndex {
    /// Reads every package's file list and builds the index. Does not consult
    /// the cache — see [`FileIndex::load_or_build`].
    pub fn build(db: &LocalDb) -> Self {
        let mut idx = FileIndex {
            stamp: Some(Stamp::of(db)),
            packages: Vec::with_capacity(db.packages.len()),
            arena: String::with_capacity(db.packages.len() * 12_000),
            entries: Vec::with_capacity(db.packages.len() * 280),
            backups: HashMap::new(),
        };

        for pkg in &db.packages {
            let pi = idx.packages.len() as u32;
            idx.packages.push(pkg.name.clone());

            let Ok(files) = db.read_files(pkg) else {
                // A package whose file list we cannot read simply owns nothing
                // as far as the index is concerned. It stays a graph node.
                continue;
            };

            for path in files.files {
                if path.ends_with('/') {
                    continue;
                }
                idx.entries.push(Entry {
                    offset: idx.arena.len() as u32,
                    len: path.len() as u32,
                    pkg: pi,
                });
                idx.arena.push_str(&path);
            }

            if !files.backup.is_empty() {
                idx.backups.insert(pi, files.backup);
            }
        }

        // Sorting by path is what makes lookup possible. Duplicates would mean
        // two packages owning the same regular file, which pacman refuses to
        // install, so no deduplication pass is warranted.
        let arena = std::mem::take(&mut idx.arena);
        idx.entries
            .sort_unstable_by(|a, b| slice(&arena, a).cmp(slice(&arena, b)));
        idx.arena = arena;

        idx
    }

    /// The package owning an absolute or relative path, if any.
    ///
    /// Accepts both `/usr/share/applications/foo.desktop` and the
    /// `usr/share/...` form pacman stores, since callers naturally hold
    /// absolute paths from directory scans.
    pub fn owner(&self, path: impl AsRef<Path>) -> Option<&str> {
        let p = path.as_ref().to_string_lossy();
        let key = p.strip_prefix('/').unwrap_or(&p);
        let i = self
            .entries
            .binary_search_by(|e| slice(&self.arena, e).cmp(key))
            .ok()?;
        self.packages
            .get(self.entries[i].pkg as usize)
            .map(|s| s.as_str())
    }

    /// Maps each package to one binary it installs under `/usr/bin`.
    ///
    /// Built in a single pass over the whole table and returned as a map,
    /// because the caller needs it for hundreds of packages: querying per
    /// package would rescan 450k entries each time. The index is sorted by
    /// path, not by package, so there is no cheaper ordering to exploit.
    pub fn binaries_by_package(&self) -> HashMap<&str, &str> {
        const BIN: &str = "usr/bin/";
        let mut out: HashMap<&str, &str> = HashMap::new();

        for e in &self.entries {
            let path = slice(&self.arena, e);
            let Some(name) = path.strip_prefix(BIN) else {
                continue;
            };
            if name.is_empty() || name.contains('/') {
                continue;
            }
            let Some(pkg) = self.packages.get(e.pkg as usize) else {
                continue;
            };
            out.entry(pkg.as_str()).or_insert(name);
        }

        out
    }

    pub fn backups_of(&self, package: &str) -> &[String] {
        self.packages
            .iter()
            .position(|p| p == package)
            .and_then(|i| self.backups.get(&(i as u32)))
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Default cache location, honouring `XDG_CACHE_HOME`.
    pub fn cache_path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
        Some(base.join("apothiki").join("fileindex.bin"))
    }

    /// Returns the cached index when it matches the current database state,
    /// otherwise rebuilds and writes a fresh cache.
    ///
    /// A cache that cannot be read or written is never fatal — it costs time,
    /// not correctness — so failures are reported and the index is built
    /// directly.
    pub fn load_or_build(db: &LocalDb) -> (Self, CacheOutcome) {
        let want = Stamp::of(db);
        let path = match Self::cache_path() {
            Some(p) => p,
            None => return (Self::build(db), CacheOutcome::Unavailable),
        };

        if let Ok(bytes) = std::fs::read(&path) {
            match postcard::from_bytes::<FileIndex>(&bytes) {
                Ok(idx) if idx.stamp.as_ref() == Some(&want) => {
                    return (idx, CacheOutcome::Hit);
                }
                // Either the database moved on or the file is from an older
                // format. Both mean "rebuild"; neither is an error worth
                // showing the user.
                _ => {}
            }
        }

        let idx = Self::build(db);
        let outcome = match idx.write_cache(&path) {
            Ok(()) => CacheOutcome::Rebuilt,
            Err(e) => CacheOutcome::RebuiltUncached(e.to_string()),
        };
        (idx, outcome)
    }

    fn write_cache(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let bytes = postcard::to_stdvec(self)?;
        // Write-then-rename so a crash mid-write cannot leave a truncated cache
        // that the next run would have to detect.
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheOutcome {
    Hit,
    Rebuilt,
    /// Built, but the cache could not be written; next start pays the cost again.
    RebuiltUncached(String),
    /// No cache directory could be determined.
    Unavailable,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Builds a throwaway local-db tree so index behaviour can be tested
    /// without touching the real system.
    fn fixture(dir: &Path, entries: &[(&str, &str, &str)]) -> LocalDb {
        for (dir_name, desc, files) in entries {
            let d = dir.join(dir_name);
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("desc"), desc).unwrap();
            if !files.is_empty() {
                fs::write(d.join("files"), files).unwrap();
            }
        }
        LocalDb::load(dir).unwrap()
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("apothiki-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn resolves_ownership_both_ways() {
        let dir = tmpdir("owner");
        let db = fixture(
            &dir,
            &[(
                "filelight-1.0-1",
                "%NAME%\nfilelight\n\n%VERSION%\n1.0-1\n",
                "%FILES%\nusr/\nusr/share/applications/\nusr/share/applications/org.kde.filelight.desktop\n",
            )],
        );
        let idx = FileIndex::build(&db);

        // Absolute (as a directory scan yields) and relative (as pacman stores).
        assert_eq!(
            idx.owner("/usr/share/applications/org.kde.filelight.desktop"),
            Some("filelight")
        );
        assert_eq!(
            idx.owner("usr/share/applications/org.kde.filelight.desktop"),
            Some("filelight")
        );
        assert_eq!(idx.owner("/usr/bin/nonexistent"), None);

        // Directories are not indexed; only the one real file is.
        assert_eq!(idx.len(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn keeps_backup_paths() {
        let dir = tmpdir("backup");
        let db = fixture(
            &dir,
            &[(
                "foo-1-1",
                "%NAME%\nfoo\n\n%VERSION%\n1-1\n",
                "%FILES%\netc/foo.conf\n\n%BACKUP%\netc/foo.conf\tdeadbeef\n",
            )],
        );
        let idx = FileIndex::build(&db);
        assert_eq!(idx.backups_of("foo"), ["etc/foo.conf"]);
        assert!(idx.backups_of("nonexistent").is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cache_round_trips_and_is_rejected_when_stale() {
        let dir = tmpdir("cache");
        let db = fixture(
            &dir,
            &[(
                "foo-1-1",
                "%NAME%\nfoo\n\n%VERSION%\n1-1\n",
                "%FILES%\nusr/bin/foo\n",
            )],
        );
        let idx = FileIndex::build(&db);
        let cache = dir.join("cache.bin");
        idx.write_cache(&cache).unwrap();

        let bytes = fs::read(&cache).unwrap();
        let back: FileIndex = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(back.owner("/usr/bin/foo"), Some("foo"));
        assert_eq!(back.stamp, idx.stamp);

        // A stamp from a different database state must not be accepted.
        let other = Stamp {
            version: CACHE_FORMAT_VERSION,
            db_mtime: Some(0),
            package_count: 999,
        };
        assert_ne!(back.stamp.as_ref(), Some(&other));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn package_without_files_entry_is_not_fatal() {
        let dir = tmpdir("nofiles");
        let db = fixture(&dir, &[("bare-1-1", "%NAME%\nbare\n\n%VERSION%\n1-1\n", "")]);
        let idx = FileIndex::build(&db);
        assert!(idx.is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }
}
