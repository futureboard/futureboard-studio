//! Optional descriptive metadata attached to an exported audio file.
//!
//! Container support is best-effort: WAV/RAUF ignore it for now, FLAC does not
//! write Vorbis comments yet, and Opus writes every field as a Vorbis comment
//! in its OpusTags header. Missing metadata never blocks an export.

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AudioMetadata {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    /// Free-form date string (e.g. an ISO `2026-06-13`). Not validated.
    pub date: Option<String>,
    pub comment: Option<String>,
}

impl AudioMetadata {
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.artist.is_none()
            && self.album.is_none()
            && self.date.is_none()
            && self.comment.is_none()
    }

    /// The fields as Vorbis comment `(FIELD, value)` pairs, in a fixed order,
    /// skipping unset and blank ones. Field names follow the Xiph
    /// recommendations (`TITLE`, `ARTIST`, `ALBUM`, `DATE`, `COMMENT`).
    pub fn vorbis_comments(&self) -> Vec<(&'static str, &str)> {
        [
            ("TITLE", &self.title),
            ("ARTIST", &self.artist),
            ("ALBUM", &self.album),
            ("DATE", &self.date),
            ("COMMENT", &self.comment),
        ]
        .into_iter()
        .filter_map(|(field, value)| {
            let value = value.as_deref()?.trim();
            (!value.is_empty()).then_some((field, value))
        })
        .collect()
    }
}
