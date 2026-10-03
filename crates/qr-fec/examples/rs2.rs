use reed_solomon_erasure::galois_8::ReedSolomon;
use std::time::Instant;
fn main() {
    for (k, m) in [(32usize, 4usize), (32, 16), (8, 1)] {
        let rs = ReedSolomon::new(k, m).unwrap();
        let mut shards: Vec<Vec<u8>> = (0..k + m).map(|i| vec![i as u8; 1386]).collect();
        let n = 2000;
        let t = Instant::now();
        for _ in 0..n {
            rs.encode(&mut shards).unwrap();
        }
        let e = t.elapsed().as_secs_f64() * 1e6 / n as f64;
        let t = Instant::now();
        for _ in 0..n {
            let mut opt: Vec<Option<Vec<u8>>> = shards.iter().cloned().map(Some).collect();
            for i in 0..m.min(4) {
                opt[i * 3] = None;
            }
            rs.reconstruct_data(&mut opt).unwrap();
        }
        let d = t.elapsed().as_secs_f64() * 1e6 / n as f64;
        println!("{k}+{m}: encode {e:.1} µs, decode {} missing {d:.1} µs (incl. clone)", m.min(4));
    }
}
