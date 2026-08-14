//! `apo` — application-centric package explorer for Arch Linux.
//!
//! M1 scaffolding: no TUI yet. This entry point exists so the data layer can be
//! exercised against the real system while it is being built (spec §16: build
//! `data/` and `apps/` first, do not start with the UI).

mod data;

use data::local::{LocalDb, Reason};
use data::sync::SyncDb;

fn main() -> anyhow::Result<()> {
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
