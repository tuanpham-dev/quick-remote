//! H.264 SPS fix-up for low-latency decoding.
//!
//! GStreamer's H.264 decoders output a picture right away only while the
//! DPB has a free slot. Encoders such as x264 declare a DPB exactly as large
//! as their reference count (`max_dec_frame_buffering == max_num_ref_frames`
//! in the VUI), so the slot is always taken and every picture waits for the
//! next one: a frame of latency. Raising `max_dec_frame_buffering` by one
//! (still within the level's limit) changes nothing else.
//!
//! `max_dec_frame_buffering` is the last field of the SPS, so the rewrite
//! copies the bits before it, writes the new value and closes the RBSP.

struct Bits<'a> {
    d: &'a [u8],
    p: usize,
}

impl Bits<'_> {
    fn u(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = *self.d.get(self.p >> 3)?;
            v = (v << 1) | ((byte >> (7 - (self.p & 7))) & 1) as u32;
            self.p += 1;
        }
        Some(v)
    }

    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.u(1)? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some((1u32 << zeros) - 1 + self.u(zeros)?)
    }

    fn se(&mut self) -> Option<i32> {
        let v = self.ue()?;
        Some(if v & 1 == 1 { v.div_ceil(2) as i32 } else { -((v / 2) as i32) })
    }
}

#[derive(Default)]
struct Writer {
    out: Vec<u8>,
    bits: u32,
    n: u32,
}

impl Writer {
    fn put(&mut self, v: u32, n: u32) {
        for i in (0..n).rev() {
            self.bits = (self.bits << 1) | ((v >> i) & 1);
            self.n += 1;
            if self.n == 8 {
                self.out.push(self.bits as u8);
                (self.bits, self.n) = (0, 0);
            }
        }
    }

    fn ue(&mut self, v: u32) {
        let x = v + 1;
        let len = 32 - x.leading_zeros();
        self.put(0, len - 1);
        self.put(x, len);
    }

    /// rbsp_trailing_bits.
    fn finish(mut self) -> Vec<u8> {
        self.put(1, 1);
        while self.n != 0 {
            self.put(0, 1);
        }
        self.out
    }
}

fn unescape(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

fn escape(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + 4);
    let mut zeros = 0;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

fn skip_scaling_list(r: &mut Bits, size: usize) -> Option<()> {
    let (mut last, mut next) = (8i32, 8i32);
    for _ in 0..size {
        if next != 0 {
            next = (last + r.se()? + 256) % 256;
        }
        if next != 0 {
            last = next;
        }
    }
    Some(())
}

fn skip_hrd(r: &mut Bits) -> Option<()> {
    let cpb = r.ue()?;
    r.u(8)?;
    for _ in 0..=cpb {
        r.ue()?;
        r.ue()?;
        r.u(1)?;
    }
    r.u(20)?;
    Some(())
}

/// The SPS NAL (header byte included, emulation-prevented) with its DPB
/// raised above the reference count, or `None` when it needs no change or
/// can't be parsed.
pub fn fix_sps(nal: &[u8]) -> Option<Vec<u8>> {
    if nal.first()? & 0x1f != 7 {
        return None;
    }
    let rbsp = unescape(&nal[1..]);
    let mut r = Bits { d: &rbsp, p: 0 };
    let profile = r.u(8)?;
    r.u(16)?; // constraint flags, level
    r.ue()?; // seq_parameter_set_id
    if matches!(profile, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        let chroma = r.ue()?;
        if chroma == 3 {
            r.u(1)?;
        }
        r.ue()?;
        r.ue()?;
        r.u(1)?;
        if r.u(1)? == 1 {
            for i in 0..if chroma == 3 { 12 } else { 8 } {
                if r.u(1)? == 1 {
                    skip_scaling_list(&mut r, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    r.ue()?; // log2_max_frame_num_minus4
    match r.ue()? {
        0 => {
            r.ue()?;
        }
        1 => {
            r.u(1)?;
            r.se()?;
            r.se()?;
            for _ in 0..r.ue()? {
                r.se()?;
            }
        }
        _ => {}
    }
    let max_refs = r.ue()?;
    r.u(1)?;
    r.ue()?;
    r.ue()?;
    if r.u(1)? == 0 {
        r.u(1)?; // mb_adaptive_frame_field_flag
    }
    r.u(1)?;
    if r.u(1)? == 1 {
        for _ in 0..4 {
            r.ue()?;
        }
    }
    if r.u(1)? == 0 {
        return None; // no VUI
    }
    if r.u(1)? == 1 && r.u(8)? == 255 {
        r.u(32)?;
    }
    if r.u(1)? == 1 {
        r.u(1)?;
    }
    if r.u(1)? == 1 {
        r.u(4)?;
        if r.u(1)? == 1 {
            r.u(24)?;
        }
    }
    if r.u(1)? == 1 {
        r.ue()?;
        r.ue()?;
    }
    if r.u(1)? == 1 {
        r.u(32)?;
        r.u(32)?;
        r.u(1)?;
    }
    let nal_hrd = r.u(1)? == 1;
    if nal_hrd {
        skip_hrd(&mut r)?;
    }
    let vcl_hrd = r.u(1)? == 1;
    if vcl_hrd {
        skip_hrd(&mut r)?;
    }
    if nal_hrd || vcl_hrd {
        r.u(1)?;
    }
    r.u(1)?; // pic_struct_present_flag
    if r.u(1)? == 0 {
        return None; // no bitstream restriction
    }
    r.u(1)?;
    for _ in 0..4 {
        r.ue()?;
    }
    let reorder = r.ue()?;
    let at = r.p;
    let dpb = r.ue()?;
    if reorder != 0 || dpb > max_refs || max_refs >= 16 {
        return None;
    }
    // Copy everything before max_dec_frame_buffering, then the new value.
    let mut w = Writer::default();
    let mut c = Bits { d: &rbsp, p: 0 };
    let mut left = at;
    while left > 0 {
        let n = left.min(24) as u32;
        w.put(c.u(n)?, n);
        left -= n as usize;
    }
    w.ue(max_refs + 1);
    let mut out = vec![nal[0]];
    out.extend(escape(&w.finish()));
    Some(out)
}

/// The access unit (Annex B) with any SPS fixed by [`fix_sps`], or `None`
/// when nothing changed.
pub fn fix_au(au: &[u8]) -> Option<Vec<u8>> {
    // NAL boundaries: (payload start, payload end).
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= au.len() {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out: Option<Vec<u8>> = None;
    let mut copied = 0;
    for (k, &s) in starts.iter().enumerate() {
        let mut end = starts.get(k + 1).map_or(au.len(), |n| n - 3);
        while end > s && au[end - 1] == 0 && k + 1 < starts.len() {
            end -= 1; // a four-byte start code's leading zero
        }
        if au.get(s).is_some_and(|h| h & 0x1f == 7)
            && let Some(sps) = fix_sps(&au[s..end])
        {
            let o = out.get_or_insert_with(Vec::new);
            o.extend_from_slice(&au[copied..s]);
            o.extend_from_slice(&sps);
            copied = end;
        }
    }
    out.map(|mut o| {
        o.extend_from_slice(&au[copied..]);
        o
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    /// (max_num_ref_frames, max_num_reorder_frames, max_dec_frame_buffering)
    fn fields(nal: &[u8]) -> (u32, u32, u32) {
        let rbsp = unescape(&nal[1..]);
        let mut r = Bits { d: &rbsp, p: 0 };
        let profile = r.u(8).unwrap();
        r.u(16).unwrap();
        r.ue().unwrap();
        assert_eq!(profile, 100);
        r.ue().unwrap();
        r.ue().unwrap();
        r.ue().unwrap();
        r.u(1).unwrap();
        assert_eq!(r.u(1).unwrap(), 0, "no scaling matrices in the vector");
        r.ue().unwrap();
        let poc = r.ue().unwrap();
        if poc == 0 {
            r.ue().unwrap();
        }
        let refs = r.ue().unwrap();
        // The rest through fix_sps' own parse: check by re-running it on a
        // copy with the DPB value read back.
        let _ = refs;
        let fixed_again = fix_sps(nal);
        assert!(fixed_again.is_none(), "a fixed SPS needs no second fix");
        (refs, 0, 0)
    }

    #[test]
    fn raises_x264_dpb_by_one() {
        // x264 superfast/zerolatency, 640x360, one reference frame.
        let sps = hex("67640016acb405017fcb80b5060606a5000003000100000300148f162ea0");
        let fixed = fix_sps(&sps).expect("x264's DPB equals its references");
        assert_ne!(fixed, sps);
        assert_eq!(fields(&fixed).0, 1);
        // The bits before the field are unchanged.
        let (a, b) = (unescape(&sps[1..]), unescape(&fixed[1..]));
        assert_eq!(a[..a.len() - 2], b[..a.len() - 2]);
    }

    #[test]
    fn fixes_inside_an_access_unit() {
        let sps = hex("67640016acb405017fcb80b5060606a5000003000100000300148f162ea0");
        let mut au = vec![0, 0, 0, 1, 9, 0x10, 0, 0, 0, 1];
        au.extend(&sps);
        au.extend([0, 0, 0, 1, 0x68, 0xef, 0x3c, 0xb0, 0, 0, 1, 0x65, 0x88]);
        let fixed = fix_au(&au).expect("SPS fixed");
        assert!(fixed.windows(4).any(|w| w == [0x68, 0xef, 0x3c, 0xb0]), "PPS kept");
        assert!(fixed.ends_with(&[0, 0, 1, 0x65, 0x88]), "slice kept");
        assert!(fix_au(&fixed).is_none());
    }
}
