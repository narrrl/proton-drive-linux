//! What the simulation tests need from Drive: [`fake_drive::FakeDrive`], an
//! in-memory Drive that injects Drive's known faults, and the seeded
//! [`rng::Rng`] that makes a run replayable.

pub mod fake_drive;
pub mod rng;
