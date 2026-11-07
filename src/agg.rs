//! Aggregate functions and the upper bounds that make pruning sound.
//!
//! Pruning needs, for every aggregate, a way to turn per-thread partial
//! results into an upper bound on the final value of *any* group that hashes
//! to a given slot. The rule is the same shape for every aggregate: map each
//! partial to a "bound contribution", then combine all contributions in the
//! slot with an operator that can only overshoot.
//!
//! | aggregate | final value of a group      | contribution     | combine |
//! |-----------|-----------------------------|------------------|---------|
//! | COUNT(*)  | sum of partial counts       | partial          | +       |
//! | SUM(v)    | sum of partial sums         | max(partial, 0)  | +       |
//! | MAX(v)    | max of partial maxima       | partial          | max     |
//!
//! For SUM the clamp to zero is what keeps the bound valid with negative
//! values: a group's total is at most the sum of its positive partials, and
//! that is at most the sum of every positive partial in its slot.

/// Which aggregate the query orders by. Results are always ordered by the
/// aggregate descending, ties broken by key ascending.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AggKind {
    Count,
    Sum,
    Max,
}

impl AggKind {
    pub fn name(&self) -> &'static str {
        match self {
            AggKind::Count => "COUNT(*)",
            AggKind::Sum => "SUM(v)",
            AggKind::Max => "MAX(v)",
        }
    }
}

/// Monomorphized aggregate operations on `i64` states.
///
/// SUM panics on `i64` overflow, as PostgreSQL errors on `bigint out of
/// range`. Wrapping would make results wrong in general, and would also make
/// pruning unsound: a wrapped partial no longer bounds anything. (DuckDB
/// widens SUM(BIGINT) to a 128-bit HUGEINT instead; that is the production
/// answer, at twice the state size.) Bounds saturate, so they can only
/// overshoot.
pub trait AggOp: Send + Sync + 'static {
    const KIND: AggKind;
    /// Identity for `bound_combine`.
    const BOUND_ZERO: i64;
    fn init(v: i64) -> i64;
    fn update(acc: &mut i64, v: i64);
    fn merge(acc: &mut i64, partial: i64);
    fn bound_contrib(partial: i64) -> i64;
    fn bound_combine(acc: &mut i64, contrib: i64);
}

/// Checked SUM addition; see `AggOp`.
#[inline(always)]
pub fn sum_add(a: i64, b: i64) -> i64 {
    match a.checked_add(b) {
        Some(s) => s,
        None => overflow(),
    }
}

#[cold]
#[inline(never)]
fn overflow() -> ! {
    panic!("SUM overflowed i64")
}

pub struct Count;
pub struct Sum;
pub struct Max;

impl AggOp for Count {
    const KIND: AggKind = AggKind::Count;
    const BOUND_ZERO: i64 = 0;
    #[inline(always)]
    fn init(_: i64) -> i64 {
        1
    }
    #[inline(always)]
    fn update(acc: &mut i64, _: i64) {
        *acc += 1;
    }
    #[inline(always)]
    fn merge(acc: &mut i64, partial: i64) {
        *acc += partial;
    }
    #[inline(always)]
    fn bound_contrib(partial: i64) -> i64 {
        partial
    }
    #[inline(always)]
    fn bound_combine(acc: &mut i64, c: i64) {
        *acc = acc.saturating_add(c);
    }
}

impl AggOp for Sum {
    const KIND: AggKind = AggKind::Sum;
    const BOUND_ZERO: i64 = 0;
    #[inline(always)]
    fn init(v: i64) -> i64 {
        v
    }
    #[inline(always)]
    fn update(acc: &mut i64, v: i64) {
        *acc = sum_add(*acc, v);
    }
    #[inline(always)]
    fn merge(acc: &mut i64, partial: i64) {
        *acc = sum_add(*acc, partial);
    }
    #[inline(always)]
    fn bound_contrib(partial: i64) -> i64 {
        partial.max(0)
    }
    #[inline(always)]
    fn bound_combine(acc: &mut i64, c: i64) {
        *acc = acc.saturating_add(c);
    }
}

impl AggOp for Max {
    const KIND: AggKind = AggKind::Max;
    const BOUND_ZERO: i64 = i64::MIN;
    #[inline(always)]
    fn init(v: i64) -> i64 {
        v
    }
    #[inline(always)]
    fn update(acc: &mut i64, v: i64) {
        if v > *acc {
            *acc = v;
        }
    }
    #[inline(always)]
    fn merge(acc: &mut i64, partial: i64) {
        Self::update(acc, partial)
    }
    #[inline(always)]
    fn bound_contrib(partial: i64) -> i64 {
        partial
    }
    #[inline(always)]
    fn bound_combine(acc: &mut i64, c: i64) {
        Self::update(acc, c)
    }
}
