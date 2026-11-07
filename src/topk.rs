//! A bounded heap that keeps the best `k` groups seen so far.
//!
//! "Best" means larger aggregate first, then smaller key, which makes the
//! answer of a query unique even when values tie. The heap's root is the
//! current k-th best entry; its value is the pruning threshold.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub key: u64,
    pub val: i64,
}

impl Entry {
    /// True if `self` ranks strictly ahead of `other`.
    #[inline(always)]
    pub fn beats(&self, other: &Entry) -> bool {
        self.val > other.val || (self.val == other.val && self.key < other.key)
    }
}

/// Heap order: the *worst* entry is the greatest, so it sits at the root.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Worst(Entry);

impl Ord for Worst {
    fn cmp(&self, other: &Self) -> Ordering {
        other.0.val.cmp(&self.0.val).then(self.0.key.cmp(&other.0.key))
    }
}
impl PartialOrd for Worst {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

pub struct TopK {
    k: usize,
    heap: BinaryHeap<Worst>,
}

impl TopK {
    pub fn new(k: usize) -> Self {
        TopK { k, heap: BinaryHeap::with_capacity(k + 1) }
    }

    #[inline]
    pub fn push(&mut self, e: Entry) {
        if self.k == 0 {
            return;
        }
        if self.heap.len() < self.k {
            self.heap.push(Worst(e));
        } else if e.beats(&self.heap.peek().unwrap().0) {
            self.heap.pop();
            self.heap.push(Worst(e));
        }
    }

    /// The k-th best value once `k` entries are held. Any group whose final
    /// value is strictly below it can never enter the answer.
    #[inline]
    pub fn threshold(&self) -> Option<i64> {
        if self.k > 0 && self.heap.len() == self.k {
            Some(self.heap.peek().unwrap().0.val)
        } else {
            None
        }
    }

    pub fn len(&self) -> usize {
        self.heap.len()
    }

    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    pub fn drain(&mut self) -> impl Iterator<Item = Entry> + '_ {
        self.heap.drain().map(|w| w.0)
    }

    /// Best first.
    pub fn into_sorted(self) -> Vec<Entry> {
        // into_sorted_vec is ascending by Ord, i.e. best (least worst) first.
        self.heap.into_sorted_vec().into_iter().map(|w| w.0).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gen::Rng;

    #[test]
    fn keeps_the_k_best_with_key_tiebreak() {
        let mut rng = Rng::new(3);
        for _ in 0..200 {
            let n = rng.below(300) as usize;
            let k = rng.below(20) as usize;
            let mut all: Vec<Entry> =
                (0..n).map(|i| Entry { key: i as u64, val: rng.below(10) as i64 - 5 }).collect();
            // Shuffle so insertion order is not key order.
            for i in (1..all.len()).rev() {
                all.swap(i, rng.below(i as u64 + 1) as usize);
            }
            let mut t = TopK::new(k);
            for &e in &all {
                t.push(e);
            }
            all.sort_by(|a, b| b.val.cmp(&a.val).then(a.key.cmp(&b.key)));
            all.truncate(k);
            let thr = t.threshold();
            assert_eq!(thr, if all.len() == k && k > 0 { Some(all[k - 1].val) } else { None });
            assert_eq!(t.into_sorted(), all);
        }
    }
}
