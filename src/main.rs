//! `apo` — application-centric package explorer for Arch Linux.
//!
//! M1: read-only explorer. **This build performs no write operations of any
//! kind** — it never invokes pacman for anything but read-only queries, and it
//! never writes to the pacman database (spec §12/M1).
//!
//! Running with no arguments starts the TUI. The subcommands are development
//! tools that predate it and remain useful for scripted checks.

mod apps;
mod data;
mod ops;
mod state;
mod ui;
mod verify;

use data::fileindex::FileIndex;
use data::graph::Graph;
use data::local::{LocalDb, Reason};
use data::sync::SyncDb;

fn main() -> anyhow::Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("verify") => run_verify(),
        Some("apps") => run_apps(),
        Some("stats") => stats(),
        Some("denylist") => denylist_audit(),
        Some("icon") => icon_probe(),
        Some("--help" | "-h") => {
            println!(
                "apo — application-centric package explorer\n\n\
                 apo              start the explorer\n\
                 apo stats        parse statistics\n\
                 apo apps         application catalog\n\
                 apo apps tools   tools list\n\
                 apo verify [n|names…]   check the model against pacman\n\n\
                 This build is read-only and removes nothing."
            );
            Ok(())
        }
        _ => run_tui(),
    }
}

/// Resolves and decodes one icon, reporting where it stopped.
///
/// Icon failures are silent by design in the UI — decoration must never break
/// the view — which makes them undiagnosable without a way to ask directly.
fn icon_probe() -> anyhow::Result<()> {
    let Some(name) = std::env::args().nth(2) else {
        println!("usage: apo icon <icon-name>");
        return Ok(());
    };
    match apps::icon::find(&name) {
        None => println!("{name}: no file found"),
        Some(path) => {
            println!("{name}: found {}", path.display());
            match apps::icon::load(&path) {
                Some(icon) => println!(
                    "  decoded {}x{}",
                    icon.rgba.width(),
                    icon.rgba.height()
                ),
                None => println!("  DECODE FAILED"),
            }
        }
    }
    Ok(())
}

/// Prints what the denylist protects and why.
///
/// The denylist makes removals impossible, so it has to be auditable: an
/// over-broad rule silently turns "cannot remove this" into "cannot remove
/// anything", which is a failure the user would experience as the tool being
/// broken rather than careful.
fn denylist_audit() -> anyhow::Result<()> {
    let db = std::sync::Arc::new(LocalDb::load(LocalDb::DEFAULT_ROOT)?);
    let g = Graph::build(db);
    let d = ops::safety::Denylist::build(&g);

    let total = g.len();
    println!(
        "{} of {total} packages protected ({:.0}%)",
        d.len(),
        d.len() as f64 / total as f64 * 100.0
    );

    let mut direct: Vec<&str> = Vec::new();
    for i in 0..total as u32 {
        if matches!(
            d.protection(i),
            Some(ops::safety::Protection::Direct(_))
        ) {
            direct.push(g.name(i));
        }
    }
    println!("\n{} protected directly:", direct.len());
    for n in &direct {
        println!("  {n}");
    }

    if let Some(name) = std::env::args().nth(2) {
        match g.index_of(&name) {
            Some(i) => match d.protection(i) {
                Some(p) => println!("\n{name}: {}", p.explain()),
                None => println!("\n{name}: removable"),
            },
            None => println!("\n{name}: not installed"),
        }
    }
    Ok(())
}

/// Refuses to start as root.
///
/// Read-only inspection needs no privileges whatsoever, and a root TUI is one
/// stray keystroke away from being the most dangerous program on the system
/// (spec §7.4, §16).
fn run_tui() -> anyhow::Result<()> {
    // SAFETY: getuid is always safe; it reads a process attribute.
    if unsafe { libc_getuid() } == 0 {
        anyhow::bail!(
            "refusing to run as root — this tool only reads, and needs no privileges"
        );
    }

    let state = state::SystemState::load()?;
    ui::run(state)
}

/// `getuid(2)` without pulling in the `libc` crate for one call.
unsafe fn libc_getuid() -> u32 {
    unsafe extern "C" {
        fn getuid() -> u32;
    }
    unsafe { getuid() }
}

/// Lists the synthesised application catalog. Compare this against the desktop
/// launcher: anything the launcher shows that is missing here is a filter bug.
fn run_apps() -> anyhow::Result<()> {
    let started = std::time::Instant::now();
    let db = LocalDb::load(LocalDb::DEFAULT_ROOT)?;
    let (index, _) = FileIndex::load_or_build(&db);

    let suffixes: Vec<String> = apps::DEFAULT_MERGE_SUFFIXES
        .iter()
        .map(|s| s.to_string())
        .collect();
    let noise: Vec<String> = apps::DEFAULT_NOISE.iter().map(|s| s.to_string()).collect();
    let catalog = apps::resolve(&db, &index, &suffixes, &noise);
    let elapsed = started.elapsed();

    println!(
        "{} apps, {} tools, {} desktop entries filtered out, in {:.1?}",
        catalog.apps.len(),
        catalog.tools.len(),
        catalog.filtered.len(),
        elapsed
    );

    let mut by_source: std::collections::BTreeMap<String, usize> = Default::default();
    for a in &catalog.apps {
        *by_source.entry(format!("{:?}", a.source)).or_default() += 1;
    }
    let sources: Vec<String> = by_source.iter().map(|(s, n)| format!("{n} {s}")).collect();
    println!("by source: {}\n", sources.join(", "));

    if std::env::args().nth(2).as_deref() == Some("tools") {
        for tool in &catalog.tools {
            println!(
                "  {:<30} {}",
                truncate(&tool.name, 29),
                truncate(tool.summary.as_deref().unwrap_or(""), 70)
            );
        }
        return Ok(());
    }

    for app in &catalog.apps {
        let pkgs = if app.packages.is_empty() {
            "-".to_string()
        } else {
            app.packages.join("+")
        };
        let tag = match app.source {
            apps::Source::Pacman => "pkg",
            apps::Source::Flatpak => "flat",
            apps::Source::AppImage => "img",
            apps::Source::Steam => "steam",
            apps::Source::Unowned => "?",
        };
        println!(
            "{:<6} {:<32} {:<28} {}",
            tag,
            truncate(&app.name, 31),
            truncate(&pkgs, 27),
            truncate(app.summary.as_deref().unwrap_or(""), 56)
        );
    }

    // Why entries were dropped, so filters can be sanity-checked in bulk.
    let mut reasons: std::collections::BTreeMap<String, usize> = Default::default();
    for (_, why) in &catalog.filtered {
        let key = why.split('(').next().unwrap_or(why).to_string();
        *reasons.entry(key).or_default() += 1;
    }
    println!("\nfiltered by reason:");
    for (why, n) in &reasons {
        println!("  {why:<28} {n}");
    }

    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max.saturating_sub(1)).collect::<String>() + "…"
}

/// Checks the dependency model against pacman itself. See `verify`.
fn run_verify() -> anyhow::Result<()> {
    let db = LocalDb::load(LocalDb::DEFAULT_ROOT)?;
    let g = Graph::build(std::sync::Arc::new(db));

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
