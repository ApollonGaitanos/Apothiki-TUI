//! Unified search across repositories and the AUR (spec §7.3).
//!
//! The requirement is specific: typing `dis` must put `discord` at the top, not
//! `libdiscid`. A raw fuzzy score does not do that on its own — fuzzy matchers
//! reward short candidates and scattered character hits, so `dis` scores
//! `libdiscid` highly for exactly the wrong reason.
//!
//! So matching and ranking are separated. `nucleo` decides *whether* a
//! candidate matches and how cleanly; the tiers below decide *what order* the
//! matches appear in. Tier always outranks fuzzy score.

use nucleo::pattern::{CaseMatching, Normalization, Pattern};
use nucleo::Matcher;

use crate::data::aur::AurIndex;
use crate::data::local::LocalDb;
use crate::data::sync::SyncDb;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// An official or distro repository.
    Repo,
    Aur,
}

#[derive(Debug, Clone)]
pub struct Hit {
    pub name: String,
    pub version: String,
    pub description: Option<String>,
    pub origin: Origin,
    /// Repository name, for repo packages.
    pub repo: Option<String>,
    pub installed: bool,
    pub votes: u32,
    /// The AUR's `OutOfDate` flag: **users have reported that the packaging is
    /// behind upstream**. It says nothing about whether *your* system needs an
    /// update — that is a separate question answered by `ops::update`.
    pub out_of_date: bool,
    /// No maintainer. Nobody is updating the packaging or fixing it when it
    /// stops building.
    pub orphaned: bool,
    score: u32,
    tier: Tier,
}

/// Match quality, in the order a human expects to see it.
///
/// Ordered so that `derive(Ord)` sorts best-first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Tier {
    ExactName,
    NamePrefix,
    NameContains,
    /// Fuzzy hit on the name — characters in order but not contiguous.
    NameFuzzy,
    DescriptionOnly,
}

fn tier_of(name: &str, query: &str) -> Option<Tier> {
    let (n, q) = (name.to_lowercase(), query.to_lowercase());
    if n == q {
        Some(Tier::ExactName)
    } else if n.starts_with(&q) {
        Some(Tier::NamePrefix)
    } else if n.contains(&q) {
        Some(Tier::NameContains)
    } else {
        None
    }
}

pub struct Searcher {
    matcher: Matcher,
}

impl Default for Searcher {
    fn default() -> Self {
        Self::new()
    }
}

impl Searcher {
    pub fn new() -> Self {
        Searcher {
            matcher: Matcher::new(nucleo::Config::DEFAULT),
        }
    }

    /// Searches repositories and the AUR, returning ranked results.
    ///
    /// `limit` bounds the work of sorting ~140k candidates when only a
    /// screenful is ever shown.
    pub fn search(
        &mut self,
        query: &str,
        sync: Option<&SyncDb>,
        aur: Option<&AurIndex>,
        installed: &LocalDb,
        limit: usize,
    ) -> Vec<Hit> {
        if query.trim().is_empty() {
            return Vec::new();
        }

        let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
        let needle = query.trim().to_lowercase();

        // Candidates are collected as lightweight references and only
        // materialised into owned `Hit`s after sorting and truncation. Cloning
        // three strings per match before sorting cost more than the matching
        // itself once the AUR's 117k packages were in play.
        let mut found: Vec<Candidate> = Vec::new();
        let mut buf = Vec::new();

        let is_installed = |name: &str| {
            installed
                .packages
                .binary_search_by(|p| p.name.as_str().cmp(name))
                .is_ok()
        };

        if let Some(sync) = sync {
            // The same name often exists in several repos (CachyOS shadows
            // Arch). Only the first is offered; four identical rows help nobody.
            let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
            for (i, p) in sync.packages.iter().enumerate() {
                if !seen.insert(p.name.as_str()) {
                    continue;
                }
                if let Some((tier, score)) = self.score_name(&pattern, &needle, &p.name, &mut buf) {
                    found.push(Candidate {
                        source: Source::Repo(i),
                        tier,
                        score,
                        installed: is_installed(&p.name),
                        name_len: p.name.len() as u32,
                    });
                }
            }
        }

        if let Some(aur) = aur {
            for (i, p) in aur.packages.iter().enumerate() {
                if let Some((tier, score)) = self.score_name(&pattern, &needle, &p.name, &mut buf) {
                    found.push(Candidate {
                        source: Source::Aur(i),
                        tier,
                        score,
                        installed: is_installed(&p.name),
                        name_len: p.name.len() as u32,
                    });
                }
            }
        }

        // Descriptions are only searched when names did not produce enough to
        // fill the screen. For a query like `dis` the name pass alone yields
        // hundreds of hits, and scoring 140k descriptions to add results nobody
        // will scroll to is the difference between 13 ms and 115 ms.
        if found.len() < limit {
            if let Some(sync) = sync {
                for (i, p) in sync.packages.iter().enumerate() {
                    if let Some(desc) = p.desc.as_deref() {
                        if self.matches(&pattern, desc, &mut buf).is_some()
                            && !found.iter().any(|c| c.source == Source::Repo(i))
                        {
                            found.push(Candidate {
                                source: Source::Repo(i),
                                tier: Tier::DescriptionOnly,
                                score: 0,
                                installed: is_installed(&p.name),
                                name_len: p.name.len() as u32,
                            });
                        }
                    }
                }
            }
            if let Some(aur) = aur {
                for (i, p) in aur.packages.iter().enumerate() {
                    if let Some(desc) = p.description.as_deref() {
                        if self.matches(&pattern, desc, &mut buf).is_some()
                            && !found.iter().any(|c| c.source == Source::Aur(i))
                        {
                            found.push(Candidate {
                                source: Source::Aur(i),
                                tier: Tier::DescriptionOnly,
                                score: 0,
                                installed: is_installed(&p.name),
                                name_len: p.name.len() as u32,
                            });
                        }
                    }
                }
            }
        }

        let name_of = |c: &Candidate| -> &str {
            match c.source {
                Source::Repo(i) => sync.map(|s| s.packages[i].name.as_str()).unwrap_or(""),
                Source::Aur(i) => aur.map(|a| a.packages[i].name.as_str()).unwrap_or(""),
            }
        };

        let order = |a: &Candidate, b: &Candidate| {
            a.tier
                .cmp(&b.tier)
                // Already installed sorts first within a tier: the user is more
                // often looking for something they have than something they do not.
                .then(b.installed.cmp(&a.installed))
                // Official repositories before the AUR: reviewed and signed.
                .then(a.source.rank().cmp(&b.source.rank()))
                .then(b.score.cmp(&a.score))
                // Shorter names last-resort: `dis` should prefer `discord` to
                // `discord-canary-bin` when everything else ties.
                .then(a.name_len.cmp(&b.name_len))
                .then(name_of(a).cmp(name_of(b)))
        };

        // Only the top `limit` are ever shown, so the rest need partitioning,
        // not ordering. A one-letter query matches nearly everything, and fully
        // sorting 140k candidates to display fifteen was the dominant cost.
        if found.len() > limit {
            found.select_nth_unstable_by(limit - 1, order);
            found.truncate(limit);
        }
        found.sort_by(order);

        found
            .into_iter()
            .filter_map(|c| match c.source {
                Source::Repo(i) => sync.map(|s| {
                    let p = &s.packages[i];
                    Hit {
                        name: p.name.clone(),
                        version: p.version.clone(),
                        description: p.desc.clone(),
                        origin: Origin::Repo,
                        repo: Some(p.repo.clone()),
                        installed: is_installed(&p.name),
                        votes: 0,
                        out_of_date: false,
                        orphaned: false,
                        score: c.score,
                        tier: c.tier,
                    }
                }),
                Source::Aur(i) => aur.map(|a| {
                    let p = &a.packages[i];
                    Hit {
                        name: p.name.clone(),
                        version: p.version.clone(),
                        description: p.description.clone(),
                        origin: Origin::Aur,
                        repo: None,
                        installed: is_installed(&p.name),
                        votes: p.votes,
                        out_of_date: p.out_of_date.is_some(),
                        orphaned: p.is_orphaned(),
                        score: c.score,
                        tier: c.tier,
                    }
                }),
            })
            .collect()
    }

    /// Scores a name, classifying how cleanly it matched.
    fn score_name(
        &mut self,
        pattern: &Pattern,
        needle: &str,
        name: &str,
        buf: &mut Vec<char>,
    ) -> Option<(Tier, u32)> {
        let score = self.matches(pattern, name, buf)?;
        Some((tier_of(name, needle).unwrap_or(Tier::NameFuzzy), score))
    }

    /// Raw fuzzy score, reusing the caller's buffer.
    ///
    /// The buffer matters: a fresh `Vec` per candidate is 140k allocations per
    /// keystroke.
    fn matches(&mut self, pattern: &Pattern, haystack: &str, buf: &mut Vec<char>) -> Option<u32> {
        buf.clear();
        let utf32 = nucleo::Utf32Str::new(haystack, buf);
        pattern.score(utf32, &mut self.matcher)
    }
}

/// Where a candidate came from, as an index rather than a clone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Repo(usize),
    Aur(usize),
}

impl Source {
    fn rank(&self) -> u8 {
        match self {
            Source::Repo(_) => 0,
            Source::Aur(_) => 1,
        }
    }
}

struct Candidate {
    source: Source,
    tier: Tier,
    score: u32,
    /// Precomputed, not looked up in the comparator: a binary search per
    /// comparison is ~1.7M lookups when a one-letter query matches everything.
    installed: bool,
    name_len: u32,
}

impl Hit {
    /// Short label for the source column.
    pub fn source_label(&self) -> String {
        match self.origin {
            Origin::Repo => self.repo.clone().unwrap_or_else(|| "repo".into()),
            Origin::Aur => "aur".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::aur::AurPackage;
    use crate::data::sync::SyncPackage;

    fn sync_with(names: &[(&str, &str)]) -> SyncDb {
        let mut db = SyncDb::default();
        for (name, desc) in names {
            db.packages.push(SyncPackage {
                name: name.to_string(),
                version: "1-1".into(),
                desc: Some(desc.to_string()),
                url: None,
                repo: "extra".into(),
                csize: None,
                isize: None,
                groups: vec![],
                provides: vec![],
                depends: vec![],
            });
        }
        db
    }

    fn empty_local() -> LocalDb {
        LocalDb {
            packages: vec![],
            errors: vec![],
            root: Default::default(),
        }
    }

    #[test]
    fn typing_dis_puts_discord_first() {
        // The spec's acceptance criterion, verbatim: `dis` must surface
        // `discord`, not `libdiscid`.
        let sync = sync_with(&[
            ("libdiscid", "Digital Audio Disc identification"),
            ("discord", "All-in-one voice and text chat"),
            ("discount", "A Markdown compiler"),
            ("display-manager", "Manages displays"),
        ]);
        let mut s = Searcher::new();
        let hits = s.search("dis", Some(&sync), None, &empty_local(), 10);

        assert_eq!(hits[0].name, "discord", "got {:?}", names(&hits));
    }

    #[test]
    fn an_exact_name_always_wins() {
        let sync = sync_with(&[
            ("code-features", "extras"),
            ("code", "the editor"),
            ("vscodium", "another editor"),
        ]);
        let mut s = Searcher::new();
        let hits = s.search("code", Some(&sync), None, &empty_local(), 10);
        assert_eq!(hits[0].name, "code");
    }

    #[test]
    fn name_matches_outrank_description_matches() {
        let sync = sync_with(&[
            ("some-tool", "a firefox helper for browsing"),
            ("firefox", "web browser"),
        ]);
        let mut s = Searcher::new();
        let hits = s.search("firefox", Some(&sync), None, &empty_local(), 10);
        assert_eq!(hits[0].name, "firefox");
    }

    #[test]
    fn repositories_outrank_the_aur_for_equal_matches() {
        let sync = sync_with(&[("thing", "official build")]);
        let aur = AurIndex {
            version: 1,
            fetched_at: 0,
            packages: vec![AurPackage {
                name: "thing".into(),
                version: "1-1".into(),
                description: Some("aur build".into()),
                votes: 9999,
                popularity: 99.0,
                out_of_date: None,
                maintainer: Some("someone".into()),
            }],
        };
        let mut s = Searcher::new();
        let hits = s.search("thing", Some(&sync), Some(&aur), &empty_local(), 10);

        assert_eq!(hits[0].origin, Origin::Repo, "reviewed builds come first");
        assert_eq!(hits[1].origin, Origin::Aur);
    }

    #[test]
    fn duplicate_names_across_repos_appear_once() {
        // CachyOS shadows Arch packages; four identical rows help nobody.
        let mut sync = sync_with(&[("ffmpeg", "one")]);
        sync.packages.push(SyncPackage {
            name: "ffmpeg".into(),
            version: "1-1".into(),
            desc: Some("two".into()),
            url: None,
            repo: "cachyos-extra-v3".into(),
            csize: None,
            isize: None,
            groups: vec![],
            provides: vec![],
            depends: vec![],
        });
        let mut s = Searcher::new();
        let hits = s.search("ffmpeg", Some(&sync), None, &empty_local(), 10);
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn an_empty_query_returns_nothing() {
        let sync = sync_with(&[("anything", "x")]);
        let mut s = Searcher::new();
        assert!(s.search("", Some(&sync), None, &empty_local(), 10).is_empty());
        assert!(s.search("   ", Some(&sync), None, &empty_local(), 10).is_empty());
    }

    #[test]
    fn aur_risk_signals_survive_into_results() {
        let aur = AurIndex {
            version: 1,
            fetched_at: 0,
            packages: vec![AurPackage {
                name: "risky-git".into(),
                version: "1-1".into(),
                description: None,
                votes: 3,
                popularity: 0.1,
                out_of_date: Some(1),
                maintainer: None,
            }],
        };
        let mut s = Searcher::new();
        let hits = s.search("risky", None, Some(&aur), &empty_local(), 10);
        assert!(hits[0].out_of_date);
        assert!(hits[0].orphaned);
    }

    fn names(hits: &[Hit]) -> Vec<&str> {
        hits.iter().map(|h| h.name.as_str()).collect()
    }
}
