//! Optional descriptive metadata attached to an exported audio file.
//!
//! Every container Studio exports carries it: Opus as Vorbis comments in its
//! OpusTags header, FLAC as a VORBIS_COMMENT metadata block, WAV as a RIFF
//! `LIST`/`INFO` chunk. RAUF (an internal recording format) has none. Unset and
//! blank fields are left out, and a file whose metadata is entirely empty gets
//! no tag block at all. Missing metadata never blocks an export.

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AudioMetadata {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    /// Free-form date string (a year, or an ISO `2026-06-13`). Not validated.
    pub date: Option<String>,
    pub comment: Option<String>,
    pub album_artist: Option<String>,
    pub composer: Option<String>,
    pub genre: Option<String>,
    /// Free-form, e.g. `3` or `3/12`. Not validated.
    pub track_number: Option<String>,
    pub copyright: Option<String>,
    /// International Standard Recording Code. Not validated.
    pub isrc: Option<String>,
}

/// A set value with surrounding whitespace removed; `None` when unset or blank.
fn present(value: &Option<String>) -> Option<&str> {
    let value = value.as_deref()?.trim();
    (!value.is_empty()).then_some(value)
}

impl AudioMetadata {
    /// Whether no field would be written: every one unset or blank.
    pub fn is_empty(&self) -> bool {
        self.vorbis_comments().is_empty()
    }

    /// The fields as Vorbis comment `(FIELD, value)` pairs, in a fixed order,
    /// skipping unset and blank ones. Field names follow the Xiph
    /// recommendations plus the de-facto `ALBUMARTIST`, `COMMENT` and `ISRC`
    /// that taggers read.
    pub fn vorbis_comments(&self) -> Vec<(&'static str, &str)> {
        [
            ("TITLE", &self.title),
            ("ARTIST", &self.artist),
            ("ALBUM", &self.album),
            ("ALBUMARTIST", &self.album_artist),
            ("COMPOSER", &self.composer),
            ("GENRE", &self.genre),
            ("DATE", &self.date),
            ("TRACKNUMBER", &self.track_number),
            ("COMMENT", &self.comment),
            ("COPYRIGHT", &self.copyright),
            ("ISRC", &self.isrc),
        ]
        .into_iter()
        .filter_map(|(field, value)| Some((field, present(value)?)))
        .collect()
    }

    /// A Vorbis comment header body (vendor string, comment count, then each
    /// `FIELD=value`, every length a little-endian u32), without any framing:
    /// the payload of a FLAC VORBIS_COMMENT block, and of `OpusTags` after its
    /// magic.
    pub fn vorbis_comment_body(&self, vendor: &str) -> Vec<u8> {
        let comments = self.vorbis_comments();
        let mut body = Vec::with_capacity(64);
        body.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
        body.extend_from_slice(vendor.as_bytes());
        body.extend_from_slice(&(comments.len() as u32).to_le_bytes());
        for (field, value) in comments {
            let length = field.len() + 1 + value.len();
            body.extend_from_slice(&(length as u32).to_le_bytes());
            body.extend_from_slice(field.as_bytes());
            body.push(b'=');
            body.extend_from_slice(value.as_bytes());
        }
        body
    }

    /// The fields as RIFF `INFO` `(id, value)` pairs, skipping unset and blank
    /// ones. RIFF INFO has no album artist, composer or ISRC id that readers
    /// agree on, so those are left to the formats that have them.
    pub fn riff_info(&self) -> Vec<(&'static [u8; 4], &str)> {
        [
            (b"INAM", &self.title),
            (b"IART", &self.artist),
            (b"IPRD", &self.album),
            (b"IGNR", &self.genre),
            (b"ICRD", &self.date),
            (b"ITRK", &self.track_number),
            (b"ICMT", &self.comment),
            (b"ICOP", &self.copyright),
        ]
        .into_iter()
        .filter_map(|(id, value)| Some((id, present(value)?)))
        .collect()
    }

    /// A complete RIFF `LIST` chunk of type `INFO`, or `None` when no INFO
    /// field is set. Each value is NUL-terminated and every subchunk padded to
    /// an even length, as RIFF requires, so the chunk's own length is even.
    pub fn riff_info_chunk(&self) -> Option<Vec<u8>> {
        let fields = self.riff_info();
        if fields.is_empty() {
            return None;
        }
        let mut payload = b"INFO".to_vec();
        for (id, value) in fields {
            let size = value.len() + 1;
            payload.extend_from_slice(id);
            payload.extend_from_slice(&(size as u32).to_le_bytes());
            payload.extend_from_slice(value.as_bytes());
            payload.push(0);
            if size % 2 == 1 {
                payload.push(0);
            }
        }
        let mut chunk = Vec::with_capacity(8 + payload.len());
        chunk.extend_from_slice(b"LIST");
        chunk.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        chunk.extend_from_slice(&payload);
        Some(chunk)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full() -> AudioMetadata {
        AudioMetadata {
            title: Some("Song".into()),
            artist: Some("Band".into()),
            album: Some("Record".into()),
            date: Some("2026".into()),
            comment: Some("Mixed late".into()),
            album_artist: Some("Various".into()),
            composer: Some("Writer".into()),
            genre: Some("Ambient".into()),
            track_number: Some("3".into()),
            copyright: Some("(c) 2026".into()),
            isrc: Some("USRC17607839".into()),
        }
    }

    #[test]
    fn blank_and_unset_fields_are_empty() {
        assert!(AudioMetadata::default().is_empty());
        let blank = AudioMetadata {
            title: Some("   ".into()),
            isrc: Some(String::new()),
            ..Default::default()
        };
        assert!(blank.is_empty());
        assert!(blank.riff_info_chunk().is_none());
        assert!(!full().is_empty());
    }

    #[test]
    fn every_field_has_its_vorbis_name() {
        let names: Vec<_> = full()
            .vorbis_comments()
            .into_iter()
            .map(|(field, _)| field)
            .collect();
        assert_eq!(
            names,
            [
                "TITLE",
                "ARTIST",
                "ALBUM",
                "ALBUMARTIST",
                "COMPOSER",
                "GENRE",
                "DATE",
                "TRACKNUMBER",
                "COMMENT",
                "COPYRIGHT",
                "ISRC"
            ]
        );
    }

    #[test]
    fn vorbis_comment_body_parses_back() {
        let metadata = AudioMetadata {
            title: Some(" Song ".into()),
            genre: Some("Ambient".into()),
            ..Default::default()
        };
        let body = metadata.vorbis_comment_body("vendor");
        let u32_at = |at: usize| u32::from_le_bytes(body[at..at + 4].try_into().unwrap()) as usize;
        assert_eq!(u32_at(0), 6);
        assert_eq!(&body[4..10], b"vendor");
        assert_eq!(u32_at(10), 2);
        assert_eq!(u32_at(14), "TITLE=Song".len());
        assert_eq!(&body[18..28], b"TITLE=Song");
        assert_eq!(u32_at(28), "GENRE=Ambient".len());
        assert_eq!(&body[32..], b"GENRE=Ambient");
    }

    #[test]
    fn riff_info_chunk_is_padded_and_sized() {
        let metadata = AudioMetadata {
            title: Some("Song".into()),        // 4 + NUL = 5, padded to 6
            artist: Some("Bands".into()),      // 5 + NUL = 6
            isrc: Some("USRC17607839".into()), // no INFO id: left out
            ..Default::default()
        };
        let chunk = metadata.riff_info_chunk().unwrap();
        assert_eq!(&chunk[0..4], b"LIST");
        let size = u32::from_le_bytes(chunk[4..8].try_into().unwrap()) as usize;
        assert_eq!(size, chunk.len() - 8);
        assert_eq!(size % 2, 0);
        assert_eq!(&chunk[8..12], b"INFO");
        assert_eq!(&chunk[12..16], b"INAM");
        assert_eq!(u32::from_le_bytes(chunk[16..20].try_into().unwrap()), 5);
        assert_eq!(&chunk[20..26], b"Song\0\0");
        assert_eq!(&chunk[26..30], b"IART");
        assert_eq!(u32::from_le_bytes(chunk[30..34].try_into().unwrap()), 6);
        assert_eq!(&chunk[34..40], b"Bands\0");
        assert_eq!(chunk.len(), 40);
    }
}
