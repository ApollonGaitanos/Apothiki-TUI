//! Data layer: everything that reads the system, with no UI knowledge.
//!
//! Per spec §16 this is built and tested before any UI exists. The seam that
//! keeps it honest is that nothing here knows what an "Application" is — that
//! synthesis happens in `apps/`, on top of the ground truth produced here.

pub mod dep;
pub mod descfmt;
pub mod fileindex;
pub mod graph;
pub mod local;
pub mod sync;
