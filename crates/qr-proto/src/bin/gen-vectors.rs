//! Writes `conformance/vectors.json`: one encoded sample of every message.

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn main() {
    let mut out = String::from("[\n");
    let all = qr_proto::samples::all();
    for (i, s) in all.iter().enumerate() {
        let comma = if i + 1 < all.len() { "," } else { "" };
        out += &format!(
            "  {{\"name\": \"{}\", \"kind\": \"{}\", \"hex\": \"{}\"}}{comma}\n",
            s.name,
            s.kind,
            hex(&s.bytes)
        );
    }
    out += "]\n";
    let path = std::env::args().nth(1).unwrap_or_else(|| "conformance/vectors.json".into());
    std::fs::write(&path, out).expect("write vectors");
    eprintln!("wrote {} vectors to {path}", all.len());
}
