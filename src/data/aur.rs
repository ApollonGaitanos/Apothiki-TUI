//! The AUR package index (spec §7.2).
//!
//! **The RPC endpoint is not usable for as-you-type search.** It rate-limits,
//! caps results, and costs 200–800 ms per call; querying it per keystroke gets
//! you throttled and still does not feel instant. So the whole package list is
//! downloaded once and searched locally, which puts AUR results on exactly the
//! same code path — and the same latency — as repository results.
//!
//! The download is ~15 MB compressed and happens on a background thread. Until
//! it lands, repository search works normally and the AUR section says so:
//! blocking the UI on a cold start would trade one bad experience for another.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Bumped when the cached layout changes, so an old cache is discarded rather
/// than misread.
const CACHE_VERSION: u32 = 1;

/// How old the index may get before a refresh is attempted.
const MAX_AGE_SECS: u64 = 24 * 60 * 60;

const URL: &str = "https://aur.archlinux.org/packages-meta-ext-v1.json.gz";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AurPackage {
    pub name: String,
    pub version: String,
    pub description: Option<String>,
    pub votes: u32,
    pub popularity: f64,
    /// Unix timestamp when flagged out of date, if it is.
    pub out_of_date: Option<i64>,
    pub maintainer: Option<String>,
}

impl AurPackage {
    /// An unmaintained package is a real risk signal, not a detail: nobody is
    /// applying upstream fixes, and the build may simply stop working.
    pub fn is_orphaned(&self) -> bool {
        self.maintainer.is_none()
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct AurIndex {
    /// Cache format version, checked on load so a stale layout is discarded
    /// rather than misread.
    pub version: u32,
    pub fetched_at: u64,
    pub packages: Vec<AurPackage>,
}

impl AurIndex {
    pub fn cache_path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
        Some(base.join("apothiki").join("aur.bin"))
    }

    pub fn is_stale(&self) -> bool {
        now().saturating_sub(self.fetched_at) > MAX_AGE_SECS
    }

    pub fn len(&self) -> usize {
        self.packages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.packages.is_empty()
    }

    /// Loads the cached index, if one exists and is the current format.
    pub fn load_cached() -> Option<Self> {
        let bytes = std::fs::read(Self::cache_path()?).ok()?;
        let index: AurIndex = postcard::from_bytes(&bytes).ok()?;
        (index.version == CACHE_VERSION).then_some(index)
    }

    fn save(&self) -> anyhow::Result<()> {
        let Some(path) = Self::cache_path() else {
            anyhow::bail!("no cache directory");
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let bytes = postcard::to_stdvec(self)?;
        // Write-then-rename: a crash mid-write must not leave a truncated cache
        // that the next run has to detect.
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Downloads and parses the full package list, then caches it.
    pub fn fetch() -> anyhow::Result<Self> {
        let response = ureq::get(URL)
            .call()
            .map_err(|e| anyhow::anyhow!("could not reach the AUR: {e}"))?;

        let mut gz = Vec::new();
        std::io::Read::read_to_end(&mut response.into_body().into_reader(), &mut gz)?;

        let mut json = String::new();
        std::io::Read::read_to_string(
            &mut flate2::read::GzDecoder::new(&gz[..]),
            &mut json,
        )?;

        let index = Self::parse(&json)?;
        // A cache we cannot write costs time on the next start, not correctness.
        let _ = index.save();
        Ok(index)
    }

    /// Parses the metadata dump.
    ///
    /// Only the fields search and ranking need are kept; the dump also carries
    /// dependency arrays, licences and keywords, which would multiply the
    /// memory cost of a list this size for no benefit here.
    pub fn parse(json: &str) -> anyhow::Result<Self> {
        let raw: Vec<serde_json::Value> = serde_json::from_str(json)?;
        let mut packages = Vec::with_capacity(raw.len());

        for item in raw {
            let Some(name) = item.get("Name").and_then(|v| v.as_str()) else {
                continue;
            };
            packages.push(AurPackage {
                name: name.to_string(),
                version: item
                    .get("Version")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                description: item
                    .get("Description")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                votes: item
                    .get("NumVotes")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32,
                popularity: item
                    .get("Popularity")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0),
                out_of_date: item.get("OutOfDate").and_then(|v| v.as_i64()),
                maintainer: item
                    .get("Maintainer")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
            });
        }

        Ok(AurIndex {
            version: CACHE_VERSION,
            fetched_at: now(),
            packages,
        })
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// What the UI knows about the AUR index at any moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AurState {
    /// No index yet; a download is running.
    Downloading,
    Ready,
    /// The download failed. Repository search is unaffected.
    Failed,
    /// No index and no attempt made.
    Absent,
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"[
      {"Name":"discord-canary","Version":"1.0-1","Description":"Canary build",
       "NumVotes":42,"Popularity":1.5,"OutOfDate":null,"Maintainer":"someone"},
      {"Name":"orphaned-thing","Version":"2.0-1","Description":null,
       "NumVotes":0,"Popularity":0.0,"OutOfDate":1700000000,"Maintainer":null},
      {"NoName":"ignored"}
    ]"#;

    #[test]
    fn parses_the_metadata_dump() {
        let index = AurIndex::parse(SAMPLE).unwrap();
        // The entry without a Name is skipped rather than aborting the parse.
        assert_eq!(index.len(), 2);

        let d = &index.packages[0];
        assert_eq!(d.name, "discord-canary");
        assert_eq!(d.votes, 42);
        assert_eq!(d.description.as_deref(), Some("Canary build"));
        assert!(!d.is_orphaned());
    }

    #[test]
    fn orphaned_and_out_of_date_packages_are_flagged() {
        let index = AurIndex::parse(SAMPLE).unwrap();
        let o = &index.packages[1];
        assert!(o.is_orphaned(), "no maintainer means orphaned");
        assert_eq!(o.out_of_date, Some(1_700_000_000));
        assert_eq!(o.description, None);
    }

    #[test]
    fn malformed_json_is_an_error_not_a_panic() {
        assert!(AurIndex::parse("not json").is_err());
        assert!(AurIndex::parse("{}").is_err());
        assert_eq!(AurIndex::parse("[]").unwrap().len(), 0);
    }

    #[test]
    fn a_fresh_index_is_not_stale() {
        let mut index = AurIndex::parse("[]").unwrap();
        assert!(!index.is_stale());
        index.fetched_at = now().saturating_sub(MAX_AGE_SECS + 1);
        assert!(index.is_stale());
    }

    #[test]
    fn cache_round_trips() {
        let index = AurIndex::parse(SAMPLE).unwrap();
        let bytes = postcard::to_stdvec(&index).unwrap();
        let back: AurIndex = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back.packages[0].name, "discord-canary");
        assert_eq!(back.version, CACHE_VERSION);
    }
}
