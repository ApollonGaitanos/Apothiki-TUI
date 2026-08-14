//! `apo` — application-centric package explorer for Arch Linux.
//!
//! M1 scaffolding: no TUI yet. This entry point exists so the data layer can be
//! exercised against the real system while it is being built (spec §16: build
//! `data/` and `apps/` first, do not start with the UI).

mod data;
mod verify;

use data::fileindex::FileIndex;
use data::graph::{Graph, OrphanMode};
use data::local::{LocalDb, Reason};
use data::sync::SyncDb;

fn main() -> anyhow::Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("verify") => return run_verify(),
        _ => {}
    }
    stats()
}

/// Checks the dependency model against pacman itself. See `verify`.
fn run_verify() -> anyhow::Result<()> {
    let db = LocalDb::load(LocalDb::DEFAULT_ROOT)?;
    let g = Graph::build(&db);

    // A bare number samples that many packages; explicit names re-check known
    // divergences cheaply. The full sweep spawns one pacman per package and is
    // heavy enough to be felt on a loaded desktop, so it is never the default.
    let args: Vec<String> = std::env::args().skip(2).collect();
    let named: Vec<String> = args
        .iter()
        .filter(|a| a.parse::<usize>().is_err())
        .cloned()
        .collect();
    let samples: usize = args
        .iter()
        .find_map(|a| a.parse().ok())
        .unwrap_or(60);

    println!("verifying {} packages against pacman", g.len());
    if !g.unresolved.is_empty() {
        println!(
            "  {} unresolved dependency strings (first 10): {:?}",
            g.unresolved.len(),
            g.unresolved.iter().take(10).collect::<Vec<_>>()
        );
    }

    let report = if named.is_empty() {
        verify::run(&g, samples)?
    } else {
        verify::run_named(&g, &named)?
    };
    println!("\n{} checks, {} failures", report.checks, report.failures.len());
    for f in &report.failures {
        println!("  FAIL {f}");
    }
    if !report.ok() {
        std::process::exit(1);
    }
    println!("all checks passed");
    Ok(())
}

fn stats() -> anyhow::Result<()> {
    let started = std::time::Instant::now();
    let db = LocalDb::load(LocalDb::DEFAULT_ROOT)?;
    let local_done = started.elapsed();
    let sync = SyncDb::load(SyncDb::DEFAULT_ROOT)?;
    let elapsed = started.elapsed();

    let explicit = db.explicit_count();
    // Foreign = listed by no repository. Not "%INSTALLED_DB% absent".
    let foreign: Vec<_> = db
        .packages
        .iter()
        .filter(|p| sync.is_foreign(&p.name))
        .collect();
    let total_size: u64 = db.packages.iter().map(|p| p.size_bytes()).sum();

    println!(
        "local db {:.1?} + {} sync repos ({} entries) {:.1?} = {:.1?} total",
        local_done,
        sync.repos.len(),
        sync.packages.len(),
        elapsed - local_done,
        elapsed
    );
    println!("parsed {} packages", db.packages.len());
    println!("  explicit:   {explicit}");
    println!("  dependency: {}", db.packages.len() - explicit);
    println!("  foreign (in no repo, i.e. AUR/local): {}", foreign.len());
    println!("  installed size: {:.2} GiB", total_size as f64 / (1 << 30) as f64);

    let t = std::time::Instant::now();
    let (index, outcome) = FileIndex::load_or_build(&db);
    println!(
        "\nfile index: {} files in {:.1?} ({outcome:?})",
        index.len(),
        t.elapsed()
    );
    // The index exists to answer exactly this question, ~350 times, instantly.
    for probe in [
        "/usr/share/applications/org.kde.filelight.desktop",
        "/usr/share/metainfo/org.kde.dolphin.appdata.xml",
        "/usr/bin/pacman",
    ] {
        println!("  {probe} -> {:?}", index.owner(probe));
    }

    // A repo that fails to load makes its packages look foreign, so this must
    // never be silent.
    if !sync.errors.is_empty() {
        println!("\n{} sync repos failed to load:", sync.errors.len());
        for (repo, err) in &sync.errors {
            println!("  {repo}: {err}");
        }
    }

    if !db.errors.is_empty() {
        println!("\n{} entries failed to parse:", db.errors.len());
        for (dir, err) in db.errors.iter().take(10) {
            println!("  {dir}: {err}");
        }
    }

    // Repo breakdown — on CachyOS the same package name exists in both the Arch
    // and CachyOS repos, so origin is load-bearing information (spec §11).
    // %INSTALLED_DB% wins when present; otherwise fall back to sync lookup.
    let mut by_repo: std::collections::BTreeMap<&str, usize> = Default::default();
    let mut shadowed = 0usize;
    for p in &db.packages {
        let repo = p
            .repo
            .as_deref()
            .or_else(|| sync.get(&p.name).map(|s| s.repo.as_str()))
            .unwrap_or("<foreign>");
        *by_repo.entry(repo).or_default() += 1;
        if sync.all_providers_of(&p.name).len() > 1 {
            shadowed += 1;
        }
    }
    println!("\nby repo (%INSTALLED_DB%, else sync lookup):");
    for (repo, n) in &by_repo {
        println!("  {repo:<24} {n}");
    }
    println!("  {shadowed} installed packages exist in more than one repo");

    if !foreign.is_empty() {
        println!("\nforeign packages ({}):", foreign.len());
        for p in foreign.iter().take(20) {
            let reason = match p.reason {
                Reason::Explicit => "explicit",
                Reason::Dependency => "dependency",
            };
            println!("  {:<32} {:<20} {reason}", p.name, p.version);
        }
    }

    Ok(())
}
