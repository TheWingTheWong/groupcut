//! groupcut: exact `GROUP BY ... ORDER BY agg DESC LIMIT k` that skips the
//! groups which cannot make the cut.
//!
//! See `engine` for the algorithm and `agg` for why the bounds are sound.

pub mod agg;
pub mod engine;
pub mod gen;
pub mod hash;
pub mod reference;
pub mod table;
pub mod topk;
pub mod wiki;

pub use agg::AggKind;
pub use engine::{top_k, Config, PartitionInfo, Stats, Strategy};
