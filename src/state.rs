//! The immutable system snapshot the UI reads.
//!
//! Spec §10: all filesystem and database work happens on a background thread and
//! produces one immutable `SystemState`; the UI thread only ever reads it. No
//! I/O in the render loop, ever — a frame that touches the disk is a frame that
//! can stall on a spinning disk, an NFS mount, or a pacman transaction.

use std::sync::Arc;

use crate::apps::{self, Catalog};
use crate::data::fileindex::FileIndex;
use crate::data::graph::Graph;
use crate::data::local::LocalDb;
use crate::data::sync::SyncDb;

pub struct SystemState {
    pub db: Arc<LocalDb>,
    pub graph: Graph,
    pub index: FileIndex,
    pub catalog: Catalog,
    /// Repository data. Optional because it is the slowest thing to load
    /// (~260 ms) and nothing in M1's views needs it on the first frame.
    pub sync: Option<SyncDb>,
    /// True while another pacman process holds the database lock. Mutations
    /// must be disabled and a banner shown; M1 has no mutations, but the
    /// detection belongs with the data (spec §5.1).
    pub db_locked: bool,
    pub load_time: std::time::Duration,
}

impl SystemState {
    /// Loads everything M1's views need. Runs off the render loop.
    pub fn load() -> anyhow::Result<Self> {
        let started = std::time::Instant::now();

        let db = Arc::new(LocalDb::load(LocalDb::DEFAULT_ROOT)?);
        let (index, _) = FileIndex::load_or_build(&db);

        let suffixes: Vec<String> = apps::DEFAULT_MERGE_SUFFIXES
            .iter()
            .map(|s| s.to_string())
            .collect();
        let noise: Vec<String> = apps::DEFAULT_NOISE.iter().map(|s| s.to_string()).collect();
        let catalog = apps::resolve(&db, &index, &suffixes, &noise);

        let graph = Graph::build(Arc::clone(&db));

        Ok(SystemState {
            db,
            graph,
            index,
            catalog,
            sync: None,
            db_locked: is_db_locked(),
            load_time: started.elapsed(),
        })
    }
}

/// Detects a concurrent pacman transaction.
///
/// Presence of the lock file is the whole test — pacman creates it for the
/// duration of a transaction and removes it afterwards. Re-checked on a timer by
/// the UI so it recovers on its own when the other process finishes, rather than
/// requiring a restart (spec §5.1).
pub fn is_db_locked() -> bool {
    std::path::Path::new("/var/lib/pacman/db.lck").exists()
}
