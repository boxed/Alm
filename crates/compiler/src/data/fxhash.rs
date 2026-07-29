//! A fast, non-cryptographic hasher for the compiler's internal maps.
//!
//! `std`'s default is SipHash-1-3, chosen to make hash-flooding attacks on
//! untrusted keys impractical. Nothing here hashes untrusted keys — the keys
//! are union-find variable ids and source regions, all integers — and
//! profiling a real build put SipHash at a noticeable share of type checking.
//! This is rustc's own `FxHash`: one multiply and one rotate per word.
//!
//! **Only for maps whose iteration order cannot reach the output.** Changing a
//! hasher reshuffles iteration, and this compiler has been bitten by that
//! before — a set of type variables iterated in hash order once made
//! compilation nondeterministic. Use it for membership tests, memo tables and
//! lookups; anything iterated must either be sorted first or keep a
//! deterministic map type.

use std::hash::{BuildHasherDefault, Hasher};

pub type FxHashMap<K, V> = std::collections::HashMap<K, V, BuildHasherDefault<FxHasher>>;
pub type FxHashSet<T> = std::collections::HashSet<T, BuildHasherDefault<FxHasher>>;

/// Chosen so that multiplying spreads entropy across the whole word; the
/// constant is the fractional part of the golden ratio scaled to 64 bits.
const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

#[derive(Default, Clone)]
pub struct FxHasher {
    hash: u64,
}

impl FxHasher {
    #[inline]
    fn add(&mut self, word: u64) {
        self.hash = (self.hash.rotate_left(5) ^ word).wrapping_mul(SEED);
    }
}

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(8) {
            let mut word = [0u8; 8];
            word[..chunk.len()].copy_from_slice(chunk);
            self.add(u64::from_le_bytes(word));
        }
    }

    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.add(i as u64);
    }

    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.add(i as u64);
    }

    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.add(i);
    }

    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.add(i as u64);
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.hash
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_behaves_like_a_hash_map() {
        let mut map: FxHashMap<usize, &str> = FxHashMap::default();
        for i in 0..1000 {
            map.insert(i, "x");
        }
        assert_eq!(map.len(), 1000);
        assert_eq!(map.get(&999), Some(&"x"));
        assert_eq!(map.get(&1000), None);
        map.insert(500, "y");
        assert_eq!(map.len(), 1000, "an existing key must replace, not duplicate");
        assert_eq!(map.get(&500), Some(&"y"));
    }

    /// Distinct small integers — the keys this is actually used with — must
    /// not pile into one bucket, or lookups degrade to a linear scan.
    #[test]
    fn consecutive_integers_spread_out() {
        use std::hash::{BuildHasher, BuildHasherDefault};
        let build: BuildHasherDefault<FxHasher> = BuildHasherDefault::default();
        let buckets = 64;
        let mut hits = vec![0usize; buckets];
        for i in 0..4096usize {
            hits[(build.hash_one(i) % buckets as u64) as usize] += 1;
        }
        let expected = 4096 / buckets;
        assert!(
            hits.iter().all(|&n| n > expected / 4 && n < expected * 4),
            "uneven spread: {hits:?}"
        );
    }
}
