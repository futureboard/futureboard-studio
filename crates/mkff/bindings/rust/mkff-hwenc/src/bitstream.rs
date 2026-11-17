//! H.264 Annex-B → AVCC: split start-code NAL units, keep SPS / PPS for the `avcC`, and write
//! 4-byte length-prefixed access units without AUD / SPS / PPS.

/// NAL units of an Annex-B byte stream (start codes removed, trailing zeros trimmed).
pub fn split_annexb(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    if starts.is_empty() {
        return if data.is_empty() { Vec::new() } else { vec![data] };
    }
    let mut out = Vec::with_capacity(starts.len());
    for (k, &s) in starts.iter().enumerate() {
        let mut e = if k + 1 < starts.len() { starts[k + 1] - 3 } else { data.len() };
        while e > s && data[e - 1] == 0 {
            e -= 1;
        }
        if e > s {
            out.push(&data[s..e]);
        }
    }
    out
}

/// What one encoder output sample became.
pub struct Converted {
    pub avcc_payload: Vec<u8>,
    pub idr: bool,
    pub sps: Option<Vec<u8>>,
    pub pps: Option<Vec<u8>>,
}

/// Annex-B (or already length-prefixed, which some encoders emit) → length-prefixed payload.
pub fn convert(data: &[u8]) -> Converted {
    let nals = if data.len() >= 4 && (data.starts_with(&[0, 0, 1]) || data.starts_with(&[0, 0, 0, 1])) {
        split_annexb(data)
    } else {
        split_length_prefixed(data).unwrap_or_else(|| split_annexb(data))
    };
    let mut c = Converted { avcc_payload: Vec::with_capacity(data.len() + 16), idr: false, sps: None, pps: None };
    for nal in nals {
        match nal[0] & 0x1F {
            7 => c.sps = Some(nal.to_vec()),
            8 => c.pps = Some(nal.to_vec()),
            9 => {}
            t => {
                if t == 5 {
                    c.idr = true;
                }
                c.avcc_payload.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                c.avcc_payload.extend_from_slice(nal);
            }
        }
    }
    c
}

fn split_length_prefixed(data: &[u8]) -> Option<Vec<&[u8]>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < data.len() {
        let n = u32::from_be_bytes(data.get(i..i + 4)?.try_into().ok()?) as usize;
        i += 4;
        out.push(data.get(i..i + n)?);
        i += n;
    }
    (!out.is_empty()).then_some(out)
}

/// AVCDecoderConfigurationRecord (ISO/IEC 14496-15 §5.3.3) for one SPS / PPS, 4-byte lengths.
pub fn avcc(sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let profile = sps.get(1).copied().unwrap_or(77);
    let mut v = vec![1, profile, sps.get(2).copied().unwrap_or(0), sps.get(3).copied().unwrap_or(40), 0xFF, 0xE1];
    v.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    v.extend_from_slice(sps);
    v.push(1);
    v.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    v.extend_from_slice(pps);
    if matches!(profile, 100 | 110 | 122 | 144) {
        // chroma_format_idc 1 (4:2:0), 8-bit luma / chroma, no SPS extensions.
        v.extend_from_slice(&[0xFD, 0xF8, 0xF8, 0x00]);
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annexb_to_avcc() {
        let stream = [0, 0, 0, 1, 0x09, 0xF0, 0, 0, 0, 1, 0x67, 77, 0, 31, 0xAA, 0, 0, 1, 0x68, 0xCE, 0, 0, 1, 0x65, 1, 2, 3, 0];
        let c = convert(&stream);
        assert!(c.idr);
        assert_eq!(c.sps.as_deref(), Some(&[0x67, 77, 0, 31, 0xAA][..]));
        assert_eq!(c.pps.as_deref(), Some(&[0x68, 0xCE][..]));
        assert_eq!(c.avcc_payload, [0, 0, 0, 4, 0x65, 1, 2, 3]);
        let a = avcc(&c.sps.unwrap(), &c.pps.unwrap());
        assert_eq!(&a[..6], &[1, 77, 0, 31, 0xFF, 0xE1]);
    }
}
