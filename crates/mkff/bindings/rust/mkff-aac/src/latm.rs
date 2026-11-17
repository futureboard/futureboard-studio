//! LATM / LOAS (14496-3 1.7): `AudioSyncStream` framing and `AudioMuxElement` parsing for the
//! single-program, single-layer case (what DVB / ATSC broadcasts carry).

use crate::asc::AudioSpecificConfig;
use mkff_audio_core::bits::BitReader;
use mkff_audio_core::{Error, Result};

/// Length in bytes of the LOAS `AudioSyncStream` frame at the start of `data` (3-byte header
/// included), or `None` without a sync word or with fewer than 3 bytes.
pub fn loas_frame_length(data: &[u8]) -> Option<usize> {
    if data.len() < 3 || data[0] != 0x56 || data[1] & 0xE0 != 0xE0 {
        return None;
    }
    Some(3 + ((usize::from(data[1] & 0x1F) << 8) | usize::from(data[2])))
}

/// Parses LOAS frames / `AudioMuxElement`s into raw access units, keeping the most recent
/// `StreamMuxConfig`.
#[derive(Clone, Debug, Default)]
pub struct LatmParser {
    config: Option<AudioSpecificConfig>,
    asc_bytes: Vec<u8>,
    audio_mux_version_a: bool,
    num_sub_frames: u32,
    frame_length_type: u32,
    fixed_frame_length: u32,
    other_data_bits: u64,
    config_changed: bool,
}

fn latm_value(br: &mut BitReader) -> Result<u32> {
    let n = br.read(2)?;
    let mut v = 0u32;
    for _ in 0..=n {
        v = (v << 8) | br.read(8)?;
    }
    Ok(v)
}

impl LatmParser {
    /// A parser with no configuration yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// The current decoder configuration (after the first `StreamMuxConfig`).
    pub fn config(&self) -> Option<&AudioSpecificConfig> {
        self.config.as_ref()
    }

    /// The current configuration re-serialised as `AudioSpecificConfig` bytes.
    pub fn asc_bytes(&self) -> &[u8] {
        &self.asc_bytes
    }

    /// True once after a frame that changed the configuration.
    pub fn take_config_changed(&mut self) -> bool {
        std::mem::take(&mut self.config_changed)
    }

    /// Parse one LOAS `AudioSyncStream` frame (sync word `0x2B7` + 13-bit length +
    /// `AudioMuxElement(1)`) and return its access units.
    pub fn parse_loas_frame(&mut self, frame: &[u8]) -> Result<Vec<Vec<u8>>> {
        let len = loas_frame_length(frame).ok_or_else(|| Error::invalid("no LOAS sync word"))?;
        if frame.len() < len {
            return Err(Error::EndOfData);
        }
        self.parse_audio_mux_element(&frame[3..len], true)
    }

    /// Parse an `AudioMuxElement` (`mux_config_present` = 1 for LOAS, 0 for LATM carried with
    /// out-of-band configuration).
    pub fn parse_audio_mux_element(
        &mut self,
        data: &[u8],
        mux_config_present: bool,
    ) -> Result<Vec<Vec<u8>>> {
        let mut br = BitReader::new(data);
        if mux_config_present {
            let use_same = br.read_bit()?;
            if !use_same {
                self.stream_mux_config(&mut br)?;
            }
        }
        if self.config.is_none() {
            return Err(Error::invalid("LATM payload before any StreamMuxConfig"));
        }
        if self.audio_mux_version_a {
            return Err(Error::unsupported("audioMuxVersionA = 1"));
        }
        let mut out = Vec::with_capacity(self.num_sub_frames as usize + 1);
        for _ in 0..=self.num_sub_frames {
            let len = match self.frame_length_type {
                0 => {
                    let mut l = 0usize;
                    loop {
                        let t = br.read(8)?;
                        l += t as usize;
                        if t != 255 {
                            break;
                        }
                    }
                    l
                }
                1 => self.fixed_frame_length as usize + 20,
                t => return Err(Error::unsupported(format!("LATM frameLengthType {t}"))),
            };
            if len * 8 > br.remaining() {
                return Err(Error::EndOfData);
            }
            let mut au = Vec::with_capacity(len);
            for _ in 0..len {
                au.push(br.read(8)? as u8);
            }
            out.push(au);
        }
        // other data and byte alignment are not needed
        let _ = self.other_data_bits;
        Ok(out)
    }

    fn stream_mux_config(&mut self, br: &mut BitReader) -> Result<()> {
        let version = br.read_bit()?;
        self.audio_mux_version_a = if version { br.read_bit()? } else { false };
        if self.audio_mux_version_a {
            return Err(Error::unsupported("audioMuxVersionA = 1"));
        }
        if version {
            latm_value(br)?; // taraBufferFullness
        }
        let _all_same_framing = br.read_bit()?;
        self.num_sub_frames = br.read(6)?;
        let num_program = br.read(4)?;
        let num_layer = br.read(3)?;
        if num_program != 0 || num_layer != 0 {
            return Err(Error::unsupported("LATM with several programs or layers"));
        }
        // the first layer always carries its configuration
        let asc = if version {
            let asc_len = latm_value(br)? as usize;
            let start = br.position();
            let asc = AudioSpecificConfig::parse_bits(br, start + asc_len)?;
            let used = br.position() - start;
            if asc_len < used {
                return Err(Error::invalid("LATM ASC length too short"));
            }
            br.skip(asc_len - used)?;
            asc
        } else {
            let end = br.len_bits();
            AudioSpecificConfig::parse_bits(br, end)?
        };
        self.frame_length_type = br.read(3)?;
        match self.frame_length_type {
            0 => {
                br.skip(8)?; // latmBufferFullness
            }
            1 => self.fixed_frame_length = br.read(9)?,
            t => return Err(Error::unsupported(format!("LATM frameLengthType {t}"))),
        }
        self.other_data_bits = 0;
        if br.read_bit()? {
            if version {
                self.other_data_bits = u64::from(latm_value(br)?);
            } else {
                loop {
                    let esc = br.read_bit()?;
                    self.other_data_bits = (self.other_data_bits << 8) | u64::from(br.read(8)?);
                    if !esc {
                        break;
                    }
                }
            }
        }
        if br.read_bit()? {
            br.skip(8)?; // crcCheckSum
        }
        let bytes = asc.to_bytes();
        if bytes != self.asc_bytes {
            self.config_changed = true;
            self.asc_bytes = bytes;
        }
        self.config = Some(asc);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mkff_audio_core::bits::BitWriter;

    /// Build a LOAS frame with a StreamMuxConfig (audioMuxVersion 0) and one AU.
    pub(crate) fn make_loas(asc: &AudioSpecificConfig, au: &[u8], with_config: bool) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.write_bit(!with_config); // useSameStreamMux
        if with_config {
            w.write_bit(false); // audioMuxVersion
            w.write_bit(true); // allStreamsSameTimeFraming
            w.write(0, 6); // numSubFrames
            w.write(0, 4); // numProgram
            w.write(0, 3); // numLayer
            asc.write(&mut w);
            w.write(0, 3); // frameLengthType
            w.write(0xFF, 8); // latmBufferFullness
            w.write_bit(false); // otherDataPresent
            w.write_bit(false); // crcCheckPresent
        }
        let mut l = au.len();
        while l >= 255 {
            w.write(255, 8);
            l -= 255;
        }
        w.write(l as u32, 8);
        for &b in au {
            w.write(u32::from(b), 8);
        }
        let body = w.finish();
        let mut out = vec![0x56, 0xE0 | (body.len() >> 8) as u8, body.len() as u8];
        out.extend_from_slice(&body);
        out
    }

    #[test]
    fn loas_round_trip() {
        let asc = AudioSpecificConfig::lc(48000, 2);
        let au: Vec<u8> = (0..300).map(|i| (i * 7) as u8).collect();
        let f = make_loas(&asc, &au, true);
        assert_eq!(loas_frame_length(&f), Some(f.len()));
        let mut p = LatmParser::new();
        let aus = p.parse_loas_frame(&f).unwrap();
        assert_eq!(aus, vec![au.clone()]);
        assert_eq!(p.config(), Some(&asc));
        assert!(p.take_config_changed());
        let f2 = make_loas(&asc, &au[..10], false);
        assert_eq!(p.parse_loas_frame(&f2).unwrap(), vec![au[..10].to_vec()]);
        assert!(!p.take_config_changed());
        // truncated / garbage
        assert!(p.parse_loas_frame(&f[..f.len() - 5]).is_err());
        assert!(LatmParser::new().parse_loas_frame(&f2).is_err());
    }
}
