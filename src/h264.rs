//! Minimal H.264 Annex-B handling: split a byte stream into access units,
//! and convert them to the AVCC form WebCodecs wants.

pub const NAL_SLICE: u8 = 1;
pub const NAL_IDR: u8 = 5;
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;
pub const NAL_AUD: u8 = 9;

pub fn nal_type(nal: &[u8]) -> u8 {
    nal.first().map_or(0, |b| b & 0x1f)
}

/// Find the next `00 00 01` start code at or after `from`.
/// Returns (offset of first byte belonging to the start code incl. a leading zero, offset just after it).
fn find_start_code(buf: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut i = from;
    while i + 3 <= buf.len() {
        if buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1 {
            let start = if i > from && buf[i - 1] == 0 {
                i - 1
            } else {
                i
            };
            return Some((start, i + 3));
        }
        i += 1;
    }
    None
}

#[derive(Debug, Default, Clone)]
pub struct AccessUnit {
    pub nals: Vec<Vec<u8>>,
}

impl AccessUnit {
    pub fn is_key(&self) -> bool {
        self.nals.iter().any(|n| nal_type(n) == NAL_IDR)
    }

    pub fn find(&self, ty: u8) -> Option<&[u8]> {
        self.nals
            .iter()
            .find(|n| nal_type(n) == ty)
            .map(|n| n.as_slice())
    }

    /// 4-byte length-prefixed slice NALs only (SPS/PPS/SEI/AUD are carried out of band).
    pub fn to_avcc(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for n in &self.nals {
            if matches!(nal_type(n), NAL_SLICE | NAL_IDR) {
                out.extend_from_slice(&(n.len() as u32).to_be_bytes());
                out.extend_from_slice(n);
            }
        }
        out
    }
}

/// Splits an Annex-B stream into access units. Requires AUD (type 9) delimiters
/// in the stream (ffmpeg `h264_metadata=aud=insert`). An AU is emitted when the
/// next AUD arrives, so this adds one frame of latency.
#[derive(Default)]
pub struct AuAssembler {
    buf: Vec<u8>,
    cur: AccessUnit,
}

impl AuAssembler {
    pub fn push(&mut self, data: &[u8]) -> Vec<AccessUnit> {
        self.buf.extend_from_slice(data);
        let mut out = Vec::new();
        // Drop any garbage before the first start code.
        let Some((mut consumed, _)) = find_start_code(&self.buf, 0) else {
            // Keep the tail: it may be the first bytes of a start code split across reads.
            let keep = self.buf.len().saturating_sub(2);
            self.buf.drain(..keep);
            return out;
        };
        while let Some((_, nal_start)) = find_start_code(&self.buf, consumed) {
            let Some((next_start, _)) = find_start_code(&self.buf, nal_start) else {
                break;
            };
            let mut nal = &self.buf[nal_start..next_start];
            while nal.last() == Some(&0) {
                nal = &nal[..nal.len() - 1];
            }
            if !nal.is_empty() {
                if nal_type(nal) == NAL_AUD {
                    if !self.cur.nals.is_empty() {
                        out.push(std::mem::take(&mut self.cur));
                    }
                } else {
                    self.cur.nals.push(nal.to_vec());
                }
            }
            consumed = next_start;
        }
        self.buf.drain(..consumed);
        out
    }
}

/// `avc1.PPCCLL` from the SPS NAL (header byte, then profile, constraints, level).
pub fn codec_string(sps: &[u8]) -> Option<String> {
    (sps.len() >= 4).then(|| format!("avc1.{:02x}{:02x}{:02x}", sps[1], sps[2], sps[3]))
}

/// AVCDecoderConfigurationRecord for VideoDecoder `description`.
pub fn avcc_description(sps: &[u8], pps: &[u8]) -> Option<Vec<u8>> {
    if sps.len() < 4 {
        return None;
    }
    let mut d = vec![1, sps[1], sps[2], sps[3], 0xff, 0xe1];
    d.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    d.extend_from_slice(sps);
    d.push(1);
    d.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    d.extend_from_slice(pps);
    Some(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPS: [u8; 6] = [0x67, 0x64, 0x00, 0x33, 0xaa, 0xbb];
    const PPS: [u8; 3] = [0x68, 0xce, 0x38];
    const AUD: [u8; 2] = [0x09, 0xf0];
    const IDR: [u8; 5] = [0x65, 0x88, 0x84, 0x00, 0x01];
    const P: [u8; 4] = [0x41, 0x9a, 0x02, 0x03];

    fn stream() -> Vec<u8> {
        let mut s = Vec::new();
        // mix 4-byte and 3-byte start codes
        for (i, nal) in [&AUD[..], &SPS, &PPS, &IDR, &AUD, &P, &AUD, &P]
            .iter()
            .enumerate()
        {
            if i % 2 == 0 {
                s.extend_from_slice(&[0, 0, 0, 1]);
            } else {
                s.extend_from_slice(&[0, 0, 1]);
            }
            s.extend_from_slice(nal);
        }
        s.extend_from_slice(&[0, 0, 0, 1]); // trailing start code so last NAL completes
        s
    }

    #[test]
    fn splits_access_units_at_any_chunk_size() {
        let data = stream();
        for chunk in [1usize, 2, 3, 5, 7, 64, data.len()] {
            let mut asm = AuAssembler::default();
            let mut aus = Vec::new();
            for c in data.chunks(chunk) {
                aus.extend(asm.push(c));
            }
            assert_eq!(aus.len(), 2, "chunk={chunk}");
            assert!(aus[0].is_key());
            assert_eq!(aus[0].find(NAL_SPS), Some(&SPS[..]));
            assert_eq!(aus[0].find(NAL_PPS), Some(&PPS[..]));
            assert!(!aus[1].is_key());
            assert_eq!(aus[1].nals, vec![P.to_vec()]);
        }
    }

    #[test]
    fn avcc_has_only_slices_length_prefixed() {
        let au = AccessUnit {
            nals: vec![SPS.to_vec(), PPS.to_vec(), IDR.to_vec()],
        };
        let mut want = (IDR.len() as u32).to_be_bytes().to_vec();
        want.extend_from_slice(&IDR);
        assert_eq!(au.to_avcc(), want);
    }

    #[test]
    fn codec_and_description() {
        assert_eq!(codec_string(&SPS).as_deref(), Some("avc1.640033"));
        let d = avcc_description(&SPS, &PPS).unwrap();
        assert_eq!(&d[..6], &[1, 0x64, 0x00, 0x33, 0xff, 0xe1]);
        assert_eq!(u16::from_be_bytes([d[6], d[7]]) as usize, SPS.len());
        assert_eq!(&d[8..8 + SPS.len()], &SPS);
        let p = 8 + SPS.len();
        assert_eq!(d[p], 1);
        assert_eq!(&d[p + 3..], &PPS);
    }

    #[test]
    fn garbage_before_first_start_code_is_dropped() {
        let mut asm = AuAssembler::default();
        let mut data = vec![9, 9, 9];
        data.extend(stream());
        let aus = asm.push(&data);
        assert_eq!(aus.len(), 2);
    }
}
