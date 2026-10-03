use qr_proto::Writer;
use qr_proto::datagram::{FrameMeta, VIDEO_HEADER_LEN, VideoShardHeader};

use crate::MAX_K;

/// How a payload of `len` bytes is cut: `blocks` blocks of `k` data shards
/// (the last block may hold fewer), each shard `shard` bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShardPlan {
    pub shard: usize,
    pub data_shards: usize,
    pub k: usize,
    pub blocks: usize,
}

impl ShardPlan {
    /// Data shards in block `b`.
    pub fn k_of(&self, b: usize) -> usize {
        (self.data_shards - b * self.k).min(self.k)
    }
}

/// Plans the cut of `len` bytes into shards of at most `max_shard` bytes
/// (rounded down to even, which keeps shards 2-byte aligned).
pub fn plan(len: usize, max_shard: usize) -> ShardPlan {
    let max_shard = (max_shard & !1).max(2);
    let n = len.div_ceil(max_shard).max(1);
    // Up to 255 blocks fit the header; beyond MAX_K × 255 shards (≈ 9 MB at
    // a 1200-byte MTU) blocks grow past MAX_K.
    let k_cap = MAX_K.max(n.div_ceil(255));
    let blocks = n.div_ceil(k_cap);
    let k = n.div_ceil(blocks);
    let data_shards = n;
    // Shrink the shard to the smallest even size that still fits, so the
    // padding in the last shard stays small.
    let shard = (len.div_ceil(data_shards) + 1) & !1;
    let shard = shard.clamp(2, max_shard);
    let data_shards = len.div_ceil(shard).max(1);
    let blocks = data_shards.div_ceil(k);
    ShardPlan { shard, data_shards, k, blocks }
}

/// Target probability that a block can't be rebuilt.
pub const BLOCK_FAILURE: f64 = 1e-3;

/// P(more than `m` of `k + m` shards lost) with independent loss `p`.
pub fn block_failure(k: usize, m: usize, p: f64) -> f64 {
    if p <= 0.0 {
        return 0.0;
    }
    let n = k + m;
    let q = 1.0 - p;
    // Sum P(X = i) for i ≤ m, then take the complement.
    let mut term = q.powi(n as i32);
    let mut ok = 0.0;
    for i in 0..=m {
        ok += term;
        term *= (n - i) as f64 / (i + 1) as f64 * p / q;
    }
    (1.0 - ok).max(0.0)
}

/// Builds video datagrams for one stream.
pub struct Packetizer {
    pub stream: u8,
    /// Largest datagram the path carries (QUIC's current max datagram size).
    pub max_datagram: usize,
    /// Minimum parity shards per data shard, e.g. 0.1 for 10%.
    pub parity: f32,
    /// Estimated independent packet loss; parity grows until a block fails
    /// with probability under [`BLOCK_FAILURE`].
    pub loss: f32,
    coders: crate::Coders,
    payload: Vec<u8>,
}

impl Packetizer {
    pub fn new(stream: u8, max_datagram: usize, parity: f32) -> Self {
        Self { stream, max_datagram, parity, loss: 0.0, coders: Default::default(), payload: Vec::new() }
    }

    /// Recovery shards for a block of `k` data shards: at least
    /// `k × parity`, and enough that with loss `loss` the block is
    /// unrecoverable with probability below [`BLOCK_FAILURE`]. A flat ratio
    /// alone fails small blocks: 4 + 1 shards at 5% loss fail 2.3% of the
    /// time.
    pub fn parity_for(&self, k: usize) -> usize {
        if self.parity <= 0.0 && self.loss <= 0.0 {
            return 0;
        }
        let mut m = ((k as f32 * self.parity).ceil() as usize).max(1);
        while m < 255 - k && block_failure(k, m, self.loss as f64) > BLOCK_FAILURE {
            m += 1;
        }
        m.min(255 - k)
    }

    /// Cuts one frame into datagrams, in send order: each block's data
    /// shards, then its parity shards.
    pub fn frame(&mut self, frame_id: u32, flags: u8, meta: &FrameMeta, au: &[u8]) -> Vec<Vec<u8>> {
        let mut payload = Writer { buf: std::mem::take(&mut self.payload) };
        payload.buf.clear();
        let meta = FrameMeta { len: au.len() as u32, ..*meta };
        meta.write(&mut payload);
        payload.raw(au);
        let p = plan(payload.buf.len(), self.max_datagram - VIDEO_HEADER_LEN);
        // Pad to whole shards so every data shard is a plain slice.
        payload.buf.resize(p.data_shards * p.shard, 0);
        let payload = payload.buf;

        let mut out = Vec::with_capacity(p.data_shards + p.blocks * self.parity_for(p.k));
        for b in 0..p.blocks {
            let k = p.k_of(b);
            let m = self.parity_for(k);
            let header = |i: usize| VideoShardHeader {
                stream: self.stream,
                flags,
                frame: frame_id,
                block: b as u8,
                blocks: p.blocks as u8,
                shard: i as u8,
                k: k as u8,
                m: m as u8,
            };
            let shard = |i: usize| &payload[(b * p.k + i) * p.shard..(b * p.k + i + 1) * p.shard];
            for i in 0..k {
                out.push(datagram(header(i), shard(i)));
            }
            if m == 0 {
                continue;
            }
            let rs = self.coders.get(k, m);
            let data: Vec<&[u8]> = (0..k).map(shard).collect();
            let mut parity = vec![vec![0u8; p.shard]; m];
            rs.encode_sep(&data, &mut parity).expect("shard sizes match");
            for (j, r) in parity.iter().enumerate() {
                out.push(datagram(header(k + j), r));
            }
        }
        self.payload = payload;
        out
    }
}

fn datagram(h: VideoShardHeader, shard: &[u8]) -> Vec<u8> {
    let mut w = Writer::with_capacity(VIDEO_HEADER_LEN + shard.len());
    h.write(&mut w);
    w.raw(shard);
    w.buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parity_meets_failure_target() {
        let mut p = Packetizer::new(0, 1200, 0.1);
        assert_eq!(p.parity_for(32), 4);
        p.loss = 0.05;
        let m = p.parity_for(4);
        assert!(block_failure(4, m, 0.05) <= BLOCK_FAILURE && m >= 3, "m={m}");
        let m = p.parity_for(32);
        assert!(block_failure(32, m, 0.05) <= BLOCK_FAILURE);
        assert!(block_failure(32, m - 1, 0.05) > BLOCK_FAILURE);
        assert!((block_failure(1, 0, 0.1) - 0.1).abs() < 1e-12);
    }

    #[test]
    fn plan_fits_mtu_and_blocks() {
        for len in [1, 2, 41, 1187, 1188, 50_000, 300_000, 2_000_000, 12_000_000] {
            let p = plan(len, 1187);
            assert!(p.shard <= 1186 && p.shard % 2 == 0, "{len}: {p:?}");
            assert!(p.data_shards * p.shard >= len);
            assert!(p.blocks <= 255 && p.blocks >= 1);
            assert!(p.k * p.blocks >= p.data_shards);
            assert!(p.k_of(p.blocks - 1) >= 1);
        }
    }
}
