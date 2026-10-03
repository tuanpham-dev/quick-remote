//! Packetize + reassemble throughput: `cargo run --release -p qr-fec --example bench`.

use std::time::{Duration, Instant};

use qr_fec::{Event, Packetizer, Reassembler};
use qr_proto::datagram::FrameMeta;

fn main() {
    for (len, parity) in [(100_000usize, 0.1f32), (500_000, 0.1), (100_000, 0.5)] {
        let au: Vec<u8> = (0..len).map(|i| (i * 31) as u8).collect();
        let mut p = Packetizer::new(0, 1400, parity);
        let mut r = Reassembler::new(0, Duration::from_millis(20));
        let n = 400u32;
        let t = Instant::now();
        let mut ok = 0;
        for id in 0..n {
            let d = p.frame(id, 0, &FrameMeta::default(), &au);
            // Drop the first shard of every block-sized run to force repair.
            for (i, x) in d.iter().enumerate() {
                if i % 40 == 3 {
                    continue;
                }
                for e in r.push(x, Instant::now()).unwrap() {
                    if let Event::Frame(_) = e {
                        ok += 1;
                    }
                }
            }
        }
        let s = t.elapsed().as_secs_f64();
        println!(
            "{len} B frames, parity {parity}: {ok}/{n} frames, {:.2} GB/s, {:.0} µs/frame",
            (len as f64 * n as f64) / s / 1e9,
            s * 1e6 / n as f64
        );
    }
}
