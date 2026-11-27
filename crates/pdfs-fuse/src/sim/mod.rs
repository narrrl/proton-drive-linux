//! The simulation tests of `docs/TESTING.md`: a fake Drive with
//! the faults the real one has, and the tools to drive daemons against it.

pub(crate) mod conformance;
pub(crate) mod daemon;
pub(crate) use pdfs_drive::sim::fake_drive;
pub(crate) mod model;
pub(crate) use pdfs_drive::sim::rng;
pub(crate) mod run;
pub(crate) mod watchdog;
