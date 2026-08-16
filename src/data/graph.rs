//! The dependency graph: provides resolution, orphan detection, and removal
//! impact simulation.
//!
//! Three questions the UI must never conflate (spec §5.2):
//!
//! 1. **Depends on** — what a package declares. Small, 5–20 entries.
//! 2. **Required by** — the reverse edges. This is the one that governs deletion.
//! 3. **What goes with it** — the simulation of `pacman -Rs`, which is what the
//!    user actually wants to know before pressing Delete.
//!
//! Everything here is computed in-process for instant feedback. Nothing here is
//! ever the basis for executing a removal: the plan is confirmed against a real
//! `pacman -Rs --print` dry-run first (§5.2, §12/M2).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use crate::data::dep::Dep;
use crate::data::local::{LocalDb, Reason};

/// Index into `LocalDb::packages`.
pub type PkgIdx = u32;

pub struct Graph {
    pub db: Arc<LocalDb>,
    /// Dependency name (real or virtual) → every installed package providing it.
    ///
    /// Owned keys rather than borrows from `db`: the graph has to live inside an
    /// owned snapshot that a background thread builds and hands to the UI, and a
    /// lifetime parameter would make that impossible.
    providers: HashMap<String, Vec<PkgIdx>>,
    /// Resolved forward edges, deduplicated.
    depends_on: Vec<Vec<PkgIdx>>,
    /// Resolved reverse edges.
    required_by: Vec<Vec<PkgIdx>>,
    /// Reverse edges from `optdepends`. Deliberately kept out of the graph
    /// proper — they are documentation, not requirements — but tracked because
    /// removing an optional dependency silently degrades whatever wanted it,
    /// which is the most common way users break their systems (spec §5.2).
    optional_for: Vec<Vec<PkgIdx>>,
    /// Dependency strings that resolved to nothing installed. Should be rare;
    /// a non-empty list usually means a parsing bug, so it is surfaced rather
    /// than swallowed.
    pub unresolved: Vec<(PkgIdx, String)>,
    /// Strongly-connected component id per package.
    ///
    /// Mutual dependencies are common in practice — `tesseract` needs
    /// `tessdata`, which `tesseract-data-eng` provides, and that in turn needs
    /// `tesseract`; `python-beautifulsoup4` and `python-soupsieve` require each
    /// other outright. Members of a cycle can only ever leave together, so
    /// removal reasons about components rather than individual packages.
    scc: Vec<u32>,
    /// Component id → its members.
    scc_members: Vec<Vec<PkgIdx>>,
}

/// Tarjan's algorithm, iteratively.
///
/// Iterative rather than recursive because the recursion depth would follow the
/// dependency chain, and a stack overflow inside the tool that is supposed to
/// tell you what is safe to delete would be a poor showing.
fn compute_sccs(n: usize, adj: &[Vec<PkgIdx>]) -> Vec<u32> {
    const UNVISITED: u32 = u32::MAX;

    let mut index = vec![UNVISITED; n];
    let mut low = vec![0u32; n];
    let mut on_stack = vec![false; n];
    let mut stack: Vec<PkgIdx> = Vec::new();
    let mut comp = vec![UNVISITED; n];
    let (mut next_index, mut next_comp) = (0u32, 0u32);

    for (start, _) in adj.iter().enumerate() {
        if index[start] != UNVISITED {
            continue;
        }
        // Each frame is (node, next child to examine).
        let mut frames: Vec<(PkgIdx, usize)> = vec![(start as u32, 0)];

        while let Some((v, child)) = frames.pop() {
            let vi = v as usize;
            if child == 0 {
                index[vi] = next_index;
                low[vi] = next_index;
                next_index += 1;
                stack.push(v);
                on_stack[vi] = true;
            }

            let mut descended = false;
            // Indexed rather than iterated because the position is saved and
            // resumed: descending into a child suspends this frame mid-scan.
            #[allow(clippy::needless_range_loop)]
            for i in child..adj[vi].len() {
                let w = adj[vi][i] as usize;
                if index[w] == UNVISITED {
                    frames.push((v, i + 1));
                    frames.push((w as u32, 0));
                    descended = true;
                    break;
                } else if on_stack[w] {
                    low[vi] = low[vi].min(index[w]);
                }
            }
            if descended {
                continue;
            }

            if low[vi] == index[vi] {
                while let Some(w) = stack.pop() {
                    on_stack[w as usize] = false;
                    comp[w as usize] = next_comp;
                    if w == v {
                        break;
                    }
                }
                next_comp += 1;
            }

            // Carry this node's low-link back to the frame that descended into
            // it, which is now on top of the stack.
            if let Some(&(parent, _)) = frames.last() {
                low[parent as usize] = low[parent as usize].min(low[vi]);
            }
        }
    }

    comp
}

/// Decides whether a `%PROVIDES%` entry satisfies a dependency's constraint.
///
/// **The constraint cannot be discarded for provides.** Spec §5.2 says to strip
/// version constraints because an already-installed set is necessarily
/// consistent — true for dependencies on real package names, and false for
/// sonames, where the version *is* the identity:
///
/// ```text
/// libxml2         provides  libxml2.so=16-64
/// libxml2-legacy  provides  libxml2.so=2-64      different library entirely
/// glew            provides  libGLEW.so=2.3-64
/// lib32-glew      provides  libGLEW.so=2.3-32    the -32/-64 is the ELF class
/// ```
///
/// Ignoring it fuses the 32- and 64-bit worlds and makes every legacy
/// compatibility package look permanently required.
///
/// Only exact-versus-exact is compared. A range constraint (`java-runtime>=11`)
/// would need full `vercmp` semantics; those are rare, always on true virtual
/// names, and getting them wrong by keeping an edge is the conservative
/// direction, so they resolve permissively.
fn provide_satisfies(dep_constraint: Option<&str>, provide_constraint: Option<&str>) -> bool {
    match (dep_constraint, provide_constraint) {
        (None, _) => true,
        (Some(d), Some(p)) if d.starts_with('=') && p.starts_with('=') => d == p,
        _ => true,
    }
}

/// Resolves a dependency to every installed package that satisfies it.
///
/// Both a real package of that name *and* anything providing the name count.
/// `ca-certificates-utils` provides `ca-certificates` while a package of that
/// name also exists; treating the real package as the sole satisfier drops the
/// reverse edges from everything that depends on it, and it then looks
/// removable when it is not.
fn resolve(db: &LocalDb, providers: &HashMap<String, Vec<PkgIdx>>, dep: &Dep) -> Vec<PkgIdx> {
    let mut out = Vec::new();

    // A dependency on an installed package name is satisfied by definition:
    // pacman would not have allowed the install otherwise.
    if let Ok(i) = db.packages.binary_search_by(|p| p.name.as_str().cmp(&dep.name)) {
        out.push(i as u32);
    }

    for &cand in providers.get(dep.name.as_str()).into_iter().flatten() {
        if out.contains(&cand) {
            continue;
        }
        let matches = db.packages[cand as usize]
            .provides
            .iter()
            .filter(|p| p.name == dep.name)
            .any(|p| provide_satisfies(dep.constraint.as_deref(), p.constraint.as_deref()));
        if matches {
            out.push(cand);
        }
    }

    out
}

impl Graph {
    pub fn build(db: Arc<LocalDb>) -> Self {
        let n = db.packages.len();

        // A package provides its own name, plus everything in %PROVIDES%.
        // Without the provides map, dependencies like `sh`, `java-runtime` or
        // `libGL.so=1-64` resolve to nothing and the graph quietly develops
        // holes that corrupt orphan detection (spec §13.1).
        let mut providers: HashMap<String, Vec<PkgIdx>> = HashMap::with_capacity(n * 2);
        for (i, p) in db.packages.iter().enumerate() {
            providers.entry(p.name.clone()).or_default().push(i as u32);
            for prov in &p.provides {
                providers.entry(prov.name.clone()).or_default().push(i as u32);
            }
        }

        let mut g = Graph {
            db: Arc::clone(&db),
            providers,
            depends_on: vec![Vec::new(); n],
            required_by: vec![Vec::new(); n],
            optional_for: vec![Vec::new(); n],
            unresolved: Vec::new(),
            scc: Vec::new(),
            scc_members: Vec::new(),
        };

        for (i, p) in db.packages.iter().enumerate() {
            let i = i as u32;
            for d in &p.depends {
                let hits = resolve(&db, &g.providers, d);
                if hits.is_empty() {
                    g.unresolved.push((i, d.to_string()));
                }
                for h in hits {
                    if h != i && !g.depends_on[i as usize].contains(&h) {
                        g.depends_on[i as usize].push(h);
                        g.required_by[h as usize].push(i);
                    }
                }
            }

            for od in &p.optdepends {
                for h in resolve(&db, &g.providers, &od.dep) {
                    if h != i && !g.optional_for[h as usize].contains(&i) {
                        g.optional_for[h as usize].push(i);
                    }
                }
            }
        }

        g.scc = compute_sccs(n, &g.depends_on);
        let comp_count = g.scc.iter().copied().max().map_or(0, |m| m as usize + 1);
        g.scc_members = vec![Vec::new(); comp_count];
        for (i, &c) in g.scc.iter().enumerate() {
            g.scc_members[c as usize].push(i as u32);
        }

        g
    }

    /// Every package in the same dependency cycle as `i`, including itself.
    /// A single-element result means `i` is in no cycle.
    pub fn cycle_group(&self, i: PkgIdx) -> &[PkgIdx] {
        &self.scc_members[self.scc[i as usize] as usize]
    }

    pub fn len(&self) -> usize {
        self.db.packages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.db.packages.is_empty()
    }

    pub fn index_of(&self, name: &str) -> Option<PkgIdx> {
        self.db
            .packages
            .binary_search_by(|p| p.name.as_str().cmp(name))
            .ok()
            .map(|i| i as u32)
    }

    pub fn name(&self, i: PkgIdx) -> &str {
        &self.db.packages[i as usize].name
    }

    pub fn depends_on(&self, i: PkgIdx) -> &[PkgIdx] {
        &self.depends_on[i as usize]
    }

    pub fn required_by(&self, i: PkgIdx) -> &[PkgIdx] {
        &self.required_by[i as usize]
    }

    /// Installed packages that list `i` as an *optional* dependency.
    pub fn optional_for(&self, i: PkgIdx) -> &[PkgIdx] {
        &self.optional_for[i as usize]
    }


    /// The full transitive closure of what `roots` depend on.
    pub fn closure(&self, roots: impl IntoIterator<Item = PkgIdx>) -> HashSet<PkgIdx> {
        let mut seen = HashSet::new();
        let mut queue: VecDeque<PkgIdx> = roots.into_iter().collect();
        for r in &queue {
            seen.insert(*r);
        }
        while let Some(cur) = queue.pop_front() {
            for &d in self.depends_on(cur) {
                if seen.insert(d) {
                    queue.push_back(d);
                }
            }
        }
        seen
    }

    fn explicit_roots(&self) -> Vec<PkgIdx> {
        self.db
            .packages
            .iter()
            .enumerate()
            .filter(|(_, p)| p.reason == Reason::Explicit)
            .map(|(i, _)| i as u32)
            .collect()
    }

    /// Packages installed as dependencies that nothing reachable still needs.
    ///
    /// Computed as **reachability from the explicit-install roots**, not as a
    /// reference count. This matters: `pacman -Qdt` counts references, so a
    /// cycle of mutually-depending orphans keeps its own refcounts alive and
    /// stays invisible to it forever (spec §5.2). A reachability pass finds
    /// them, because nothing outside the cycle points into it.
    pub fn orphans(&self, mode: OrphanMode) -> Vec<PkgIdx> {
        let reachable = self.closure(self.explicit_roots());

        let mut out: Vec<PkgIdx> = (0..self.len() as u32)
            .filter(|i| !reachable.contains(i))
            .collect();

        if mode == OrphanMode::Conservative {
            // Mirror `pacman -Qdt`: a package that some *installed* package
            // lists as an optional dependency is not reported, because it may
            // be there deliberately to enable a feature.
            out.retain(|&i| self.optional_for(i).is_empty());
        }
        out
    }

    /// Orphans that pacman's refcounting cannot see, because something still
    /// requires them — but that something is itself an orphan.
    ///
    /// Two shapes end up here, and the UI must not confuse them:
    ///
    /// - **Transitively stranded** — an orphan root with a whole tree beneath
    ///   it. `npm` on the dev machine is required by nothing, yet keeps
    ///   `nodejs`, `node-gyp`, `semver`, `ada` and `simdjson` alive with
    ///   non-zero refcounts, so `pacman -Qdtt` never reports them.
    /// - **Cycle-trapped** — packages requiring each other, which refcounting
    ///   can never free. See [`Graph::cycle_trapped_orphans`].
    ///
    /// Both are almost always genuine garbage, but they are presented
    /// separately and cautiously because pacman's own tooling will never have
    /// flagged them (spec §5.2).
    pub fn hidden_orphans(&self, mode: OrphanMode) -> Vec<PkgIdx> {
        self.orphans(mode)
            .into_iter()
            .filter(|&i| !self.required_by(i).is_empty())
            .collect()
    }

    /// Orphans that are genuinely part of a dependency cycle: each one can
    /// reach itself by following `depends_on` edges.
    ///
    /// Distinguished from merely transitively-stranded orphans because a cycle
    /// is the case refcounting can *never* resolve, no matter what else is
    /// removed first. Orphan counts are tiny, so the direct
    /// can-it-reach-itself test is used in preference to a full SCC pass.
    pub fn cycle_trapped_orphans(&self) -> Vec<PkgIdx> {
        let orphans: HashSet<PkgIdx> = self.orphans(OrphanMode::Aggressive).into_iter().collect();
        orphans
            .iter()
            .copied()
            .filter(|&start| {
                let mut seen = HashSet::new();
                let mut queue: VecDeque<PkgIdx> = self.depends_on(start).to_vec().into();
                while let Some(cur) = queue.pop_front() {
                    if cur == start {
                        return true;
                    }
                    if !orphans.contains(&cur) || !seen.insert(cur) {
                        continue;
                    }
                    queue.extend(self.depends_on(cur).iter().copied());
                }
                false
            })
            .collect()
    }

    /// Simulates removing `targets`, the way `pacman -Rs` would.
    ///
    /// Returns what else would go, what would block the removal, and which
    /// installed packages would silently lose an optional dependency.
    pub fn plan_removal(&self, targets: &[PkgIdx]) -> RemovalPlan {
        // This mirrors alpm's `_alpm_recursedeps` rather than using our own
        // reachability model, because its job is to *predict pacman*, not to
        // analyse the system. The two genuinely differ: pacman reasons by
        // reference count, so a package kept alive only by an orphan is not
        // itself considered an orphan — but removing that orphan does take it.
        //
        // A package joins the removal when all three hold:
        //   1. something already being removed depends on it,
        //   2. it was installed as a dependency (explicit installs are user
        //      choices and are never cascaded), and
        //   3. every package that requires it is also being removed.
        //
        // Condition 1 is what leaves pre-existing orphans alone: nothing in the
        // removal set points at them, so they never become candidates.
        // Iterating to a fixpoint is what lets deep chains unravel one layer at
        // a time.
        let mut going: HashSet<PkgIdx> = targets.iter().copied().collect();
        loop {
            // Removing one package can unblock another that was still required
            // a moment ago, so the whole candidate set is re-examined until a
            // pass adds nothing. Passes are bounded by the depth of the
            // dependency chain, which is small.
            let candidates: Vec<PkgIdx> = going
                .iter()
                .flat_map(|&g| self.depends_on(g).iter().copied())
                .collect();

            let mut added = false;
            for cand in candidates {
                if going.contains(&cand) {
                    continue;
                }

                // Cycle members can only leave together: inside a cycle each
                // package is required by another, so no member ever satisfies
                // the test alone and a package-at-a-time rule deadlocks. The
                // component is judged as a whole against the world outside it.
                let group = self.cycle_group(cand);

                let all_are_dependencies = group
                    .iter()
                    .all(|&m| self.db.packages[m as usize].reason == Reason::Dependency);
                if !all_are_dependencies {
                    continue;
                }

                let group_set: HashSet<PkgIdx> = group.iter().copied().collect();
                let free = group.iter().all(|&m| {
                    self.required_by(m)
                        .iter()
                        .all(|r| group_set.contains(r) || going.contains(r))
                });

                if free {
                    going.extend(group.iter().copied());
                    added = true;
                }
            }
            if !added {
                break;
            }
        }

        let target_set: HashSet<PkgIdx> = targets.iter().copied().collect();
        let cascade: Vec<PkgIdx> = going
            .iter()
            .copied()
            .filter(|i| !target_set.contains(i))
            .collect();

        // Blockers: anything staying that requires something going. pacman
        // refuses such a removal outright rather than breaking the dependency.
        let mut blockers = Vec::new();
        for &t in &going {
            for &r in self.required_by(t) {
                if !going.contains(&r) {
                    blockers.push((r, t));
                }
            }
        }
        blockers.sort_unstable();
        blockers.dedup();

        // Optional-dependency casualties: nothing breaks, features just stop
        // working, with no error anywhere. Precisely why this is surfaced.
        let mut optdep_losses = Vec::new();
        for &t in &going {
            for &r in self.optional_for(t) {
                if !going.contains(&r) {
                    optdep_losses.push((r, t));
                }
            }
        }
        optdep_losses.sort_unstable();
        optdep_losses.dedup();

        let freed_bytes = going
            .iter()
            .map(|&i| self.db.packages[i as usize].size_bytes())
            .sum();

        let mut target = targets.to_vec();
        target.sort_unstable();
        let mut cascade = cascade;
        cascade.sort_unstable();

        RemovalPlan {
            target,
            cascade,
            blockers,
            optdep_losses,
            freed_bytes,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrphanMode {
    /// Mirrors `pacman -Qdt`: excludes packages that are an optional dependency
    /// of something installed. The safer default.
    Conservative,
    /// Mirrors `pacman -Qdtt`: includes them.
    Aggressive,
}

#[derive(Debug, Clone, Default)]
pub struct RemovalPlan {
    /// What the user asked to remove.
    pub target: Vec<PkgIdx>,
    /// Dependencies that become orphaned as a result, which `-Rs` also takes.
    pub cascade: Vec<PkgIdx>,
    /// (dependent, dependency) pairs where something staying needs something
    /// going. Non-empty means pacman would refuse the operation.
    pub blockers: Vec<(PkgIdx, PkgIdx)>,
    /// (package, lost optdep) pairs — silent feature loss, not an error.
    pub optdep_losses: Vec<(PkgIdx, PkgIdx)>,
    pub freed_bytes: u64,
}

impl RemovalPlan {
    /// Everything that would actually leave the system.
    pub fn all_removed(&self) -> Vec<PkgIdx> {
        let mut v = self.target.clone();
        v.extend_from_slice(&self.cascade);
        v.sort_unstable();
        v.dedup();
        v
    }

    pub fn is_blocked(&self) -> bool {
        !self.blockers.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::local::parse_desc;

    /// Builds a `LocalDb` from terse `(name, reason, depends, provides,
    /// optdepends)` tuples so graph behaviour can be asserted without fixtures.
    fn db_of(specs: &[(&str, Reason, &[&str], &[&str], &[&str])]) -> LocalDb {
        let mut packages: Vec<_> = specs
            .iter()
            .map(|(name, reason, deps, provides, optdeps)| {
                let mut text = format!("%NAME%\n{name}\n\n%VERSION%\n1-1\n\n%SIZE%\n1000\n");
                if *reason == Reason::Dependency {
                    text.push_str("\n%REASON%\n1\n");
                }
                for (key, vals) in [
                    ("DEPENDS", deps),
                    ("PROVIDES", provides),
                    ("OPTDEPENDS", optdeps),
                ] {
                    if !vals.is_empty() {
                        text.push_str(&format!("\n%{key}%\n{}\n", vals.join("\n")));
                    }
                }
                parse_desc(&text, name).unwrap()
            })
            .collect();
        packages.sort_by(|a, b| a.name.cmp(&b.name));
        LocalDb {
            packages,
            errors: Vec::new(),
            root: Default::default(),
        }
    }

    fn names(g: &Graph, idx: &[PkgIdx]) -> Vec<String> {
        let mut v: Vec<String> = idx.iter().map(|&i| g.name(i).to_string()).collect();
        v.sort();
        v
    }

    #[test]
    fn resolves_virtual_and_soname_provides() {
        // `app` needs `sh` and `libGL.so`, neither of which is a package name.
        let db = db_of(&[
            ("app", Reason::Explicit, &["sh", "libGL.so=1-64"], &[], &[]),
            ("bash", Reason::Dependency, &[], &["sh"], &[]),
            ("mesa", Reason::Dependency, &[], &["libGL.so=1-64"], &[]),
        ]);
        let g = Graph::build(std::sync::Arc::new(db));
        let app = g.index_of("app").unwrap();

        assert_eq!(names(&g, g.depends_on(app)), ["bash", "mesa"]);
        assert!(g.unresolved.is_empty(), "{:?}", g.unresolved);
        // Nothing is orphaned: both are reachable from the explicit root.
        assert!(g.orphans(OrphanMode::Conservative).is_empty());
    }

    #[test]
    fn version_constraints_do_not_break_edges() {
        let db = db_of(&[
            ("app", Reason::Explicit, &["curl>=7.20.0"], &[], &[]),
            ("curl", Reason::Dependency, &[], &[], &[]),
        ]);
        let g = Graph::build(std::sync::Arc::new(db));
        assert_eq!(names(&g, g.depends_on(g.index_of("app").unwrap())), ["curl"]);
    }

    #[test]
    fn unreachable_dependency_is_an_orphan() {
        let db = db_of(&[
            ("app", Reason::Explicit, &[], &[], &[]),
            ("leftover", Reason::Dependency, &[], &[], &[]),
        ]);
        let g = Graph::build(std::sync::Arc::new(db));
        assert_eq!(
            names(&g, &g.orphans(OrphanMode::Conservative)),
            ["leftover"]
        );
    }

    #[test]
    fn orphan_cycles_are_found_where_refcounting_fails() {
        // `a` and `b` require each other and nothing else requires either.
        // pacman -Qdt can never report these; a reachability pass must.
        let db = db_of(&[
            ("app", Reason::Explicit, &[], &[], &[]),
            ("a", Reason::Dependency, &["b"], &[], &[]),
            ("b", Reason::Dependency, &["a"], &[], &[]),
        ]);
        let g = Graph::build(std::sync::Arc::new(db));
        assert_eq!(names(&g, &g.orphans(OrphanMode::Aggressive)), ["a", "b"]);
        assert_eq!(names(&g, &g.cycle_trapped_orphans()), ["a", "b"]);
    }

    #[test]
    fn conservative_mode_spares_optional_dependencies() {
        let db = db_of(&[
            ("app", Reason::Explicit, &[], &[], &["extra: nice to have"]),
            ("extra", Reason::Dependency, &[], &[], &[]),
        ]);
        let g = Graph::build(std::sync::Arc::new(db));
        // -Qdt style: not reported, something optionally wants it.
        assert!(g.orphans(OrphanMode::Conservative).is_empty());
        // -Qdtt style: reported.
        assert_eq!(names(&g, &g.orphans(OrphanMode::Aggressive)), ["extra"]);
    }

    #[test]
    fn removal_cascades_to_newly_orphaned_dependencies_only() {
        let db = db_of(&[
            ("gimp", Reason::Explicit, &["gegl", "glib"], &[], &[]),
            ("firefox", Reason::Explicit, &["glib"], &[], &[]),
            ("gegl", Reason::Dependency, &[], &[], &[]),
            ("glib", Reason::Dependency, &[], &[], &[]),
        ]);
        let g = Graph::build(std::sync::Arc::new(db));
        let plan = g.plan_removal(&[g.index_of("gimp").unwrap()]);

        // gegl goes with it; glib stays because firefox still needs it.
        assert_eq!(names(&g, &plan.cascade), ["gegl"]);
        assert_eq!(names(&g, &plan.all_removed()), ["gegl", "gimp"]);
        assert!(!plan.is_blocked());
        assert_eq!(plan.freed_bytes, 2000);
    }

    #[test]
    fn explicit_packages_are_never_taken_by_the_cascade() {
        // `tool` is unused but the user chose it; -Rs leaves it alone.
        let db = db_of(&[
            ("app", Reason::Explicit, &["lib"], &[], &[]),
            ("tool", Reason::Explicit, &[], &[], &[]),
            ("lib", Reason::Dependency, &[], &[], &[]),
        ]);
        let g = Graph::build(std::sync::Arc::new(db));
        let plan = g.plan_removal(&[g.index_of("app").unwrap()]);
        assert_eq!(names(&g, &plan.cascade), ["lib"]);
        assert!(!plan.all_removed().contains(&g.index_of("tool").unwrap()));
    }

    #[test]
    fn something_still_needed_blocks_removal() {
        let db = db_of(&[
            ("app", Reason::Explicit, &["lib"], &[], &[]),
            ("lib", Reason::Dependency, &[], &[], &[]),
        ]);
        let g = Graph::build(std::sync::Arc::new(db));
        let plan = g.plan_removal(&[g.index_of("lib").unwrap()]);
        assert!(plan.is_blocked());
        let (dependent, dep) = plan.blockers[0];
        assert_eq!(g.name(dependent), "app");
        assert_eq!(g.name(dep), "lib");
    }

    #[test]
    fn optional_dependency_loss_is_reported_but_does_not_block() {
        let db = db_of(&[
            ("player", Reason::Explicit, &[], &[], &["codec: MP3 support"]),
            ("codec", Reason::Explicit, &[], &[], &[]),
        ]);
        let g = Graph::build(std::sync::Arc::new(db));
        let plan = g.plan_removal(&[g.index_of("codec").unwrap()]);

        assert!(!plan.is_blocked(), "optdeps must never block a removal");
        assert_eq!(plan.optdep_losses.len(), 1);
        assert_eq!(g.name(plan.optdep_losses[0].0), "player");
    }

    #[test]
    fn pre_existing_orphans_are_not_swept_up_by_an_unrelated_removal() {
        // Regression guard, found by comparing against `pacman -Rs --print`:
        // `junk` was already orphaned before this operation, so -Rs leaves it.
        // Including it made removing one program claim to remove fourteen
        // unrelated packages.
        let db = db_of(&[
            ("app", Reason::Explicit, &["lib"], &[], &[]),
            ("lib", Reason::Dependency, &[], &[], &[]),
            ("junk", Reason::Dependency, &[], &[], &[]),
        ]);
        let g = Graph::build(std::sync::Arc::new(db));
        let plan = g.plan_removal(&[g.index_of("app").unwrap()]);

        assert_eq!(names(&g, &plan.all_removed()), ["app", "lib"]);
        assert!(!plan.all_removed().contains(&g.index_of("junk").unwrap()));
        // It is still an orphan; that is the orphan view's job, not removal's.
        assert!(g
            .orphans(OrphanMode::Conservative)
            .contains(&g.index_of("junk").unwrap()));
    }

    #[test]
    fn soname_versions_distinguish_providers() {
        // Found by comparing against pacman: `libxml2` provides
        // `libxml2.so=16-64` and `libxml2-legacy` provides `libxml2.so=2-64`.
        // Stripping the constraint links both, so the legacy package looks
        // permanently required and never becomes removable.
        let db = db_of(&[
            ("app", Reason::Explicit, &["libxml2.so=16-64"], &[], &[]),
            ("legacy-user", Reason::Explicit, &["libxml2.so=2-64"], &[], &[]),
            ("libxml2", Reason::Dependency, &[], &["libxml2.so=16-64"], &[]),
            ("libxml2-legacy", Reason::Dependency, &[], &["libxml2.so=2-64"], &[]),
        ]);
        let g = Graph::build(std::sync::Arc::new(db));

        assert_eq!(names(&g, g.depends_on(g.index_of("app").unwrap())), ["libxml2"]);
        assert_eq!(
            names(&g, g.depends_on(g.index_of("legacy-user").unwrap())),
            ["libxml2-legacy"]
        );
    }

    #[test]
    fn elf_class_suffix_keeps_32_and_64_bit_apart() {
        // `glew` provides libGLEW.so=2.3-64, `lib32-glew` provides =2.3-32.
        // The -32/-64 suffix is the only thing distinguishing them.
        let db = db_of(&[
            ("game", Reason::Explicit, &["libGLEW.so=2.3-32"], &[], &[]),
            ("glew", Reason::Dependency, &[], &["libGLEW.so=2.3-64"], &[]),
            ("lib32-glew", Reason::Dependency, &[], &["libGLEW.so=2.3-32"], &[]),
        ]);
        let g = Graph::build(std::sync::Arc::new(db));
        assert_eq!(
            names(&g, g.depends_on(g.index_of("game").unwrap())),
            ["lib32-glew"]
        );
    }

    #[test]
    fn a_real_package_and_its_providers_both_satisfy_a_dependency() {
        // `ca-certificates-utils` provides `ca-certificates`, which is also a
        // real package. Treating only the real package as the satisfier drops
        // the reverse edges and makes the provider look removable.
        let db = db_of(&[
            ("curl", Reason::Explicit, &["ca-certificates"], &[], &[]),
            ("ca-certificates", Reason::Dependency, &[], &[], &[]),
            (
                "ca-certificates-utils",
                Reason::Dependency,
                &[],
                &["ca-certificates"],
                &[],
            ),
        ]);
        let g = Graph::build(std::sync::Arc::new(db));
        assert_eq!(
            names(&g, g.depends_on(g.index_of("curl").unwrap())),
            ["ca-certificates", "ca-certificates-utils"]
        );
    }

    #[test]
    fn mutually_dependent_packages_are_removed_as_a_group() {
        // Regression guard for a deadlock: `tesseract` needs `tessdata` (which
        // `tesseract-data-eng` provides) and that needs `tesseract` back.
        // Judging one package at a time, neither is ever free, so pacman took
        // four packages we claimed would stay.
        let db = db_of(&[
            ("spectacle", Reason::Explicit, &["tesseract"], &[], &[]),
            (
                "tesseract",
                Reason::Dependency,
                &["tessdata", "leptonica"],
                &[],
                &[],
            ),
            (
                "tesseract-data-eng",
                Reason::Dependency,
                &["tesseract"],
                &["tessdata"],
                &[],
            ),
            ("leptonica", Reason::Dependency, &[], &[], &[]),
        ]);
        let g = Graph::build(std::sync::Arc::new(db));

        let cycle = names(&g, g.cycle_group(g.index_of("tesseract").unwrap()));
        assert_eq!(cycle, ["tesseract", "tesseract-data-eng"]);

        let plan = g.plan_removal(&[g.index_of("spectacle").unwrap()]);
        assert_eq!(
            names(&g, &plan.all_removed()),
            ["leptonica", "spectacle", "tesseract", "tesseract-data-eng"]
        );
    }

    #[test]
    fn a_cycle_held_by_an_outsider_is_not_removed() {
        // Same shape, but something staying still needs the cycle.
        let db = db_of(&[
            ("spectacle", Reason::Explicit, &["tesseract"], &[], &[]),
            ("other", Reason::Explicit, &["tesseract"], &[], &[]),
            ("tesseract", Reason::Dependency, &["tessdata"], &[], &[]),
            (
                "tesseract-data-eng",
                Reason::Dependency,
                &["tesseract"],
                &["tessdata"],
                &[],
            ),
        ]);
        let g = Graph::build(std::sync::Arc::new(db));
        let plan = g.plan_removal(&[g.index_of("spectacle").unwrap()]);
        assert_eq!(names(&g, &plan.all_removed()), ["spectacle"]);
    }

    #[test]
    fn an_explicit_member_protects_the_whole_cycle() {
        let db = db_of(&[
            ("app", Reason::Explicit, &["a"], &[], &[]),
            ("a", Reason::Dependency, &["b"], &[], &[]),
            ("b", Reason::Explicit, &["a"], &[], &[]),
        ]);
        let g = Graph::build(std::sync::Arc::new(db));
        let plan = g.plan_removal(&[g.index_of("app").unwrap()]);
        assert_eq!(names(&g, &plan.all_removed()), ["app"]);
    }

    #[test]
    fn removal_does_not_traverse_through_a_target() {
        // b is reachable only via a. Removing a must take b and then c.
        let db = db_of(&[
            ("a", Reason::Explicit, &["b"], &[], &[]),
            ("b", Reason::Dependency, &["c"], &[], &[]),
            ("c", Reason::Dependency, &[], &[], &[]),
        ]);
        let g = Graph::build(std::sync::Arc::new(db));
        let plan = g.plan_removal(&[g.index_of("a").unwrap()]);
        assert_eq!(names(&g, &plan.all_removed()), ["a", "b", "c"]);
    }
}
