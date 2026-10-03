//! A host advertised on this machine is found by a browser (same process,
//! loopback multicast). The two-namespace version is in tests/e2e (A1).

use std::time::{Duration, Instant};

use qr_net::discovery::{Advertiser, Browser, Discovery};

#[tokio::test]
async fn finds_host_within_3s() {
    let _adv = Advertiser::start("test-host", "qr-test-aaaa-bbbbb", "linux", 9450, 0).unwrap();
    let t0 = Instant::now();
    let (_b, mut rx) = Browser::start().unwrap();
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(3), rx.recv()).await.expect("found within 3 s").unwrap();
        if let Discovery::Up(h) = ev
            && h.host_id == "qr-test-aaaa-bbbbb"
        {
            assert_eq!(h.name, "test-host");
            assert!(h.addrs.iter().all(|a| a.port() == 9450) && !h.addrs.is_empty());
            eprintln!("found after {:?}: {:?}", t0.elapsed(), h.addrs);
            break;
        }
    }
}
