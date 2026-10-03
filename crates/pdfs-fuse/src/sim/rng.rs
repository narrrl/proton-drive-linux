//! A small seeded random number generator, so a failing simulation is a seed
//! that can be replayed. SplitMix64: fast, good enough for choosing operations
//! and faults, and no dependency.

#[derive(Clone, Debug)]
pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub(crate) fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `0..n`; `0` when `n` is `0`.
    pub(crate) fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next_u64() % n }
    }

    /// A value in `lo..=hi`.
    pub(crate) fn between(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.below(hi.saturating_sub(lo) + 1)
    }

    /// True with probability `p`.
    pub(crate) fn chance(&mut self, p: f64) -> bool {
        p > 0.0 && (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64 <= p
    }

    pub(crate) fn pick<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        items.get(self.below(items.len() as u64) as usize)
    }

    /// A child generator, so one consumer's draws do not shift another's.
    pub(crate) fn fork(&mut self) -> Self {
        Self::new(self.next_u64())
    }
}

#[cfg(test)]
mod tests {
    use super::Rng;

    #[test]
    fn a_seed_replays_the_same_sequence() {
        let (mut a, mut b) = (Rng::new(7), Rng::new(7));
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
        assert_ne!(Rng::new(7).next_u64(), Rng::new(8).next_u64());
    }

    #[test]
    fn bounded_draws_stay_in_range() {
        let mut rng = Rng::new(1);
        for _ in 0..1000 {
            assert!(rng.below(5) < 5);
            assert!((3..=9).contains(&rng.between(3, 9)));
        }
        assert_eq!(rng.below(0), 0);
        assert!(!rng.chance(0.0));
        assert!(rng.chance(1.0));
        assert!([1, 2, 3].contains(rng.pick(&[1, 2, 3]).unwrap()));
        assert_eq!(rng.pick::<u8>(&[]), None);
    }

    #[test]
    fn a_fork_draws_apart_from_its_parent() {
        let mut parent = Rng::new(3);
        let mut child = parent.fork();
        assert_ne!(parent.next_u64(), child.next_u64());
    }
}
