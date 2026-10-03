use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use qr_proto::DecodeError;
use qr_proto::datagram::{FrameMeta, VideoShardHeader};

/// A complete frame, in order.
#[derive(Debug, Clone)]
pub struct Frame {
    pub stream: u8,
    pub id: u32,
    pub flags: u8,
    pub meta: FrameMeta,
    pub au: Vec<u8>,
    /// When the last shard needed to complete it arrived.
    pub completed: Instant,
    /// Data shards rebuilt from parity.
    pub repaired: u32,
}

#[derive(Debug, Clone)]
pub enum Event {
    Frame(Frame),
    /// The frame could not be completed in time; the decoder's reference
    /// chain is broken until the next keyframe.
    Lost {
        id: u32,
    },
}

/// Counters since the last [`Reassembler::take_stats`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stats {
    pub bytes_rx: u64,
    pub shards_rx: u32,
    pub shards_lost: u32,
    pub shards_repaired: u32,
    pub frames_rx: u32,
    pub frames_lost: u32,
}

struct Block {
    k: usize,
    m: usize,
    shards: Vec<Option<Vec<u8>>>,
    have: usize,
    /// The block's data shards once all of them are known.
    data: Option<Vec<Vec<u8>>>,
}

struct FrameState {
    flags: u8,
    blocks: Vec<Option<Block>>,
    first_seen: Instant,
    received: u32,
    repaired: u32,
    payload: Option<(Vec<u8>, Instant)>,
    emitted: bool,
}

impl FrameState {
    fn expected_shards(&self) -> u32 {
        let known = self.blocks.iter().flatten().next().map(|b| (b.k, b.m));
        self.blocks
            .iter()
            .map(|b| match b {
                Some(b) => (b.k + b.m) as u32,
                None => known.map(|(k, m)| (k + m) as u32).unwrap_or(1),
            })
            .sum()
    }
}

/// Rebuilds frames of one stream from video datagrams.
pub struct Reassembler {
    stream: u8,
    next: u32,
    frames: BTreeMap<u32, FrameState>,
    /// A missing frame is declared lost once a later frame has been seen
    /// for this long.
    pub deadline: Duration,
    stats: Stats,
    coders: crate::Coders,
}

/// Frames kept ahead of the next expected one; anything further is junk.
const WINDOW: u32 = 512;

impl Reassembler {
    pub fn new(stream: u8, deadline: Duration) -> Self {
        Self { stream, next: 0, frames: BTreeMap::new(), deadline, stats: Stats::default(), coders: Default::default() }
    }

    /// Restarts at frame `next` (a new stream with the same id).
    pub fn reset(&mut self, next: u32) {
        self.next = next;
        self.frames.clear();
    }

    pub fn take_stats(&mut self) -> Stats {
        std::mem::take(&mut self.stats)
    }

    pub fn next_expected(&self) -> u32 {
        self.next
    }

    /// Takes one video datagram. Complete or lost frames come out in order.
    pub fn push(&mut self, datagram: &[u8], now: Instant) -> Result<Vec<Event>, DecodeError> {
        let (h, payload) = VideoShardHeader::parse(datagram)?;
        self.stats.bytes_rx += datagram.len() as u64;
        self.stats.shards_rx += 1;
        if h.stream != self.stream || h.frame >= self.next + WINDOW {
            return Ok(self.drain(now));
        }
        if h.frame < self.next {
            // Late shard of a frame already passed on: it only counts as
            // received, for the loss statistics.
            if let Some(f) = self.frames.get_mut(&h.frame) {
                f.received += 1;
            }
            return Ok(self.drain(now));
        }
        let f = self.frames.entry(h.frame).or_insert_with(|| FrameState {
            flags: h.flags,
            blocks: (0..h.blocks).map(|_| None).collect(),
            first_seen: now,
            received: 0,
            repaired: 0,
            payload: None,
            emitted: false,
        });
        if h.block as usize >= f.blocks.len() {
            return Err(DecodeError::BadTag("video block", h.block));
        }
        f.received += 1;
        if f.payload.is_none() {
            let b = f.blocks[h.block as usize].get_or_insert_with(|| Block {
                k: h.k as usize,
                m: h.m as usize,
                shards: vec![None; h.k as usize + h.m as usize],
                have: 0,
                data: None,
            });
            let i = h.shard as usize;
            if b.data.is_none() && i < b.shards.len() && b.shards[i].is_none() {
                b.shards[i] = Some(payload.to_vec());
                b.have += 1;
                if b.have >= b.k {
                    f.repaired += complete_block(b, &mut self.coders);
                }
            }
            if f.blocks.iter().all(|b| b.as_ref().is_some_and(|b| b.data.is_some())) {
                let mut bytes = Vec::new();
                for b in f.blocks.iter_mut().flatten() {
                    for s in b.data.take().into_iter().flatten() {
                        bytes.extend_from_slice(&s);
                    }
                    b.shards.clear();
                }
                f.payload = Some((bytes, now));
            }
        }
        Ok(self.drain(now))
    }

    /// Declares overdue frames lost and retires old bookkeeping. Call it
    /// periodically (every few ms) even when no datagrams arrive.
    pub fn poll(&mut self, now: Instant) -> Vec<Event> {
        self.drain(now)
    }

    fn drain(&mut self, now: Instant) -> Vec<Event> {
        let mut out = Vec::new();
        loop {
            let ready = matches!(self.frames.get(&self.next), Some(f) if f.payload.is_some());
            if ready {
                let id = self.next;
                let f = self.frames.get_mut(&id).expect("present");
                let (bytes, completed) = f.payload.take().expect("checked");
                f.emitted = true;
                self.next += 1;
                match FrameMeta::parse(&bytes) {
                    Ok((meta, au)) => {
                        self.stats.frames_rx += 1;
                        self.stats.shards_repaired += f.repaired;
                        out.push(Event::Frame(Frame {
                            stream: self.stream,
                            id,
                            flags: f.flags,
                            meta,
                            au: au.to_vec(),
                            completed,
                            repaired: f.repaired,
                        }));
                    }
                    Err(_) => {
                        self.stats.frames_lost += 1;
                        out.push(Event::Lost { id });
                    }
                }
                continue;
            }
            // The next frame is missing or incomplete: give up on it once a
            // later frame has been around for longer than the deadline.
            let overdue = self
                .frames
                .range(self.next + 1..)
                .next()
                .is_some_and(|(_, later)| now.duration_since(later.first_seen) >= self.deadline);
            if overdue {
                if let Some(f) = self.frames.get(&self.next) {
                    let blocks: Vec<String> = f
                        .blocks
                        .iter()
                        .map(|b| b.as_ref().map_or("-".into(), |b| format!("{}/{}+{}", b.have, b.k, b.m)))
                        .collect();
                    tracing::debug!(frame = self.next, ?blocks, "frame lost");
                } else {
                    tracing::debug!(frame = self.next, "frame lost: no shard arrived");
                }
                self.stats.frames_lost += 1;
                out.push(Event::Lost { id: self.next });
                if let Some(f) = self.frames.get_mut(&self.next) {
                    f.emitted = true;
                }
                self.next += 1;
                continue;
            }
            break;
        }
        // Retire frames already passed once late shards stop mattering, and
        // count what never arrived. Frames arrive roughly in id order, so
        // only the front of the map needs looking at.
        let horizon = self.deadline * 2;
        while let Some(entry) = self.frames.first_entry() {
            let f = entry.get();
            if *entry.key() >= self.next || !f.emitted || now.duration_since(f.first_seen) < horizon {
                break;
            }
            let f = entry.remove();
            self.stats.shards_lost += f.expected_shards().saturating_sub(f.received);
        }
        out
    }
}

/// Rebuilds the block's data shards from any `k` shards; returns how many
/// data shards came from parity.
fn complete_block(b: &mut Block, coders: &mut crate::Coders) -> u32 {
    let missing = (0..b.k).filter(|i| b.shards[*i].is_none()).count() as u32;
    if missing > 0 {
        let len = b.shards.iter().flatten().next().map_or(0, Vec::len);
        // Inconsistent shard sizes come only from a broken peer: leave the
        // block incomplete and let the deadline declare the frame lost.
        if b.shards.iter().flatten().any(|s| s.len() != len)
            || coders.get(b.k, b.m).reconstruct_data(&mut b.shards).is_err()
        {
            return 0;
        }
    }
    b.data = Some(b.shards.drain(..b.k).map(|s| s.expect("present or rebuilt")).collect());
    missing
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Packetizer;
    use rand::seq::SliceRandom;
    use rand::{Rng, SeedableRng};

    fn meta() -> FrameMeta {
        FrameMeta { codec: 1, width: 64, height: 64, capture_us: 5, ..Default::default() }
    }

    fn au(id: u32, len: usize) -> Vec<u8> {
        (0..len).map(|i| (i as u32 ^ id.wrapping_mul(2654435761)) as u8).collect()
    }

    #[test]
    fn repairs_up_to_parity_and_loses_beyond() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(7);
        let mut p = Packetizer::new(0, 1200, 0.25);
        let t0 = Instant::now();
        for trial in 0..200u32 {
            let len = rng.gen_range(1..200_000);
            let data = au(trial, len);
            let dgrams = p.frame(0, 0, &meta(), &data);
            // Group by block, drop up to m shards per block (or m+1).
            let too_many = trial % 5 == 0;
            let mut keep = Vec::new();
            let mut by_block: BTreeMap<u8, Vec<Vec<u8>>> = BTreeMap::new();
            for d in dgrams {
                let (h, _) = VideoShardHeader::parse(&d).unwrap();
                by_block.entry(h.block).or_default().push(d);
            }
            let mut lossy_block = false;
            for (_, mut ds) in by_block {
                let (h, _) = VideoShardHeader::parse(&ds[0]).unwrap();
                let drop = if too_many && !lossy_block {
                    lossy_block = true;
                    h.m as usize + 1
                } else {
                    rng.gen_range(0..=h.m as usize)
                };
                ds.shuffle(&mut rng);
                keep.extend(ds.into_iter().skip(drop));
            }
            keep.shuffle(&mut rng);
            let mut r = Reassembler::new(0, Duration::from_millis(20));
            let mut got = None;
            for d in &keep {
                for e in r.push(d, t0).unwrap() {
                    if let Event::Frame(f) = e {
                        got = Some(f);
                    }
                }
            }
            if too_many {
                assert!(got.is_none(), "trial {trial}: should not complete");
            } else {
                let f = got.unwrap_or_else(|| panic!("trial {trial} len {len} incomplete"));
                assert_eq!(f.au, data);
                assert_eq!(f.meta.capture_us, 5);
            }
        }
    }

    #[test]
    fn in_order_with_loss_after_deadline() {
        let mut p = Packetizer::new(0, 1200, 0.1);
        let t0 = Instant::now();
        let mut r = Reassembler::new(0, Duration::from_millis(20));
        let f0 = p.frame(0, 0, &meta(), &au(0, 3000));
        let f1 = p.frame(1, 0, &meta(), &au(1, 3000));
        let f2 = p.frame(2, 0, &meta(), &au(2, 3000));
        // Frame 1 entirely missing; 2 arrives before 0 completes.
        let mut ids = Vec::new();
        for d in f2.iter().chain(f0.iter()) {
            for e in r.push(d, t0).unwrap() {
                ids.push(e);
            }
        }
        assert!(matches!(ids.as_slice(), [Event::Frame(f)] if f.id == 0));
        assert!(r.poll(t0 + Duration::from_millis(10)).is_empty());
        let late = r.poll(t0 + Duration::from_millis(25));
        assert!(matches!(late.as_slice(), [Event::Lost { id: 1 }, Event::Frame(f)] if f.id == 2));
        // A late shard of frame 1 is ignored.
        assert!(r.push(&f1[0], t0 + Duration::from_millis(30)).unwrap().is_empty());
        let s = r.take_stats();
        assert_eq!((s.frames_rx, s.frames_lost), (2, 1));
    }

    #[test]
    fn counts_lost_shards_after_retirement() {
        let mut p = Packetizer::new(0, 1200, 0.5);
        let t0 = Instant::now();
        let mut r = Reassembler::new(0, Duration::from_millis(20));
        let d = p.frame(0, 0, &meta(), &au(0, 10_000));
        let total = d.len() as u32;
        for x in d.iter().skip(2) {
            r.push(x, t0).unwrap();
        }
        r.poll(t0 + Duration::from_millis(50));
        let s = r.take_stats();
        assert_eq!(s.frames_rx, 1);
        assert_eq!(s.shards_lost, 2);
        assert_eq!(s.shards_rx, total - 2);
        assert_eq!(s.shards_repaired, 2);
    }
}
