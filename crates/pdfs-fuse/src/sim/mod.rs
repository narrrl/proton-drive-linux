//! The simulation tests of `docs/MILESTONE-3.0.0.md` §8: a fake Drive with
//! the faults the real one has, and the tools to drive daemons against it.

pub(crate) mod daemon;
pub(crate) mod fake_drive;
pub(crate) mod model;
pub(crate) mod rng;
pub(crate) mod run;
pub(crate) mod watchdog;
