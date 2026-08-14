//! Correctness harness: check our in-process model against pacman itself.
//!
//! The whole product rests on one claim — *"if you delete this, here is exactly
//! what goes with it"* — and the only way to know that claim holds is to ask
//! pacman the same question and compare. Both oracles used here are read-only
//! and need no privileges:
//!
//! ```text
//! pacman -Qdtq            orphans, conservative
//! pacman -Qdttq           orphans, aggressive
//! pacman -Rs --print P    exactly what removing P would take
//! ```
//!
//! Per spec §15.6 the agent never executes a real removal; `--print` is how the
//! removal path gets exercised. A divergence here is a graph bug, and a graph
//! bug means we would tell the user something untrue about deleting their
//! system.

use std::collections::BTreeSet;
use std::process::Command;

use crate::data::graph::{Graph, OrphanMode};

pub struct Report {
    pub checks: usize,
    pub failures: Vec<String>,
}

impl Report {
    fn pass(&mut self) {
        self.checks += 1;
    }

    fn fail(&mut self, msg: String) {
        self.checks += 1;
        self.failures.push(msg);
    }

    pub fn ok(&self) -> bool {
        self.failures.is_empty()
    }
}

fn pacman(args: &[&str]) -> anyhow::Result<Vec<String>> {
    let out = Command::new("pacman").args(args).output()?;
    // `-Qdtq` exits 1 with no output when there are no orphans, which is not an
    // error condition.
    if !out.status.success() && !out.stdout.is_empty() {
        anyhow::bail!(
            "pacman {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

/// Strips the `-<version>` suffix `-Rs --print` appends to each package name.
///
/// The output is `name-ver-rel`, and both names and versions may contain
/// hyphens, so splitting is ambiguous in general. Anchoring on the known set of
/// installed package names removes the ambiguity entirely.
fn strip_version<'a>(line: &'a str, known: &BTreeSet<&str>) -> &'a str {
    let mut cut = line.len();
    while let Some(i) = line[..cut].rfind('-') {
        if known.contains(&line[..i]) {
            return &line[..i];
        }
        cut = i;
    }
    line
}

/// Compares removal plans for a named set of packages.
///
/// Used to re-check specific divergences without re-running the full sweep,
/// which spawns one `pacman` per package and is heavy enough to be felt on a
/// loaded desktop.
pub fn run_named(g: &Graph, names: &[String]) -> anyhow::Result<Report> {
    let mut r = Report {
        checks: 0,
        failures: Vec::new(),
    };
    let known: BTreeSet<&str> = g.db.packages.iter().map(|p| p.name.as_str()).collect();
    for name in names {
        let Some(idx) = g.index_of(name) else {
            r.fail(format!("{name}: not installed"));
            continue;
        };
        check_removal(g, idx, &known, &mut r);
    }
    Ok(r)
}

/// Compares one package's removal plan against `pacman -Rs --print`.
fn check_removal(g: &Graph, idx: u32, known: &BTreeSet<&str>, r: &mut Report) {
    let name = g.name(idx).to_string();
    let plan = g.plan_removal(&[idx]);

    let theirs_raw = match pacman(&["-Rs", "--print", &name]) {
        Ok(v) => v,
        // pacman refuses when something still requires the target. Our plan
        // must agree that it is blocked.
        Err(_) => {
            if plan.is_blocked() {
                r.pass();
            } else {
                r.fail(format!(
                    "{name}: pacman refuses removal but we report it unblocked"
                ));
            }
            return;
        }
    };

    let theirs: BTreeSet<String> = theirs_raw
        .iter()
        .map(|l| strip_version(l, known).to_string())
        .collect();
    let ours: BTreeSet<String> = plan
        .all_removed()
        .into_iter()
        .map(|i| g.name(i).to_string())
        .collect();

    if ours == theirs {
        r.pass();
    } else {
        let missing: Vec<_> = theirs.difference(&ours).cloned().collect();
        let extra: Vec<_> = ours.difference(&theirs).cloned().collect();
        r.fail(format!(
            "{name}: plan differs from pacman -Rs --print\n      \
             we miss {} {missing:?}\n      we add  {} {extra:?}",
            missing.len(),
            extra.len()
        ));
    }
}

pub fn run(g: &Graph, removal_samples: usize) -> anyhow::Result<Report> {
    let mut r = Report {
        checks: 0,
        failures: Vec::new(),
    };
    let known: BTreeSet<&str> = g.db.packages.iter().map(|p| p.name.as_str()).collect();

    // 1. Orphans, both safety levels.
    for (mode, args, label) in [
        (OrphanMode::Conservative, &["-Qdtq"][..], "-Qdt"),
        (OrphanMode::Aggressive, &["-Qdttq"][..], "-Qdtt"),
    ] {
        let theirs: BTreeSet<String> = pacman(args)?.into_iter().collect();
        let ours: BTreeSet<String> = g
            .orphans(mode)
            .into_iter()
            .map(|i| g.name(i).to_string())
            .collect();

        // We may legitimately find *more* than pacman does: a cycle of orphans
        // is invisible to refcounting but not to reachability. Finding fewer is
        // always a bug.
        let missing: Vec<_> = theirs.difference(&ours).cloned().collect();
        let extra: Vec<_> = ours.difference(&theirs).cloned().collect();

        if missing.is_empty() {
            r.pass();
        } else {
            r.fail(format!(
                "orphans({label}): pacman reports {missing:?} which we miss"
            ));
        }

        if !extra.is_empty() {
            // Extras are legitimate only when refcounting structurally cannot
            // see them: something still requires them, but that something is
            // itself an orphan.
            let hidden: BTreeSet<String> = g
                .hidden_orphans(mode)
                .into_iter()
                .map(|i| g.name(i).to_string())
                .collect();
            let unexplained: Vec<_> = extra.iter().filter(|e| !hidden.contains(*e)).collect();
            if unexplained.is_empty() {
                r.pass();
                let cyc: BTreeSet<String> = g
                    .cycle_trapped_orphans()
                    .into_iter()
                    .map(|i| g.name(i).to_string())
                    .collect();
                println!(
                    "  note: {label} — {} orphans hidden from refcounting {extra:?} \
                     (of which cycle-trapped: {cyc:?})",
                    extra.len()
                );
            } else {
                r.fail(format!(
                    "orphans({label}): we report {unexplained:?} which pacman does not, \
                     and nothing requires them, so refcounting should have found them"
                ));
            }
        } else {
            r.pass();
        }
    }

    // 2. Removal plans. Sampled across the whole package list rather than taken
    // from the front, so the comparison covers leaves, libraries and metapackages
    // rather than just whatever sorts first alphabetically.
    let n = g.len();
    let step = (n / removal_samples.max(1)).max(1);
    let mut compared = 0;

    for i in (0..n).step_by(step) {
        check_removal(g, i as u32, &known, &mut r);
        compared += 1;
    }

    println!("  compared {compared} removal plans against pacman -Rs --print");
    Ok(r)
}
