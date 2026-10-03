//! Cuts encoded video frames into FEC-protected datagram shards and puts
//! them back together.
//!
//! A frame is `FrameMeta ‖ access unit`, split into equal-sized data shards
//! (the last one zero-padded). Data shards are grouped into blocks of at most
//! [`MAX_K`]; each block gets `ceil(k × parity)` Reed-Solomon recovery
//! shards, so any `k` of a block's `k + m` shards rebuild it.
//!
//! The code is GF(2^8) Reed-Solomon with the Vandermonde-derived systematic
//! matrix of `reed-solomon-erasure` (the Backblaze construction). It was
//! chosen over the GF(2^16) FFT codec of `reed-solomon-simd`, whose decoder
//! has a fixed cost of about 350 µs per block, against 12 µs here for a
//! 32 + 4 block with four shards missing.

mod packetize;
mod reassemble;

use std::collections::HashMap;
use std::sync::Arc;

use reed_solomon_erasure::galois_8::ReedSolomon;

pub use packetize::{Packetizer, ShardPlan, plan};
pub use reassemble::{Event, Frame, Reassembler, Stats};

/// Most data shards in one FEC block. Small blocks keep decode cheap and
/// spread a burst of loss over separate blocks of a large keyframe.
pub const MAX_K: usize = 32;

/// Coders by `(k, m)`; building one inverts a matrix, so keep them.
#[derive(Default)]
pub(crate) struct Coders(HashMap<(usize, usize), Arc<ReedSolomon>>);

impl Coders {
    pub(crate) fn get(&mut self, k: usize, m: usize) -> Arc<ReedSolomon> {
        self.0
            .entry((k, m))
            .or_insert_with(|| Arc::new(ReedSolomon::new(k, m).expect("1 <= k, 1 <= m, k + m <= 256")))
            .clone()
    }
}
