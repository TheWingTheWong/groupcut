//! The obviously-correct answer: aggregate everything, sort, cut.

use crate::agg::AggKind;
use std::collections::HashMap;

pub fn top_k(keys: &[u64], vals: &[i64], agg: AggKind, k: usize) -> Vec<(u64, i64)> {
    let mut m: HashMap<u64, i64> = HashMap::new();
    for (&key, &v) in keys.iter().zip(vals) {
        match m.get_mut(&key) {
            None => {
                m.insert(key, if agg == AggKind::Count { 1 } else { v });
            }
            Some(a) => match agg {
                AggKind::Count => *a += 1,
                AggKind::Sum => *a = crate::agg::sum_add(*a, v),
                AggKind::Max => *a = (*a).max(v),
            },
        }
    }
    let mut all: Vec<(u64, i64)> = m.into_iter().collect();
    all.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    all.truncate(k);
    all
}
