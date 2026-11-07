//! A linear-probing hash table from `u64` keys to `i64` aggregate states.
//!
//! The caller supplies the hash (computed once per row and shared with the
//! partition/slot layout), and the table probes with its low bits. Keys are
//! stored inline next to their values, `0` marks an empty bucket, and the one
//! real key equal to the marker lives in a side field. A zero marker makes a
//! fresh table a zeroed allocation, which the OS serves as untouched zero
//! pages instead of a pass that writes every bucket.

use crate::hash::mix;

const EMPTY: u64 = 0;

pub struct AggTable {
    /// `(key, state)` interleaved, so a probe touches one cache line.
    buckets: Vec<(u64, i64)>,
    len: usize,
    mask: usize,
    /// State for the key `0`, which cannot be stored in `buckets`.
    zero_key: Option<i64>,
    /// Growable tables double at 5/8 load; fixed ones report `is_full`
    /// instead and leave flushing to the caller.
    growable: bool,
}

impl AggTable {
    pub fn with_capacity(groups: usize) -> Self {
        // Keep the load factor at or below 1/2 after `groups` inserts.
        let cap = (groups.max(4) * 2).next_power_of_two();
        AggTable {
            buckets: vec![(EMPTY, 0); cap],
            len: 0,
            mask: cap - 1,
            zero_key: None,
            growable: true,
        }
    }

    /// A table with exactly `buckets` buckets (a power of two) that never
    /// grows. Check `is_full` after each insert.
    pub fn fixed(buckets: usize) -> Self {
        assert!(buckets.is_power_of_two() && buckets >= 8);
        AggTable {
            buckets: vec![(EMPTY, 0); buckets],
            len: 0,
            mask: buckets - 1,
            zero_key: None,
            growable: false,
        }
    }

    /// For fixed tables: at 5/8 load, time to flush.
    #[inline(always)]
    pub fn is_full(&self) -> bool {
        self.len * 8 >= self.buckets.len() * 5
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len + self.zero_key.is_some() as usize
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn capacity(&self) -> usize {
        self.buckets.len()
    }

    /// Insert `key` with `new` if absent, otherwise apply `f` to its state.
    #[inline(always)]
    pub fn upsert(&mut self, key: u64, h: u64, new: i64, f: impl FnOnce(&mut i64)) {
        if key == EMPTY {
            match &mut self.zero_key {
                Some(v) => f(v),
                None => self.zero_key = Some(new),
            }
            return;
        }
        let mut i = h as usize & self.mask;
        loop {
            let b = unsafe { self.buckets.get_unchecked_mut(i) };
            if b.0 == key {
                f(&mut b.1);
                return;
            }
            if b.0 == EMPTY {
                *b = (key, new);
                self.len += 1;
                // Grow at 5/8 load.
                if self.growable && self.len * 8 > self.buckets.len() * 5 {
                    self.grow();
                }
                return;
            }
            i = (i + 1) & self.mask;
        }
    }

    pub fn get(&self, key: u64) -> Option<i64> {
        if key == EMPTY {
            return self.zero_key;
        }
        let mut i = mix(key) as usize & self.mask;
        loop {
            let (k, v) = self.buckets[i];
            if k == key {
                return Some(v);
            }
            if k == EMPTY {
                return None;
            }
            i = (i + 1) & self.mask;
        }
    }

    fn grow(&mut self) {
        let cap = self.buckets.len() * 2;
        let old = std::mem::replace(&mut self.buckets, vec![(EMPTY, 0); cap]);
        self.mask = cap - 1;
        for (k, v) in old {
            if k == EMPTY {
                continue;
            }
            let mut i = mix(k) as usize & self.mask;
            while self.buckets[i].0 != EMPTY {
                i = (i + 1) & self.mask;
            }
            self.buckets[i] = (k, v);
        }
    }

    /// Remove every entry but keep the allocation.
    pub fn clear(&mut self) {
        if self.len > 0 {
            self.buckets.fill((EMPTY, 0));
        }
        self.len = 0;
        self.zero_key = None;
    }

    /// Visit every `(key, state)` pair.
    #[inline]
    pub fn for_each(&self, mut f: impl FnMut(u64, i64)) {
        for &(k, v) in &self.buckets {
            if k != EMPTY {
                f(k, v);
            }
        }
        if let Some(v) = self.zero_key {
            f(EMPTY, v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gen::Rng;
    use std::collections::HashMap;

    fn add(t: &mut AggTable, k: u64, v: i64) {
        t.upsert(k, mix(k), v, |a| *a += v);
    }

    #[test]
    fn matches_std_hashmap_under_random_load() {
        let mut rng = Rng::new(7);
        for round in 0..20 {
            let mut t = AggTable::with_capacity(1);
            let mut m: HashMap<u64, i64> = HashMap::new();
            let domain = 1 + rng.below(5000);
            for _ in 0..rng.below(20_000) {
                // Mix in the sentinel key and its neighbours on purpose.
                let k = match rng.below(50) {
                    0 => u64::MAX,
                    1 => u64::MAX - 1,
                    2 => 0,
                    _ => rng.below(domain),
                };
                let v = rng.below(100) as i64 - 50;
                add(&mut t, k, v);
                *m.entry(k).or_insert(0) += v;
            }
            assert_eq!(t.len(), m.len(), "round {round}");
            for (&k, &v) in &m {
                assert_eq!(t.get(k), Some(v));
            }
            let mut seen = 0;
            t.for_each(|k, v| {
                assert_eq!(m[&k], v);
                seen += 1;
            });
            assert_eq!(seen, m.len());
            assert_eq!(t.get(u64::MAX - 2), m.get(&(u64::MAX - 2)).copied());
        }
    }

    #[test]
    fn clear_keeps_capacity_and_forgets_everything() {
        let mut t = AggTable::with_capacity(4);
        for k in 0..1000 {
            add(&mut t, k, 1);
        }
        add(&mut t, u64::MAX, 1);
        let cap = t.capacity();
        t.clear();
        assert_eq!(t.len(), 0);
        assert_eq!(t.capacity(), cap);
        assert_eq!(t.get(5), None);
        assert_eq!(t.get(u64::MAX), None);
        add(&mut t, 5, 3);
        assert_eq!(t.get(5), Some(3));
    }
}
