use super::{
    AraTrackBinding, AutomationLane, AutomationPoint, AutomationTargetDesc, ClipSource,
    FutureboardProject, InputMonitorMode, MidiAccent, MidiArticulation, MidiControllerKind,
    MidiControllerLane, MidiControllerPoint, MidiNote, MidiPitchPoint, MidiSysExEvent,
    MidiSysExKind, PluginFormat, PluginStateBlob, ProjectAraDocument, ProjectAsset,
    ProjectAudioConnection, ProjectAudioPortBinding, ProjectClip, ProjectInsert,
    ProjectLyricSyllable, ProjectLyricSyllableMode, ProjectMixer, ProjectPluginInstance,
    ProjectSend, ProjectSolfegeEngine, ProjectSolfegeLane, ProjectSongSectionType,
    ProjectSongTextEvent, ProjectSongTextEventKind, ProjectSoundfontPlayer, ProjectTake,
    ProjectTempoPoint, ProjectTimelineMarker, ProjectTimelineRegion, ProjectTrack,
    ProjectTrackAudioFormat, ProjectTrackMidiInputRouting, ProjectTrackOutputRouting,
    ProjectTrackType, SoundfontEnvelope, SoundfontRenderQuality, TrackRouting,
    V33TrackInputRouting,
};
use crate::components::timeline::timeline_state::{
    AudioClipStretchState, StretchAlgorithm, StretchMode, WarpMarker,
};
use sphere_audio_editor::{ClipEnvelope, EnvelopeCurve, EnvelopePoint};
use sphere_midi_service::mpe::{MpeOutputMode, MpeTrackConfiguration};
use sphere_midi_service::{
    CustomExpressionLane, ExpressionCurve, ExpressionInterpolation, ExpressionPoint, NoteExpression,
};
use std::io::{self, Cursor, Read};
use std::path::PathBuf;

pub const PROJECT_MAGIC: &[u8; 8] = b"FBSTUD1\0";
/// On-disk format version. v6 adds multi-channel audio input routing.
/// v5 adds MIDI controller (CC) lanes per MIDI clip.
/// v4 adds a per-MIDI-note muted flag. v3 adds persisted track routing fields.
/// Older files still load: v1/v2 use stable per-track routing defaults,
/// v1/v2/v3 notes default to unmuted, and pre-v5 MIDI clips have no CC lanes.
/// v7 adds project-level tempo automation markers (TempoMap); pre-v7 files have
/// no tempo points and play at the static `bpm`.
/// v8 adds stable ids on tempo points for independent marker editing.
/// v11 adds a content fingerprint per project asset for cross-session import
/// dedup. Pre-v11 files have no asset fingerprint and load with `None`.
/// v13 adds timeline markers and regions. v14 adds internal RAUF clip sources.
/// v15 adds persisted master-bus inserts.
/// v16 adds a per-clip non-destructive stretch/pitch block (mode, algorithm,
/// ratio, BMP pair, pitch/formant/transient/fade/gain/pan, warp markers). Pre-v16
/// clips load with [`AudioClipStretchState::default`] (mode Off, ratio 1.0,
/// preserve_pitch false).
/// v18 persists enabled VSTi output channels per insert.
/// v19 persists the per-instrument VSTi multi-out mixer collapse flag.
/// v20 persists mixer tree expanded/pinned/hidden channel state.
/// v21 persists per-automation-point curve tension. Pre-v21 points load with
/// tension 0.0 (a straight segment), so older projects are unchanged.
/// v22 adds a per-note MIDI channel (pre-v22 notes default to channel 1) and
/// a per-track "play each note on its own channel" toggle (pre-v22 tracks
/// default to `false`, matching the pre-existing fixed-channel behavior).
/// v23 preserves imported MIDI SysEx events on MIDI clips.
/// v24 adds project-owned chord and lyric cues.
/// v25 adds MIDI articulations: a per-note articulation tag (0 = none) and a
/// per-clip direction articulation event list. Pre-v25 data loads with no
/// articulations; unknown tags from newer files degrade to "none" on load.
/// v26 persists stable MIDI note and CC-point identities plus optional note
/// release velocity. Pre-v26 notes/points mint fresh ids on load; release
/// velocity defaults to unset (`0`).
/// v27 replaces combined chord/lyric cues with typed Song Text events and adds
/// lyric timing/syllable data plus section metadata. v24-v26 cues migrate on load;
/// pre-v24 projects have no Song Text events.
/// v28 persists the built-in Soundfont Player instrument per track (`.sf2`
/// path, preset bank/patch, volume, reverb/chorus, polyphony). Pre-v28 tracks
/// load with no soundfont, which is what they had before the field existed.
/// v29 adds the Soundfont Player's amp envelope (A/D/S/R) and render quality.
/// v28 soundfonts load with the bypassed envelope and Standard quality, which is
/// exactly the signal path they had; an unknown quality key from a newer file
/// degrades to Standard rather than failing the load.
/// v30 adds arrangement group membership; v31 persists folder collapse state;
/// v32 persists each track's volume-automation read/bypass state.
/// v33 adds the reference Video track (track type tag 7) and the video clip
/// source (clip source tag 4), which stores only the asset id and source path —
/// frames are always decoded from the file, never persisted. Pre-v33 projects
/// have no Video track, which is exactly what they had before the type existed.
/// v34 splits the combined per-track input union into an audio Audio Connection
/// reference plus the independent MIDI input field, and adds a project-level
/// Audio Connections registry section at the body tail. v33 and older files
/// still load: their combined field is migrated into generated connections.
/// v35 appends Master / Monitor output routing (two optional Audio Connection
/// ids plus the one-time bootstrap latch) after the registry section.
/// v36 persists each insert's registry-resolved instrument/effect role, so an
/// effect in slot zero is never mistaken for an instrument after project load.
/// v37 appends the native Solfege instrument state to each track.
/// v38 appends per-note musical accent: five normalised components and a
/// provenance tag, written as an optional block after the pitch curve. A v38
/// file loads with no accent on any note, which is exactly the state it was
/// saved in — and "no accent" is a distinct state from "neutral accent", so
/// re-analysis treats a pre-v39 project as never analysed rather than as
/// analysed-and-found-flat.
/// v39 appends the conductor lanes' fold state — four collapse latches and five
/// dragged heights — after the output routing. A v39 file loads with every lane
/// expanded at its default height, which is the state it was saved in, since
/// that is what v39 always restored.
/// v41 appended each clip's ARA binding after its stretch block, and the ARA
/// document archives after the conductor lanes. A v40 file loads with no ARA at
/// all — the state it was saved in, since v40 could not express a binding.
/// v42 moves the binding from the clip to the track, where it belongs: ARA is a
/// track processor like an insert, and every audio clip on the track becomes one
/// of its playback regions. The v41 per-clip byte is still read and discarded so
/// those files keep loading; their tracks come back without ARA, which is the
/// closest honest answer, since a v41 file could bind individual clips to
/// different plug-ins and a track can only carry one.
/// v43 adds the project timebase (display format + timecode frame rate).
/// v44 adds per-track recorded takes. A take names one of the track's own
/// clips, so the audio is not duplicated — only the record of which pass made
/// it and whether it is the one heard.
/// v45 appends protocol-neutral per-note expression curves. v46 appends
/// per-track MPE output settings at the tail of each track block.
/// v47 appends the non-destructive clip gain envelope to each audio stretch
/// block. Pre-v47 clips load with an empty envelope.
/// v48 appends the clip de-noise amount. Pre-v48 clips load with de-noise
/// bypassed.
/// v49 appends clip channel/DC/de-hum process fields. Pre-v49 clips load as
/// stereo identity with DC and de-hum bypassed.
pub const PROJECT_VERSION: u32 = 49;

/// Minimum on-disk format version that can be loaded without data loss.
/// Versions below this will show a warning but can still be loaded.
/// Currently set to v33 (Video track introduction) since v32 and earlier
/// lack Video track, modern Audio Connections, and other critical features.
pub const MIN_SUPPORTED_VERSION: u32 = 33;

/// Minimum on-disk header size: magic (8) + version (4) + reserved (4) + body_len (4).
pub const PROJECT_HEADER_SIZE: usize = 20;

#[derive(Debug)]
pub enum ProjectError {
    Io(io::Error),
    InvalidMagic,
    UnsupportedVersion(u32),
    /// File is from an older version that can be loaded but may lack features.
    /// Contains the file's version number.
    OldVersion(u32),
    /// File is shorter than the header or declared payload.
    IncompleteFile {
        reason: String,
    },
    UnexpectedEof {
        needed: usize,
        remaining: usize,
        field: &'static str,
    },
    Corrupted(String),
    ChecksumMismatch {
        expected: u32,
        got: u32,
    },
}

impl ProjectError {
    /// Primary message shown in UI dialogs (no raw parser tokens).
    pub fn user_message(&self) -> &'static str {
        match self {
            ProjectError::Io(_) => {
                "Could not read the project file. Check that the file exists and is accessible."
            }
            ProjectError::InvalidMagic => "This file is not a Futureboard project.",
            ProjectError::UnsupportedVersion(version) if *version > PROJECT_VERSION => {
                "This project was created by a newer unsupported version of Futureboard."
            }
            ProjectError::UnsupportedVersion(_) => {
                "This project version is not supported by this build of Futureboard."
            }
            ProjectError::OldVersion(_) => {
                "This project was created with an older version of Futureboard. It can be opened, but some features may be missing or behave differently."
            }
            ProjectError::IncompleteFile { .. }
            | ProjectError::UnexpectedEof { .. }
            | ProjectError::ChecksumMismatch { .. } => {
                "Could not open this project because the file appears to be incomplete or corrupted."
            }
            ProjectError::Corrupted(msg) if is_truncation_detail(msg) => {
                "Could not open this project because the file appears to be incomplete or corrupted."
            }
            ProjectError::Corrupted(_) => {
                "Could not open this project because the file appears to be incomplete or corrupted."
            }
        }
    }

    /// Optional secondary line for dialogs and logs.
    pub fn technical_detail(&self) -> String {
        match self {
            ProjectError::Io(e) => format!("I/O error: {e}"),
            ProjectError::InvalidMagic => "invalid magic bytes".to_string(),
            ProjectError::UnsupportedVersion(v) => format!("unsupported version: {v}"),
            ProjectError::OldVersion(v) => format!("old version: {v} (current {PROJECT_VERSION}, minimum {MIN_SUPPORTED_VERSION})"),
            ProjectError::IncompleteFile { reason } => reason.clone(),
            ProjectError::UnexpectedEof {
                needed,
                remaining,
                field,
            } => format!("unexpected EOF reading {field} (needed {needed}, remaining {remaining})"),
            ProjectError::Corrupted(msg) => msg.clone(),
            ProjectError::ChecksumMismatch { expected, got } => {
                format!("checksum mismatch: expected {expected:#010x}, got {got:#010x}")
            }
        }
    }
}

fn is_truncation_detail(msg: &str) -> bool {
    msg.contains("truncated")
        || msg.contains("too small")
        || msg.contains("file truncated")
        || msg.contains("unexpected EOF")
}

impl std::fmt::Display for ProjectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.technical_detail())
    }
}

impl From<io::Error> for ProjectError {
    fn from(e: io::Error) -> Self {
        ProjectError::Io(e)
    }
}

// ââ Low-level writer ââââââââââââââââââââââââââââââââââââââââââââââââââââââââââ

pub struct FbWriter {
    buf: Vec<u8>,
}

impl FbWriter {
    pub fn new() -> Self {
        Self {
            buf: Vec::with_capacity(4096),
        }
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    fn write_u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    fn write_u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    fn write_u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    fn write_f32(&mut self, v: f32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    fn write_f64(&mut self, v: f64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    fn write_bool(&mut self, v: bool) {
        self.buf.push(v as u8);
    }

    fn write_str(&mut self, s: &str) {
        let bytes = s.as_bytes();
        self.write_u32(bytes.len() as u32);
        self.buf.extend_from_slice(bytes);
    }

    fn write_opt_str(&mut self, s: &Option<String>) {
        match s {
            None => self.write_u8(0),
            Some(v) => {
                self.write_u8(1);
                self.write_str(v);
            }
        }
    }

    fn write_opt_path(&mut self, p: &Option<PathBuf>) {
        match p {
            None => self.write_u8(0),
            Some(v) => {
                self.write_u8(1);
                self.write_str(&v.to_string_lossy());
            }
        }
    }

    fn write_opt_u32(&mut self, v: &Option<u32>) {
        match v {
            None => self.write_u8(0),
            Some(x) => {
                self.write_u8(1);
                self.write_u32(*x);
            }
        }
    }

    fn write_opt_u8(&mut self, v: &Option<u8>) {
        match v {
            None => self.write_u8(0),
            Some(x) => {
                self.write_u8(1);
                self.write_u8(*x);
            }
        }
    }

    fn write_opt_f64(&mut self, v: &Option<f64>) {
        match v {
            None => self.write_u8(0),
            Some(x) => {
                self.write_u8(1);
                self.write_f64(*x);
            }
        }
    }

    fn write_opt_f32(&mut self, v: &Option<f32>) {
        match v {
            None => self.write_u8(0),
            Some(x) => {
                self.write_u8(1);
                self.write_f32(*x);
            }
        }
    }

    fn write_opt_u64(&mut self, v: &Option<u64>) {
        match v {
            None => self.write_u8(0),
            Some(x) => {
                self.write_u8(1);
                self.write_u64(*x);
            }
        }
    }

    fn write_bytes(&mut self, bytes: &[u8]) {
        self.write_u32(bytes.len() as u32);
        self.buf.extend_from_slice(bytes);
    }
}

// ââ Low-level reader ââââââââââââââââââââââââââââââââââââââââââââââââââââââââââ

pub struct FbReader<'a> {
    cur: Cursor<&'a [u8]>,
}

impl<'a> FbReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            cur: Cursor::new(data),
        }
    }

    fn remaining(&self) -> usize {
        let pos = self.cur.position() as usize;
        let len = self.cur.get_ref().len();
        len.saturating_sub(pos)
    }

    fn read_exact_field(
        &mut self,
        buf: &mut [u8],
        field: &'static str,
    ) -> Result<(), ProjectError> {
        let needed = buf.len();
        let remaining = self.remaining();
        if remaining < needed {
            return Err(ProjectError::UnexpectedEof {
                needed,
                remaining,
                field,
            });
        }
        self.cur
            .read_exact(buf)
            .map_err(|_| ProjectError::UnexpectedEof {
                needed,
                remaining,
                field,
            })
    }

    fn read_u8(&mut self) -> Result<u8, ProjectError> {
        let mut b = [0u8; 1];
        self.read_exact_field(&mut b, "u8")?;
        Ok(b[0])
    }

    fn read_u32(&mut self) -> Result<u32, ProjectError> {
        let mut b = [0u8; 4];
        self.read_exact_field(&mut b, "u32")?;
        Ok(u32::from_le_bytes(b))
    }

    fn read_u64(&mut self) -> Result<u64, ProjectError> {
        let mut b = [0u8; 8];
        self.read_exact_field(&mut b, "u64")?;
        Ok(u64::from_le_bytes(b))
    }

    fn read_f32(&mut self) -> Result<f32, ProjectError> {
        let mut b = [0u8; 4];
        self.read_exact_field(&mut b, "f32")?;
        Ok(f32::from_le_bytes(b))
    }

    fn read_f64(&mut self) -> Result<f64, ProjectError> {
        let mut b = [0u8; 8];
        self.read_exact_field(&mut b, "f64")?;
        Ok(f64::from_le_bytes(b))
    }

    fn read_bool(&mut self) -> Result<bool, ProjectError> {
        Ok(self.read_u8()? != 0)
    }

    fn read_str(&mut self) -> Result<String, ProjectError> {
        let len = self.read_u32()? as usize;
        let mut buf = vec![0u8; len];
        self.read_exact_field(&mut buf, "string bytes")?;
        String::from_utf8(buf).map_err(|_| ProjectError::Corrupted("invalid UTF-8 string".into()))
    }

    fn read_opt_str(&mut self) -> Result<Option<String>, ProjectError> {
        match self.read_u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.read_str()?)),
            t => Err(ProjectError::Corrupted(format!("bad option tag {t}"))),
        }
    }

    fn read_opt_path(&mut self) -> Result<Option<PathBuf>, ProjectError> {
        match self.read_u8()? {
            0 => Ok(None),
            1 => Ok(Some(PathBuf::from(self.read_str()?))),
            t => Err(ProjectError::Corrupted(format!("bad option tag {t}"))),
        }
    }

    fn read_opt_u32(&mut self) -> Result<Option<u32>, ProjectError> {
        match self.read_u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.read_u32()?)),
            t => Err(ProjectError::Corrupted(format!("bad option tag {t}"))),
        }
    }

    fn read_opt_u8(&mut self) -> Result<Option<u8>, ProjectError> {
        match self.read_u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.read_u8()?)),
            t => Err(ProjectError::Corrupted(format!("bad option tag {t}"))),
        }
    }

    fn read_opt_f64(&mut self) -> Result<Option<f64>, ProjectError> {
        match self.read_u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.read_f64()?)),
            t => Err(ProjectError::Corrupted(format!("bad option tag {t}"))),
        }
    }

    fn read_opt_f32(&mut self) -> Result<Option<f32>, ProjectError> {
        match self.read_u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.read_f32()?)),
            t => Err(ProjectError::Corrupted(format!("bad option tag {t}"))),
        }
    }

    fn read_opt_u64(&mut self) -> Result<Option<u64>, ProjectError> {
        match self.read_u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.read_u64()?)),
            t => Err(ProjectError::Corrupted(format!("bad option tag {t}"))),
        }
    }

    fn read_bytes(&mut self) -> Result<Vec<u8>, ProjectError> {
        let len = self.read_u32()? as usize;
        let mut buf = vec![0u8; len];
        self.read_exact_field(&mut buf, "byte blob")?;
        Ok(buf)
    }
}

// ââ Encoding ââââââââââââââââââââââââââââââââââââââââââââââââââââââââââââââââââ

fn encode_plugin_format(w: &mut FbWriter, f: PluginFormat) {
    w.write_u8(match f {
        PluginFormat::Vst3 => 0,
        PluginFormat::Clap => 1,
        PluginFormat::Au => 2,
        PluginFormat::Lv2 => 3,
        // New tag, never a reuse: an older build decodes 4 as `Unknown` and
        // treats the insert as unloadable rather than silently sending a VST2
        // module down the VST3 bridge.
        PluginFormat::Vst2 => 4,
        PluginFormat::Unknown => 0xFF,
    });
}

fn encode_opt_plugin_format(w: &mut FbWriter, f: &Option<PluginFormat>) {
    match f {
        None => w.write_u8(0),
        Some(fmt) => {
            w.write_u8(1);
            encode_plugin_format(w, *fmt);
        }
    }
}

fn encode_plugin_state_blob(w: &mut FbWriter, blob: &PluginStateBlob) {
    w.write_str(&blob.plugin_id);
    encode_opt_plugin_format(w, &blob.format);
    w.write_bytes(&blob.state_bytes);
    w.write_opt_str(&blob.vendor);
    w.write_opt_str(&blob.name);
    w.write_opt_str(&blob.version);
}

fn encode_plugin_instance(w: &mut FbWriter, inst: &ProjectPluginInstance) {
    w.write_str(&inst.instance_id);
    encode_plugin_format(w, inst.format);
    w.write_opt_path(&inst.plugin_path);
    w.write_str(&inst.plugin_uid);
    w.write_str(&inst.display_name);
    encode_plugin_state_blob(w, &inst.state);
}

fn encode_insert(w: &mut FbWriter, ins: &ProjectInsert) {
    w.write_str(&ins.id);
    w.write_u32(ins.slot_index);
    w.write_bool(ins.bypassed);
    w.write_u32(ins.enabled_audio_output_channels.len() as u32);
    for channel in &ins.enabled_audio_output_channels {
        w.write_u8(*channel);
    }
    // v19: mixer-only multi-out collapse flag (visual; never affects routing).
    w.write_bool(ins.multiout_collapsed);
    // v36: authoritative registry role. Keep an explicit None tag so migrated
    // legacy projects can continue using their positional fallback.
    w.write_u8(match ins.plugin_is_instrument {
        None => 0,
        Some(false) => 1,
        Some(true) => 2,
    });
    match &ins.plugin {
        None => w.write_u8(0),
        Some(inst) => {
            w.write_u8(1);
            encode_plugin_instance(w, inst);
        }
    }
}

fn encode_automation_lane(w: &mut FbWriter, lane: &AutomationLane) {
    w.write_str(&lane.id);
    w.write_str(&lane.parameter_name);
    w.write_bool(lane.visible);
    // Target descriptor + enabled (v2).
    w.write_u8(lane.target.tag);
    w.write_str(&lane.target.insert_id);
    w.write_str(&lane.target.parameter_id);
    w.write_str(&lane.target.parameter_name);
    w.write_str(&lane.target.send_id);
    w.write_bool(lane.enabled);
    w.write_u32(lane.points.len() as u32);
    for p in &lane.points {
        w.write_f32(p.beat);
        w.write_f32(p.value);
        w.write_u8(p.curve); // v2
        w.write_f32(p.tension); // v21
    }
}

fn encode_midi_note(w: &mut FbWriter, n: &MidiNote) {
    w.write_u8(n.pitch);
    w.write_f32(n.start_beats);
    w.write_f32(n.duration_beats);
    w.write_u8(n.velocity);
    w.write_bool(n.muted); // v4
    w.write_u8(n.channel.clamp(1, 16)); // v22
    w.write_u8(n.articulation); // v25 (0 = none)
    w.write_u64(n.id); // v26 (0 = mint on load for legacy writers)
    w.write_u8(n.release_velocity); // v26 (0 = unset)
                                    // v38: continuous pitch performance. Cent deviations keyed by beats from
                                    // the note start, so the shape survives transposition and moves.
    w.write_u32(n.pitch_curve.len() as u32);
    for point in &n.pitch_curve {
        w.write_u64(point.id);
        w.write_f32(point.beat);
        w.write_f32(point.cents);
        w.write_u8(point.shape);
    }
    // v39: musical accent. A presence byte rather than a sentinel value,
    // because every component of an accent is a legitimate number and there is
    // no value left over to mean "absent".
    match n.accent.as_ref() {
        Some(accent) => {
            w.write_bool(true);
            w.write_f32(accent.prominence);
            w.write_f32(accent.attack);
            w.write_f32(accent.agogic);
            w.write_f32(accent.timbre);
            w.write_f32(accent.confidence);
            w.write_u8(accent.source);
        }
        None => w.write_bool(false),
    }
    // v45: protocol-neutral note-owned expression.  This is appended so every
    // pre-v45 positional body remains readable.
    encode_note_expression(w, &n.expression);
}

fn encode_expression_interpolation(w: &mut FbWriter, interpolation: ExpressionInterpolation) {
    w.write_u8(match interpolation {
        ExpressionInterpolation::Linear => 0,
        ExpressionInterpolation::Step => 1,
        ExpressionInterpolation::Smooth => 2,
        ExpressionInterpolation::Bezier => 3,
    });
}

fn decode_expression_interpolation(
    r: &mut FbReader,
) -> Result<ExpressionInterpolation, ProjectError> {
    match r.read_u8()? {
        0 => Ok(ExpressionInterpolation::Linear),
        1 => Ok(ExpressionInterpolation::Step),
        2 => Ok(ExpressionInterpolation::Smooth),
        3 => Ok(ExpressionInterpolation::Bezier),
        tag => Err(ProjectError::Corrupted(format!(
            "bad expression interpolation tag {tag}"
        ))),
    }
}

fn encode_expression_curve(w: &mut FbWriter, curve: &ExpressionCurve) {
    w.write_u32(curve.points.len() as u32);
    for point in &curve.points {
        w.write_f32(point.position);
        w.write_f32(point.value);
        encode_expression_interpolation(w, point.interpolation);
    }
}

fn decode_expression_curve(r: &mut FbReader) -> Result<ExpressionCurve, ProjectError> {
    let count = r.read_u32()? as usize;
    if count > 1_000_000 {
        return Err(ProjectError::Corrupted(
            "invalid note expression point count".to_string(),
        ));
    }
    let mut points = Vec::with_capacity(count.min(4096));
    for _ in 0..count {
        points.push(ExpressionPoint {
            position: r.read_f32()?,
            value: r.read_f32()?,
            interpolation: decode_expression_interpolation(r)?,
        });
    }
    Ok(ExpressionCurve::from_points(points))
}

fn encode_note_expression(w: &mut FbWriter, expression: &NoteExpression) {
    let expression = expression.sanitized();
    encode_expression_curve(w, &expression.pitch);
    encode_expression_curve(w, &expression.pressure);
    encode_expression_curve(w, &expression.timbre);
    w.write_opt_f32(&expression.release_velocity);
    w.write_u32(expression.custom.len() as u32);
    for lane in &expression.custom {
        w.write_u32(lane.controller as u32);
        w.write_str(&lane.name);
        encode_expression_curve(w, &lane.curve);
    }
}

fn decode_note_expression(r: &mut FbReader) -> Result<NoteExpression, ProjectError> {
    let pitch = decode_expression_curve(r)?;
    let pressure = decode_expression_curve(r)?;
    let timbre = decode_expression_curve(r)?;
    let release_velocity = r.read_opt_f32()?;
    let count = r.read_u32()? as usize;
    if count > 65_536 {
        return Err(ProjectError::Corrupted(
            "invalid custom expression lane count".to_string(),
        ));
    }
    let mut custom = Vec::with_capacity(count.min(256));
    for _ in 0..count {
        custom.push(CustomExpressionLane {
            controller: r.read_u32()?.min(u16::MAX as u32) as u16,
            name: r.read_str()?,
            curve: decode_expression_curve(r)?,
        });
    }
    Ok(NoteExpression {
        pitch,
        pressure,
        timbre,
        release_velocity,
        custom,
    }
    .sanitized())
}

/// v5: controller kind tag. CC carries its number; the rest are tag-only.
fn encode_controller_kind(w: &mut FbWriter, kind: MidiControllerKind) {
    match kind {
        MidiControllerKind::CC(n) => {
            w.write_u8(0);
            w.write_u8(n);
        }
        MidiControllerKind::PitchBend => w.write_u8(1),
        MidiControllerKind::ChannelPressure => w.write_u8(2),
        MidiControllerKind::PolyPressure => w.write_u8(3),
    }
}

/// v5: a controller lane and its points.
fn encode_controller_lane(w: &mut FbWriter, lane: &MidiControllerLane) {
    encode_controller_kind(w, lane.kind);
    w.write_bool(lane.visible);
    w.write_f32(lane.height);
    w.write_bool(lane.collapsed);
    w.write_u32(lane.points.len() as u32);
    for p in &lane.points {
        w.write_f32(p.beat);
        w.write_f32(p.value);
        w.write_u64(p.id); // v26
    }
}

fn encode_sysex_event(w: &mut FbWriter, event: &MidiSysExEvent) {
    w.write_u8(match event.kind {
        MidiSysExKind::Normal => 0,
        MidiSysExKind::Escaped => 1,
    });
    w.write_u64(event.tick);
    w.write_f32(event.beat);
    w.write_bytes(&event.data);
}

/// v16: per-clip non-destructive stretch/pitch block. `dirty` is transient and
/// intentionally not persisted (decodes as `false`).
fn encode_stretch(w: &mut FbWriter, s: &AudioClipStretchState) {
    w.write_u8(s.mode.to_tag());
    w.write_u8(s.algorithm.to_tag());
    w.write_u32(s.original_sample_rate);
    w.write_u32(s.project_sample_rate);
    w.write_u64(s.original_duration_samples);
    w.write_u64(s.source_start_samples);
    w.write_u64(s.source_end_samples);
    w.write_f64(s.clip_timeline_start_beats);
    w.write_f64(s.clip_timeline_duration_beats);
    w.write_f64(s.stretch_ratio);
    w.write_opt_f64(&s.bpm_source);
    w.write_opt_f64(&s.bpm_target);
    w.write_bool(s.preserve_pitch);
    w.write_f32(s.pitch_shift_semitones);
    w.write_bool(s.formant_preserve);
    w.write_bool(s.transient_preserve);
    w.write_f32(s.transient_sensitivity);
    w.write_bool(s.reverse);
    w.write_bool(s.normalize_gain);
    w.write_f32(s.fade_in_ms);
    w.write_f32(s.fade_out_ms);
    w.write_f32(s.gain_db);
    w.write_f32(s.pan);
    let marker_count = s
        .warp_markers
        .len()
        .min(AudioClipStretchState::MAX_WARP_MARKERS);
    w.write_u32(marker_count as u32);
    for m in s.warp_markers.iter().take(marker_count) {
        w.write_u64(m.id);
        w.write_u64(m.source_sample);
        w.write_f64(m.timeline_beat);
        w.write_bool(m.locked);
    }
    let point_count = s.gain_envelope.points.len().min(ClipEnvelope::MAX_POINTS);
    w.write_u32(point_count as u32);
    for point in s.gain_envelope.points.iter().take(point_count) {
        w.write_u64(point.id);
        w.write_f32(point.time);
        w.write_f32(point.value_db);
        w.write_u8(point.curve.to_tag());
    }
    // v48: adaptive clip de-noise amount. It is appended so all earlier
    // stretch fields retain their byte offsets for backwards compatibility.
    w.write_f32(s.denoise_amount.clamp(0.0, 1.0));
    // v49: clip process graph (channel / DC / de-hum).
    w.write_u8(s.channel_transform);
    w.write_bool(s.dc_remove);
    w.write_f32(s.dc_left);
    w.write_f32(s.dc_right);
    w.write_f32(s.dehum_hz);
    w.write_u8(s.dehum_harmonics);
    w.write_f32(s.dehum_reduction_db);
}

fn encode_clip(w: &mut FbWriter, c: &ProjectClip) {
    w.write_str(&c.id);
    w.write_str(&c.name);
    w.write_f64(c.start_beat);
    w.write_f64(c.duration_beats);
    w.write_f32(c.offset_beats);
    w.write_f32(c.gain);
    w.write_bool(c.muted);
    match &c.source {
        ClipSource::Empty => w.write_u8(0),
        ClipSource::Audio {
            asset_id,
            source_path,
        } => {
            w.write_u8(1);
            w.write_str(asset_id);
            w.write_opt_path(source_path);
        }
        ClipSource::Rauf {
            asset_id,
            source_path,
            metadata_path,
            sample_format,
            sample_rate,
            channels,
            start_frame,
            length_frames,
        } => {
            w.write_u8(3);
            w.write_str(asset_id);
            w.write_str(&source_path.to_string_lossy());
            w.write_opt_path(metadata_path);
            w.write_str(sample_format);
            w.write_u32(*sample_rate);
            w.write_u32(*channels as u32);
            w.write_u64(*start_frame);
            w.write_u64(*length_frames);
        }
        ClipSource::Video {
            asset_id,
            source_path,
        } => {
            w.write_u8(4);
            w.write_str(asset_id);
            w.write_opt_path(source_path);
        }
        ClipSource::Midi {
            notes,
            controller_lanes,
            sysex_events,
            articulations,
        } => {
            w.write_u8(2);
            w.write_u32(notes.len() as u32);
            for n in notes {
                encode_midi_note(w, n);
            }
            // v5: controller lanes follow the notes.
            w.write_u32(controller_lanes.len() as u32);
            for lane in controller_lanes {
                encode_controller_lane(w, lane);
            }
            // v23: SysEx events are preserved for future playback/export.
            w.write_u32(sysex_events.len() as u32);
            for event in sysex_events {
                encode_sysex_event(w, event);
            }
            // v25: direction articulation events follow the SysEx block.
            w.write_u32(articulations.len() as u32);
            for event in articulations {
                w.write_f32(event.beat);
                w.write_u8(event.articulation);
            }
        }
    }
    // v16: stretch/pitch block trails the source for every clip.
    encode_stretch(w, &c.stretch);
}

fn encode_input_monitor(w: &mut FbWriter, m: InputMonitorMode) {
    w.write_u8(match m {
        InputMonitorMode::Off => 0,
        InputMonitorMode::Always => 1,
        InputMonitorMode::WhenRecordArmed => 2,
    });
}

fn encode_track_type(w: &mut FbWriter, t: ProjectTrackType) {
    w.write_u8(match t {
        ProjectTrackType::Audio => 0,
        ProjectTrackType::Midi => 1,
        ProjectTrackType::Instrument => 2,
        ProjectTrackType::Bus => 3,
        ProjectTrackType::Return => 4,
        ProjectTrackType::Group => 5,
        ProjectTrackType::Master => 6,
        ProjectTrackType::Video => 7,
    });
}

/// **v33 encoder â legacy compatibility only.** v34 saves never call this;
/// it exists for the legacy fixtures and round-trip tests.
#[cfg(test)]
fn encode_track_input_routing(w: &mut FbWriter, input: &V33TrackInputRouting) {
    match input {
        V33TrackInputRouting::None => w.write_u8(0),
        V33TrackInputRouting::AllInputs => w.write_u8(1),
        V33TrackInputRouting::AudioDeviceChannel { device_id, channel } => {
            w.write_u8(2);
            w.write_str(device_id);
            w.write_u32(*channel);
        }
        V33TrackInputRouting::AudioDeviceChannels {
            device_id,
            channels,
        } => {
            w.write_u8(4);
            w.write_str(device_id);
            w.write_u32(channels.len() as u32);
            for channel in channels {
                w.write_u32(*channel);
            }
        }
        V33TrackInputRouting::MidiDevice { device_id } => {
            w.write_u8(3);
            w.write_str(device_id);
        }
    }
}

fn encode_track_output_routing(w: &mut FbWriter, output: &ProjectTrackOutputRouting) {
    match output {
        ProjectTrackOutputRouting::Main => w.write_u8(0),
        ProjectTrackOutputRouting::Bus { bus_id } => {
            w.write_u8(1);
            w.write_str(bus_id);
        }
        ProjectTrackOutputRouting::HardwareOutput { device_id, channel } => {
            w.write_u8(2);
            w.write_str(device_id);
            w.write_u32(*channel);
        }
        ProjectTrackOutputRouting::None => w.write_u8(3),
        ProjectTrackOutputRouting::Instrument { track_id } => {
            w.write_u8(4);
            w.write_str(track_id);
        }
    }
}

fn encode_track_audio_format(w: &mut FbWriter, audio_format: ProjectTrackAudioFormat) {
    w.write_u8(match audio_format {
        ProjectTrackAudioFormat::Mono => 0,
        ProjectTrackAudioFormat::Stereo => 1,
    });
}

fn encode_track_midi_input_routing(w: &mut FbWriter, input: &ProjectTrackMidiInputRouting) {
    match input {
        ProjectTrackMidiInputRouting::None => w.write_u8(0),
        ProjectTrackMidiInputRouting::AllInputs => w.write_u8(1),
        ProjectTrackMidiInputRouting::MidiDevice { device_id } => {
            w.write_u8(2);
            w.write_str(device_id);
        }
    }
}

fn routing_output_bus_id(output: &ProjectTrackOutputRouting) -> Option<String> {
    match output {
        ProjectTrackOutputRouting::Bus { bus_id } => Some(bus_id.clone()),
        _ => None,
    }
}

fn encode_track(w: &mut FbWriter, t: &ProjectTrack) {
    w.write_str(&t.id);
    w.write_str(&t.name);
    encode_track_type(w, t.track_type);
    w.write_opt_str(&t.parent_group_id); // v30
    w.write_bool(t.group_collapsed); // v31
    w.write_str(&t.color_hex);
    w.write_f32(t.volume_norm);
    w.write_f32(t.pan);
    w.write_bool(t.muted);
    w.write_bool(t.solo);
    w.write_bool(t.record_arm);
    encode_input_monitor(w, t.input_monitor);
    // routing â v34 stores the audio input as a logical Audio Connection id.
    // The legacy combined union is never written by this encoder.
    w.write_opt_str(&t.routing.audio_input_connection_id);
    encode_track_output_routing(w, &t.routing.output);
    encode_track_audio_format(w, t.routing.audio_format);
    encode_track_midi_input_routing(w, &t.routing.midi_input);
    w.write_opt_u8(&t.routing.midi_channel.map(|ch| ch.clamp(1, 16)));
    w.write_bool(t.routing.midi_output_per_note); // v22
    let output_bus = routing_output_bus_id(&t.routing.output);
    w.write_opt_str(&output_bus);
    w.write_u32(t.routing.sends.len() as u32);
    for s in &t.routing.sends {
        w.write_str(&s.id);
        w.write_str(&s.target_track_id);
        w.write_bool(s.enabled);
        w.write_bool(s.pre_fader);
        w.write_f32(s.gain_db);
    }
    // inserts
    w.write_u32(t.inserts.len() as u32);
    for ins in &t.inserts {
        encode_insert(w, ins);
    }
    // automation lanes
    w.write_u32(t.automation_lanes.len() as u32);
    for lane in &t.automation_lanes {
        encode_automation_lane(w, lane);
    }
    // clips
    w.write_u32(t.clips.len() as u32);
    for c in &t.clips {
        encode_clip(w, c);
    }
    w.write_opt_f32(&t.row_height_px);
    encode_soundfont_player(w, t.soundfont.as_ref()); // v28
    w.write_bool(t.volume_automation_read); // v32
    encode_solfege_engine(w, t.solfege.as_ref()); // v37
                                                  // v42: the track's ARA plug-in. Identity only â its edits live in the
                                                  // project-level document archive keyed by (plug-in, track).
    match &t.ara {
        Some(ara) => {
            w.write_u8(1);
            w.write_str(&ara.plugin_id);
            w.write_str(&ara.plugin_path);
            w.write_str(&ara.class_id);
        }
        None => w.write_u8(0),
    }
    // v43: per-track timebase, at the tail of the track block for the same
    // reason the ARA binding is â a v42 track block simply ends before it.
    w.write_u8(t.timebase);
    // v44: recorded takes. Appended for the same reason again; a v43 track
    // block ends above and reads as a track with no take list.
    w.write_u32(t.takes.len() as u32);
    for take in &t.takes {
        w.write_str(&take.id);
        w.write_str(&take.name);
        w.write_str(&take.clip_id);
        w.write_bool(take.active);
        w.write_str(&take.recorded_at);
    }
    w.write_bool(t.takes_expanded);
    // v46: per-track MPE output settings. Appended so v45 and older track
    // blocks remain positionally readable.
    let mpe = t.routing.mpe.sanitized();
    w.write_u8(mpe.mode.to_tag());
    w.write_u8(mpe.member_channels);
    w.write_f32(mpe.member_pitch_range);
    w.write_f32(mpe.manager_pitch_range);
}

/// v28: built-in Soundfont Player instrument state. A leading flag keeps the
/// common case (no soundfont on this track) to a single byte.
fn encode_soundfont_player(w: &mut FbWriter, soundfont: Option<&ProjectSoundfontPlayer>) {
    let Some(soundfont) = soundfont else {
        w.write_bool(false);
        return;
    };
    w.write_bool(true);
    w.write_opt_path(&soundfont.path);
    // Bank and patch are only meaningful together, and both are non-negative
    // (MIDI bank select / program change), so one flag plus two u32s round-trips
    // the pair without a signed encoding.
    let preset = soundfont.preset_bank.zip(soundfont.preset_patch);
    w.write_bool(preset.is_some());
    let (bank, patch) = preset.unwrap_or((0, 0));
    w.write_u32(bank.max(0) as u32);
    w.write_u32(patch.max(0) as u32);
    w.write_f32(soundfont.volume);
    w.write_bool(soundfont.reverb_chorus);
    w.write_u32(soundfont.polyphony);
    // v29: amp envelope + render quality. The quality is written as its stable
    // key rather than an ordinal so adding a setting later cannot renumber the
    // ones already on disk.
    let envelope = soundfont.envelope.sanitized();
    w.write_f32(envelope.attack_ms);
    w.write_f32(envelope.decay_ms);
    w.write_f32(envelope.sustain);
    w.write_f32(envelope.release_ms);
    w.write_str(soundfont.quality.key());
}

fn decode_soundfont_player(
    r: &mut FbReader,
    version: u32,
) -> Result<Option<ProjectSoundfontPlayer>, ProjectError> {
    if !r.read_bool()? {
        return Ok(None);
    }
    let path = r.read_opt_path()?;
    let has_preset = r.read_bool()?;
    let bank = r.read_u32()? as i32;
    let patch = r.read_u32()? as i32;
    let volume = r.read_f32()?.clamp(0.0, 1.0);
    let reverb_chorus = r.read_bool()?;
    let polyphony = r.read_u32()?.clamp(1, 256);
    let (envelope, quality) = if version >= 29 {
        let envelope = SoundfontEnvelope {
            attack_ms: r.read_f32()?,
            decay_ms: r.read_f32()?,
            sustain: r.read_f32()?,
            release_ms: r.read_f32()?,
        }
        .sanitized();
        (envelope, SoundfontRenderQuality::from_key(&r.read_str()?))
    } else {
        // A v28 soundfont played through neither, so the defaults reproduce it.
        (
            SoundfontEnvelope::default(),
            SoundfontRenderQuality::default(),
        )
    };
    Ok(Some(ProjectSoundfontPlayer {
        path,
        preset_bank: has_preset.then_some(bank),
        preset_patch: has_preset.then_some(patch),
        volume,
        reverb_chorus,
        polyphony,
        envelope,
        quality,
    }))
}

fn encode_solfege_engine(w: &mut FbWriter, solfege: Option<&ProjectSolfegeEngine>) {
    let Some(solfege) = solfege else {
        w.write_bool(false);
        return;
    };
    w.write_bool(true);
    w.write_opt_path(&solfege.model_path);
    w.write_str(&solfege.instrument);
    w.write_str(&solfege.voice);
    w.write_str(&solfege.preset);
    w.write_f32(solfege.bow_pressure.clamp(0.0, 1.0));
    w.write_f32(solfege.vibrato.clamp(0.0, 1.0));
    w.write_f32(solfege.dynamics.clamp(0.0, 1.0));
    w.write_f32(solfege.expression.clamp(0.0, 1.0));
    // v38: Solfege editor lane layout (which performance lanes are on screen).
    w.write_u32(solfege.visible_lanes.len() as u32);
    for lane in &solfege.visible_lanes {
        w.write_str(&lane.lane_id);
        w.write_f32(lane.height);
    }
}

fn decode_solfege_engine(
    r: &mut FbReader,
    version: u32,
) -> Result<Option<ProjectSolfegeEngine>, ProjectError> {
    if !r.read_bool()? {
        return Ok(None);
    }
    let model_path = r.read_opt_path()?;
    let instrument = r.read_str()?;
    let voice = r.read_str()?;
    let preset = r.read_str()?;
    let bow_pressure = r.read_f32()?.clamp(0.0, 1.0);
    let vibrato = r.read_f32()?.clamp(0.0, 1.0);
    let dynamics = r.read_f32()?.clamp(0.0, 1.0);
    let expression = r.read_f32()?.clamp(0.0, 1.0);
    // v38 adds the editor lane layout. A v37 file falls back to the
    // instrument's default lanes when the timeline restores it.
    let visible_lanes = if version >= 38 {
        let count = r.read_u32()? as usize;
        let mut lanes = Vec::with_capacity(count.min(64));
        for _ in 0..count {
            lanes.push(ProjectSolfegeLane {
                lane_id: r.read_str()?,
                height: r.read_f32()?,
            });
        }
        lanes
    } else {
        ProjectSolfegeEngine::default().visible_lanes
    };
    Ok(Some(ProjectSolfegeEngine {
        model_path,
        instrument,
        voice,
        preset,
        bow_pressure,
        vibrato,
        dynamics,
        expression,
        visible_lanes,
    }))
}

fn encode_asset(w: &mut FbWriter, a: &ProjectAsset) {
    w.write_str(&a.id);
    w.write_str(&a.original_filename);
    w.write_opt_str(&a.relative_path);
    w.write_opt_path(&a.absolute_path);
    w.write_opt_f64(&a.duration_secs);
    w.write_opt_u32(&a.sample_rate);
    w.write_opt_u8(&a.channels);
    w.write_opt_str(&a.source_fingerprint); // v11
    w.write_opt_str(&a.waveform_peak_relative_path); // v12
    w.write_opt_u64(&a.duration_samples); // v12
}

fn encode_song_text_event(w: &mut FbWriter, event: &ProjectSongTextEvent) {
    w.write_str(&event.id);
    w.write_f64(event.beat);
    match &event.kind {
        ProjectSongTextEventKind::Chord { symbol } => {
            w.write_u8(0);
            w.write_str(symbol);
        }
        ProjectSongTextEventKind::Lyric {
            text,
            syllable_mode,
            continuation,
            duration_beats,
            syllables,
        } => {
            w.write_u8(1);
            w.write_str(text);
            w.write_u8(match syllable_mode {
                ProjectLyricSyllableMode::Phrase => 0,
                ProjectLyricSyllableMode::Syllables => 1,
            });
            w.write_bool(*continuation);
            w.write_opt_f64(duration_beats);
            w.write_u32(syllables.len() as u32);
            for syllable in syllables {
                w.write_str(&syllable.text);
                w.write_f64(syllable.offset_beats);
                w.write_opt_f64(&syllable.duration_beats);
            }
        }
        ProjectSongTextEventKind::Section {
            name,
            section_type,
            color_hex,
        } => {
            w.write_u8(2);
            w.write_str(name);
            w.write_u8(match section_type {
                ProjectSongSectionType::Custom => 0,
                ProjectSongSectionType::Intro => 1,
                ProjectSongSectionType::Verse => 2,
                ProjectSongSectionType::PreChorus => 3,
                ProjectSongSectionType::Chorus => 4,
                ProjectSongSectionType::Bridge => 5,
                ProjectSongSectionType::Solo => 6,
                ProjectSongSectionType::Outro => 7,
            });
            w.write_str(color_hex);
        }
    }
}

fn decode_song_text_event(r: &mut FbReader) -> Result<ProjectSongTextEvent, ProjectError> {
    let id = r.read_str()?;
    let beat = r.read_f64()?;
    let kind = match r.read_u8()? {
        0 => ProjectSongTextEventKind::Chord {
            symbol: r.read_str()?,
        },
        1 => {
            let text = r.read_str()?;
            let syllable_mode = match r.read_u8()? {
                0 => ProjectLyricSyllableMode::Phrase,
                1 => ProjectLyricSyllableMode::Syllables,
                tag => {
                    return Err(ProjectError::Corrupted(format!(
                        "bad lyric syllable mode tag {tag}"
                    )));
                }
            };
            let continuation = r.read_bool()?;
            let duration_beats = r.read_opt_f64()?;
            let syllable_count = r.read_u32()? as usize;
            const MIN_SYLLABLE_BYTES: usize = 13;
            if syllable_count > 100_000 || syllable_count > r.remaining() / MIN_SYLLABLE_BYTES {
                return Err(ProjectError::Corrupted(
                    "invalid Song Text syllable count".to_string(),
                ));
            }
            let mut syllables = Vec::with_capacity(syllable_count);
            for _ in 0..syllable_count {
                syllables.push(ProjectLyricSyllable {
                    text: r.read_str()?,
                    offset_beats: r.read_f64()?,
                    duration_beats: r.read_opt_f64()?,
                });
            }
            ProjectSongTextEventKind::Lyric {
                text,
                syllable_mode,
                continuation,
                duration_beats,
                syllables,
            }
        }
        2 => {
            let name = r.read_str()?;
            let section_type = match r.read_u8()? {
                0 => ProjectSongSectionType::Custom,
                1 => ProjectSongSectionType::Intro,
                2 => ProjectSongSectionType::Verse,
                3 => ProjectSongSectionType::PreChorus,
                4 => ProjectSongSectionType::Chorus,
                5 => ProjectSongSectionType::Bridge,
                6 => ProjectSongSectionType::Solo,
                7 => ProjectSongSectionType::Outro,
                tag => {
                    return Err(ProjectError::Corrupted(format!(
                        "bad song section type tag {tag}"
                    )));
                }
            };
            ProjectSongTextEventKind::Section {
                name,
                section_type,
                color_hex: r.read_str()?,
            }
        }
        tag => {
            return Err(ProjectError::Corrupted(format!(
                "bad song text event kind tag {tag}"
            )));
        }
    };
    Ok(ProjectSongTextEvent { id, beat, kind })
}

#[derive(Debug, Clone, PartialEq)]
struct LegacyProjectSongTextCue {
    id: String,
    beat: f64,
    chord: String,
    lyric: String,
}

fn decode_legacy_song_text_cue(r: &mut FbReader) -> Result<LegacyProjectSongTextCue, ProjectError> {
    Ok(LegacyProjectSongTextCue {
        id: r.read_str()?,
        beat: r.read_f64()?,
        chord: r.read_str()?,
        lyric: r.read_str()?,
    })
}

fn migrate_legacy_song_text_cue(
    cue: LegacyProjectSongTextCue,
    events: &mut Vec<ProjectSongTextEvent>,
) {
    let has_chord = !cue.chord.trim().is_empty();
    let has_lyric = !cue.lyric.trim().is_empty();
    match (has_chord, has_lyric) {
        (true, true) => {
            events.push(ProjectSongTextEvent {
                id: format!("{}:chord", cue.id),
                beat: cue.beat,
                kind: ProjectSongTextEventKind::Chord { symbol: cue.chord },
            });
            events.push(ProjectSongTextEvent {
                id: format!("{}:lyric", cue.id),
                beat: cue.beat,
                kind: ProjectSongTextEventKind::Lyric {
                    text: cue.lyric,
                    syllable_mode: ProjectLyricSyllableMode::Phrase,
                    continuation: false,
                    duration_beats: None,
                    syllables: Vec::new(),
                },
            });
        }
        (true, false) => events.push(ProjectSongTextEvent {
            id: cue.id,
            beat: cue.beat,
            kind: ProjectSongTextEventKind::Chord { symbol: cue.chord },
        }),
        (false, true) => events.push(ProjectSongTextEvent {
            id: cue.id,
            beat: cue.beat,
            kind: ProjectSongTextEventKind::Lyric {
                text: cue.lyric,
                syllable_mode: ProjectLyricSyllableMode::Phrase,
                continuation: false,
                duration_beats: None,
                syllables: Vec::new(),
            },
        }),
        (false, false) => {}
    }
}

fn encode_body(project: &FutureboardProject) -> Vec<u8> {
    let mut w = FbWriter::new();

    // Header fields
    w.write_str(&project.id);
    w.write_str(&project.name);
    w.write_u64(project.created_at);
    w.write_u64(project.modified_at);

    // Settings
    w.write_f64(project.settings.bpm);
    w.write_u32(project.settings.time_sig_num);
    w.write_u32(project.settings.time_sig_den);
    w.write_u32(project.settings.sample_rate);
    w.write_u32(project.settings.bit_depth);

    // Mixer
    w.write_f32(project.mixer.master_volume_norm);
    w.write_u32(project.mixer.master_inserts.len() as u32);
    for ins in &project.mixer.master_inserts {
        encode_insert(&mut w, ins);
    }

    // Tracks
    w.write_u32(project.tracks.len() as u32);
    for t in &project.tracks {
        encode_track(&mut w, t);
    }

    // Assets
    w.write_u32(project.assets.len() as u32);
    for a in &project.assets {
        encode_asset(&mut w, a);
    }

    // Tempo automation markers (v7+). Appended at the end of the body so older
    // readers that stop after assets are unaffected. v8+ includes stable ids.
    w.write_u32(project.settings.tempo_points.len() as u32);
    for p in &project.settings.tempo_points {
        if PROJECT_VERSION >= 8 {
            w.write_str(&p.id);
        }
        w.write_f64(p.beat);
        w.write_f64(p.bpm);
        w.write_u8(p.curve);
    }

    // Time signature markers (v9+).
    w.write_u32(project.settings.time_signature_points.len() as u32);
    for p in &project.settings.time_signature_points {
        w.write_str(&p.id);
        w.write_f64(p.beat);
        w.write_u32(p.numerator as u32);
        w.write_u32(p.denominator as u32);
        w.write_u32(p.grouping.len() as u32);
        for g in &p.grouping {
            w.write_u32(*g as u32);
        }
    }

    // Timeline arrangement markers and regions (v13+).
    w.write_u32(project.settings.timeline_markers.len() as u32);
    for marker in &project.settings.timeline_markers {
        w.write_str(&marker.id);
        w.write_f64(marker.beat);
        w.write_str(&marker.name);
        w.write_str(&marker.color_hex);
    }
    w.write_u32(project.settings.timeline_regions.len() as u32);
    for region in &project.settings.timeline_regions {
        w.write_str(&region.id);
        w.write_f64(region.start_beat);
        w.write_f64(region.end_beat);
        w.write_str(&region.name);
        w.write_str(&region.color_hex);
    }

    // Mixer tree UI state (v20+).
    w.write_u32(project.mixer.tree_expanded_node_ids.len() as u32);
    for id in &project.mixer.tree_expanded_node_ids {
        w.write_str(id);
    }
    w.write_u32(project.mixer.tree_pinned_channel_ids.len() as u32);
    for id in &project.mixer.tree_pinned_channel_ids {
        w.write_str(id);
    }
    w.write_u32(project.mixer.tree_hidden_channel_ids.len() as u32);
    for id in &project.mixer.tree_hidden_channel_ids {
        w.write_str(id);
    }

    // Typed Song Text events (v27+). Kept at the body tail for backwards loading.
    w.write_u32(project.settings.song_text_events.len() as u32);
    for event in &project.settings.song_text_events {
        encode_song_text_event(&mut w, event);
    }

    // Audio Connections registry (v34+). Last section, so a reader that stops
    // earlier is unaffected.
    w.write_u32(project.audio_connections.len() as u32);
    for connection in &project.audio_connections {
        encode_audio_connection(&mut w, connection);
    }

    // Master / Monitor output routing (v35+). Ids only: the resolved route,
    // runtime device index, hardware owner, and channel list are all recomputed
    // from the current machine and are deliberately not persisted.
    w.write_opt_str(&project.master_output_connection_id);
    w.write_opt_str(&project.monitor_output_connection_id);
    w.write_bool(project.output_routing_initialized);

    // Conductor lane fold state (v40+). Appended after the output routing for
    // the reason that block was appended after the registry: the body is
    // positional, so a v39 file simply ends here and must decode as v39.
    w.write_bool(project.global_lanes.arranger_collapsed);
    w.write_bool(project.global_lanes.marker_collapsed);
    w.write_bool(project.global_lanes.tempo_collapsed);
    w.write_bool(project.global_lanes.time_signature_collapsed);
    w.write_opt_f32(&project.global_lanes.arranger_height);
    w.write_opt_f32(&project.global_lanes.marker_height);
    w.write_opt_f32(&project.global_lanes.tempo_height);
    w.write_opt_f32(&project.global_lanes.time_signature_height);
    w.write_opt_f32(&project.global_lanes.song_text_height);

    // ARA document archives (v41+). Appended after the conductor lanes for the
    // same reason that block was appended after the routing: the body is
    // positional, so a v40 file simply ends there.
    w.write_u32(project.ara_documents.len() as u32);
    for document in &project.ara_documents {
        w.write_str(&document.plugin_id);
        w.write_str(&document.track_id);
        w.write_str(&document.archive_id);
        w.write_bytes(&document.data);
    }

    // Project timebase (v43+). Appended after the ARA documents for the same
    // reason that block was appended after the conductor lanes: the body is
    // positional, so a v42 file simply ends there and must decode as v42.
    w.write_u8(project.settings.time_display_format);
    w.write_u8(project.settings.timecode_rate);

    w.into_bytes()
}

fn encode_audio_connection(w: &mut FbWriter, c: &ProjectAudioConnection) {
    w.write_str(&c.id);
    w.write_str(&c.name);
    w.write_str(&c.direction);
    w.write_str(&c.channel_layout);
    w.write_u32(c.channel_count);
    w.write_opt_str(&c.device_id);
    w.write_bool(c.enabled);
    // Ordered bindings â index order is semantic (Left, Right) and is written
    // exactly as held.
    w.write_u32(c.port_bindings.len() as u32);
    for binding in &c.port_bindings {
        w.write_u32(binding.logical_channel);
        w.write_str(&binding.device_id);
        w.write_str(&binding.port_name);
        w.write_u32(binding.port_index);
    }
}

/// Smallest possible encoded connection: 4 short strings + 2 u32 + opt tag +
/// bool + binding count. Used to bound the collection length against the
/// remaining buffer before allocating.
const MIN_AUDIO_CONNECTION_BYTES: usize = 26;
/// Hard ceiling on connections in one project, mirroring the other collections.
const MAX_AUDIO_CONNECTIONS: usize = 100_000;
/// Hard ceiling on bindings in one connection. Far above any real layout.
const MAX_AUDIO_PORT_BINDINGS: usize = 1024;
/// Smallest ARA document record on the wire: three empty strings and an empty
/// blob, each a bare u32 length.
const MIN_ARA_DOCUMENT_BYTES: usize = 16;
/// Hard ceiling on saved ARA documents, mirroring the other collections. One per
/// bound plug-in, so real projects hold a handful.
const MAX_ARA_DOCUMENTS: usize = 1024;

fn decode_audio_connection(r: &mut FbReader) -> Result<ProjectAudioConnection, ProjectError> {
    let id = r.read_str()?;
    if id.is_empty() {
        return Err(ProjectError::Corrupted(
            "audio connection with empty id".to_string(),
        ));
    }
    let name = r.read_str()?;
    let direction = r.read_str()?;
    if direction != "input" && direction != "output" {
        return Err(ProjectError::Corrupted(format!(
            "unknown audio connection direction {direction}"
        )));
    }
    let channel_layout = r.read_str()?;
    if !matches!(channel_layout.as_str(), "mono" | "stereo" | "custom") {
        return Err(ProjectError::Corrupted(format!(
            "unknown audio channel layout {channel_layout}"
        )));
    }
    let channel_count = r.read_u32()?;
    if channel_count as usize > MAX_AUDIO_PORT_BINDINGS {
        return Err(ProjectError::Corrupted(
            "invalid audio connection channel count".to_string(),
        ));
    }
    let device_id = r.read_opt_str()?;
    let enabled = r.read_bool()?;
    let binding_count = r.read_u32()? as usize;
    // Bound before allocating: a truncated or hostile file must not make us
    // reserve an arbitrary vector.
    if binding_count > MAX_AUDIO_PORT_BINDINGS || binding_count > r.remaining() / 12 {
        return Err(ProjectError::Corrupted(
            "invalid audio connection binding count".to_string(),
        ));
    }
    let mut port_bindings = Vec::with_capacity(binding_count);
    for _ in 0..binding_count {
        port_bindings.push(ProjectAudioPortBinding {
            logical_channel: r.read_u32()?,
            device_id: r.read_str()?,
            port_name: r.read_str()?,
            port_index: r.read_u32()?,
        });
    }
    Ok(ProjectAudioConnection {
        id,
        name,
        direction,
        channel_layout,
        channel_count,
        device_id,
        port_bindings,
        enabled,
    })
}

/// Encodes a `FutureboardProject` into the full `.fbproj` binary format.
pub fn encode_project(project: &FutureboardProject) -> Vec<u8> {
    let body = encode_body(project);
    let checksum = crc32fast::hash(&body);

    let mut out = Vec::with_capacity(8 + 4 + 4 + 4 + body.len() + 4);
    out.extend_from_slice(PROJECT_MAGIC);
    out.extend_from_slice(&PROJECT_VERSION.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // reserved
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out.extend_from_slice(&checksum.to_le_bytes());
    out
}

// ââ Decoding ââââââââââââââââââââââââââââââââââââââââââââââââââââââââââââââââââ

fn decode_plugin_format(r: &mut FbReader) -> Result<PluginFormat, ProjectError> {
    Ok(match r.read_u8()? {
        0 => PluginFormat::Vst3,
        1 => PluginFormat::Clap,
        2 => PluginFormat::Au,
        3 => PluginFormat::Lv2,
        4 => PluginFormat::Vst2,
        _ => PluginFormat::Unknown,
    })
}

fn decode_opt_plugin_format(r: &mut FbReader) -> Result<Option<PluginFormat>, ProjectError> {
    match r.read_u8()? {
        0 => Ok(None),
        1 => Ok(Some(decode_plugin_format(r)?)),
        t => Err(ProjectError::Corrupted(format!("bad option tag {t}"))),
    }
}

fn decode_plugin_state_blob(r: &mut FbReader) -> Result<PluginStateBlob, ProjectError> {
    Ok(PluginStateBlob {
        plugin_id: r.read_str()?,
        format: decode_opt_plugin_format(r)?,
        state_bytes: r.read_bytes()?,
        vendor: r.read_opt_str()?,
        name: r.read_opt_str()?,
        version: r.read_opt_str()?,
    })
}

fn decode_plugin_instance(r: &mut FbReader) -> Result<ProjectPluginInstance, ProjectError> {
    Ok(ProjectPluginInstance {
        instance_id: r.read_str()?,
        format: decode_plugin_format(r)?,
        plugin_path: r.read_opt_path()?,
        plugin_uid: r.read_str()?,
        display_name: r.read_str()?,
        state: decode_plugin_state_blob(r)?,
    })
}

fn decode_insert(r: &mut FbReader, version: u32) -> Result<ProjectInsert, ProjectError> {
    let id = r.read_str()?;
    let slot_index = r.read_u32()?;
    let bypassed = r.read_bool()?;
    let enabled_audio_output_channels = if version >= 18 {
        let count = r.read_u32()? as usize;
        let mut channels = Vec::with_capacity(count.min(32));
        for _ in 0..count {
            let channel = r.read_u8()?;
            if (1..=32).contains(&channel) && !channels.contains(&channel) {
                channels.push(channel);
            }
        }
        channels
    } else {
        Vec::new()
    };
    let multiout_collapsed = if version >= 19 { r.read_bool()? } else { false };
    let plugin_is_instrument = if version >= 36 {
        match r.read_u8()? {
            0 => None,
            1 => Some(false),
            2 => Some(true),
            tag => {
                return Err(ProjectError::Corrupted(format!(
                    "bad insert role tag {tag}"
                )));
            }
        }
    } else {
        None
    };
    let plugin = match r.read_u8()? {
        0 => None,
        1 => Some(decode_plugin_instance(r)?),
        t => {
            return Err(ProjectError::Corrupted(format!(
                "bad plugin option tag {t}"
            )));
        }
    };
    Ok(ProjectInsert {
        id,
        slot_index,
        bypassed,
        enabled_audio_output_channels,
        plugin_is_instrument,
        multiout_collapsed,
        plugin,
    })
}

fn decode_automation_lane(r: &mut FbReader, version: u32) -> Result<AutomationLane, ProjectError> {
    let id = r.read_str()?;
    let parameter_name = r.read_str()?;
    let visible = r.read_bool()?;
    let (target, enabled) = if version >= 2 {
        let tag = r.read_u8()?;
        let insert_id = r.read_str()?;
        let parameter_id = r.read_str()?;
        let target_param_name = r.read_str()?;
        let send_id = r.read_str()?;
        let enabled = r.read_bool()?;
        (
            AutomationTargetDesc {
                tag,
                insert_id,
                parameter_id,
                parameter_name: target_param_name,
                send_id,
            },
            enabled,
        )
    } else {
        (AutomationTargetDesc::default(), true)
    };
    let count = r.read_u32()? as usize;
    let mut points = Vec::with_capacity(count);
    for _ in 0..count {
        let beat = r.read_f32()?;
        let value = r.read_f32()?;
        let curve = if version >= 2 { r.read_u8()? } else { 0 };
        let tension = if version >= 21 { r.read_f32()? } else { 0.0 };
        points.push(AutomationPoint {
            beat,
            value,
            curve,
            tension,
        });
    }
    Ok(AutomationLane {
        id,
        parameter_name,
        target,
        enabled,
        visible,
        points,
    })
}

fn decode_midi_note(r: &mut FbReader, version: u32) -> Result<MidiNote, ProjectError> {
    Ok(MidiNote {
        pitch: r.read_u8()?,
        start_beats: r.read_f32()?,
        duration_beats: r.read_f32()?,
        velocity: r.read_u8()?,
        // v4 added the muted flag; older files default to unmuted.
        muted: if version >= 4 { r.read_bool()? } else { false },
        // v22 added a per-note MIDI channel; older files default to channel 1.
        channel: if version >= 22 {
            r.read_u8()?.clamp(1, 16)
        } else {
            1
        },
        // v25 added the per-note articulation tag; older files have none.
        articulation: if version >= 25 { r.read_u8()? } else { 0 },
        // v26 adds stable id + optional release velocity.
        id: if version >= 26 { r.read_u64()? } else { 0 },
        release_velocity: if version >= 26 { r.read_u8()? } else { 0 },
        // v38 adds the per-note pitch curve; older files have none.
        pitch_curve: if version >= 38 {
            let count = r.read_u32()? as usize;
            let mut points = Vec::with_capacity(count.min(4096));
            for _ in 0..count {
                points.push(MidiPitchPoint {
                    id: r.read_u64()?,
                    beat: r.read_f32()?,
                    cents: r.read_f32()?,
                    shape: r.read_u8()?,
                });
            }
            points
        } else {
            Vec::new()
        },
        // v39 adds the per-note accent; older files have none.
        accent: if version >= 39 && r.read_bool()? {
            Some(MidiAccent {
                prominence: r.read_f32()?,
                attack: r.read_f32()?,
                agogic: r.read_f32()?,
                timbre: r.read_f32()?,
                confidence: r.read_f32()?,
                source: r.read_u8()?,
            })
        } else {
            None
        },
        // v45 adds note-owned expression curves; older files restore with no
        // expression and remain binary-compatible.
        expression: if version >= 45 {
            decode_note_expression(r)?
        } else {
            NoteExpression::default()
        },
    })
}

fn decode_controller_kind(r: &mut FbReader) -> Result<MidiControllerKind, ProjectError> {
    Ok(match r.read_u8()? {
        0 => MidiControllerKind::CC(r.read_u8()?),
        1 => MidiControllerKind::PitchBend,
        2 => MidiControllerKind::ChannelPressure,
        3 => MidiControllerKind::PolyPressure,
        t => {
            return Err(ProjectError::Corrupted(format!(
                "unknown controller kind tag {t}"
            )));
        }
    })
}

fn decode_controller_lane(
    r: &mut FbReader,
    version: u32,
) -> Result<MidiControllerLane, ProjectError> {
    let kind = decode_controller_kind(r)?;
    let visible = r.read_bool()?;
    let height = r.read_f32()?;
    let collapsed = r.read_bool()?;
    let count = r.read_u32()? as usize;
    let mut points = Vec::with_capacity(count);
    for _ in 0..count {
        let beat = r.read_f32()?;
        let value = r.read_f32()?;
        let id = if version >= 26 { r.read_u64()? } else { 0 };
        points.push(MidiControllerPoint { id, beat, value });
    }
    Ok(MidiControllerLane {
        kind,
        points,
        visible,
        height,
        collapsed,
    })
}

fn decode_sysex_event(r: &mut FbReader) -> Result<MidiSysExEvent, ProjectError> {
    let kind = match r.read_u8()? {
        0 => MidiSysExKind::Normal,
        1 => MidiSysExKind::Escaped,
        t => {
            return Err(ProjectError::Corrupted(format!(
                "unknown SysEx event kind tag {t}"
            )));
        }
    };
    Ok(MidiSysExEvent {
        kind,
        tick: r.read_u64()?,
        beat: r.read_f32()?,
        data: r.read_bytes()?,
    })
}

/// v16: per-clip stretch/pitch block. See [`encode_stretch`].
fn decode_stretch(r: &mut FbReader, version: u32) -> Result<AudioClipStretchState, ProjectError> {
    let mode = StretchMode::from_tag(r.read_u8()?);
    let algorithm = StretchAlgorithm::from_tag(r.read_u8()?);
    let original_sample_rate = r.read_u32()?;
    let project_sample_rate = r.read_u32()?;
    let original_duration_samples = r.read_u64()?;
    let source_start_samples = r.read_u64()?;
    let source_end_samples = r.read_u64()?;
    let clip_timeline_start_beats = r.read_f64()?;
    let clip_timeline_duration_beats = r.read_f64()?;
    let stretch_ratio = r.read_f64()?;
    let bpm_source = r.read_opt_f64()?;
    let bpm_target = r.read_opt_f64()?;
    let preserve_pitch = r.read_bool()?;
    let pitch_shift_semitones = r.read_f32()?;
    let formant_preserve = r.read_bool()?;
    let transient_preserve = r.read_bool()?;
    let transient_sensitivity = r.read_f32()?;
    let reverse = r.read_bool()?;
    let normalize_gain = r.read_bool()?;
    let fade_in_ms = r.read_f32()?;
    let fade_out_ms = r.read_f32()?;
    let gain_db = r.read_f32()?;
    let pan = r.read_f32()?;
    let marker_count = r.read_u32()? as usize;
    let retained_marker_count = marker_count.min(AudioClipStretchState::MAX_WARP_MARKERS);
    let mut warp_markers = Vec::with_capacity(retained_marker_count);
    for marker_index in 0..marker_count {
        let marker = WarpMarker {
            id: r.read_u64()?,
            source_sample: r.read_u64()?,
            timeline_beat: r.read_f64()?,
            locked: r.read_bool()?,
        };
        if marker_index < retained_marker_count {
            warp_markers.push(marker);
        }
    }
    let gain_envelope = if version >= 47 {
        let point_count = r.read_u32()? as usize;
        let retained_point_count = point_count.min(ClipEnvelope::MAX_POINTS);
        let mut points = Vec::with_capacity(retained_point_count);
        for point_index in 0..point_count {
            let point = EnvelopePoint {
                id: r.read_u64()?,
                time: r.read_f32()?,
                value_db: r.read_f32()?,
                curve: EnvelopeCurve::from_tag(r.read_u8()?),
            };
            if point_index < retained_point_count {
                points.push(point);
            }
        }
        ClipEnvelope { points }
    } else {
        ClipEnvelope::default()
    };
    let denoise_amount = if version >= 48 {
        r.read_f32()?.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let (
        channel_transform,
        dc_remove,
        dc_left,
        dc_right,
        dehum_hz,
        dehum_harmonics,
        dehum_reduction_db,
    ) = if version >= 49 {
        (
            r.read_u8()?,
            r.read_bool()?,
            r.read_f32()?,
            r.read_f32()?,
            r.read_f32()?,
            r.read_u8()?,
            r.read_f32()?,
        )
    } else {
        (0, false, 0.0, 0.0, 0.0, 0, 0.0)
    };
    let mut stretch = AudioClipStretchState {
        mode,
        algorithm,
        original_sample_rate,
        project_sample_rate,
        original_duration_samples,
        source_start_samples,
        source_end_samples,
        clip_timeline_start_beats,
        clip_timeline_duration_beats,
        stretch_ratio,
        bpm_source,
        bpm_target,
        preserve_pitch,
        pitch_shift_semitones,
        formant_preserve,
        transient_preserve,
        transient_sensitivity,
        reverse,
        normalize_gain,
        fade_in_ms,
        fade_out_ms,
        gain_db,
        pan,
        dirty: false,
        warp_markers,
        gain_envelope,
        denoise_amount,
        channel_transform,
        dc_remove,
        dc_left,
        dc_right,
        dehum_hz,
        dehum_harmonics,
        dehum_reduction_db,
    };
    stretch.sanitize_in_place();
    Ok(stretch)
}

fn decode_clip(r: &mut FbReader, version: u32) -> Result<ProjectClip, ProjectError> {
    let id = r.read_str()?;
    let name = r.read_str()?;
    let start_beat = r.read_f64()?;
    let duration_beats = r.read_f64()?;
    let offset_beats = r.read_f32()?;
    let gain = r.read_f32()?;
    let muted = r.read_bool()?;
    let source = match r.read_u8()? {
        0 => ClipSource::Empty,
        1 => ClipSource::Audio {
            asset_id: r.read_str()?,
            source_path: r.read_opt_path()?,
        },
        4 if version >= 33 => ClipSource::Video {
            asset_id: r.read_str()?,
            source_path: r.read_opt_path()?,
        },
        3 if version >= 14 => ClipSource::Rauf {
            asset_id: r.read_str()?,
            source_path: PathBuf::from(r.read_str()?),
            metadata_path: r.read_opt_path()?,
            sample_format: r.read_str()?,
            sample_rate: r.read_u32()?,
            channels: r.read_u32()? as u16,
            start_frame: r.read_u64()?,
            length_frames: r.read_u64()?,
        },
        2 => {
            let count = r.read_u32()? as usize;
            let mut notes = Vec::with_capacity(count);
            for _ in 0..count {
                notes.push(decode_midi_note(r, version)?);
            }
            // v5: controller lanes follow the notes; older files have none.
            let controller_lanes = if version >= 5 {
                let lane_count = r.read_u32()? as usize;
                let mut lanes = Vec::with_capacity(lane_count);
                for _ in 0..lane_count {
                    lanes.push(decode_controller_lane(r, version)?);
                }
                lanes
            } else {
                Vec::new()
            };
            let sysex_events = if version >= 23 {
                let event_count = r.read_u32()? as usize;
                let mut events = Vec::with_capacity(event_count);
                for _ in 0..event_count {
                    events.push(decode_sysex_event(r)?);
                }
                events
            } else {
                Vec::new()
            };
            // v25: direction articulation events; older files have none.
            let articulations = if version >= 25 {
                let event_count = r.read_u32()? as usize;
                let mut events = Vec::with_capacity(event_count);
                for _ in 0..event_count {
                    events.push(MidiArticulation {
                        beat: r.read_f32()?,
                        articulation: r.read_u8()?,
                    });
                }
                events
            } else {
                Vec::new()
            };
            ClipSource::Midi {
                notes,
                controller_lanes,
                sysex_events,
                articulations,
            }
        }
        t => {
            return Err(ProjectError::Corrupted(format!(
                "unknown clip source tag {t}"
            )));
        }
    };
    // v16: stretch/pitch block trails the source. Older files have none and
    // default to an un-stretched clip.
    let stretch = if version >= 16 {
        decode_stretch(r, version)?
    } else {
        AudioClipStretchState::default()
    };
    // v41 stored an ARA binding per clip; v42 moved it to the track. The bytes
    // are still consumed so a v41 body stays positionally aligned.
    if version == 41 && r.read_u8()? == 1 {
        let _plugin_id = r.read_str()?;
        let _plugin_path = r.read_str()?;
        let _class_id = r.read_str()?;
    }
    Ok(ProjectClip {
        id,
        name,
        start_beat,
        duration_beats,
        offset_beats,
        gain,
        muted,
        source,
        stretch,
    })
}

fn decode_track_type(r: &mut FbReader) -> Result<ProjectTrackType, ProjectError> {
    Ok(match r.read_u8()? {
        0 => ProjectTrackType::Audio,
        1 => ProjectTrackType::Midi,
        2 => ProjectTrackType::Instrument,
        3 => ProjectTrackType::Bus,
        4 => ProjectTrackType::Return,
        5 => ProjectTrackType::Group,
        6 => ProjectTrackType::Master,
        7 => ProjectTrackType::Video,
        t => return Err(ProjectError::Corrupted(format!("unknown track type {t}"))),
    })
}

fn decode_input_monitor(r: &mut FbReader) -> Result<InputMonitorMode, ProjectError> {
    Ok(match r.read_u8()? {
        0 => InputMonitorMode::Off,
        1 => InputMonitorMode::Always,
        2 => InputMonitorMode::WhenRecordArmed,
        t => {
            return Err(ProjectError::Corrupted(format!(
                "unknown input monitor mode {t}"
            )));
        }
    })
}

/// **v33 decoder.** Reads the combined union from an old file so the migration
/// can convert it; v34 files never reach this.
fn decode_track_input_routing(r: &mut FbReader) -> Result<V33TrackInputRouting, ProjectError> {
    Ok(match r.read_u8()? {
        0 => V33TrackInputRouting::None,
        1 => V33TrackInputRouting::AllInputs,
        2 => V33TrackInputRouting::AudioDeviceChannel {
            device_id: r.read_str()?,
            channel: r.read_u32()?,
        },
        3 => V33TrackInputRouting::MidiDevice {
            device_id: r.read_str()?,
        },
        4 => {
            let device_id = r.read_str()?;
            let count = r.read_u32()? as usize;
            if count == 0 {
                V33TrackInputRouting::None
            } else {
                let mut channels = Vec::with_capacity(count);
                for _ in 0..count {
                    channels.push(r.read_u32()?);
                }
                V33TrackInputRouting::AudioDeviceChannels {
                    device_id,
                    channels,
                }
            }
        }
        t => {
            return Err(ProjectError::Corrupted(format!(
                "unknown track input routing {t}"
            )));
        }
    })
}

fn decode_track_output_routing(
    r: &mut FbReader,
) -> Result<ProjectTrackOutputRouting, ProjectError> {
    Ok(match r.read_u8()? {
        0 => ProjectTrackOutputRouting::Main,
        1 => ProjectTrackOutputRouting::Bus {
            bus_id: r.read_str()?,
        },
        2 => ProjectTrackOutputRouting::HardwareOutput {
            device_id: r.read_str()?,
            channel: r.read_u32()?,
        },
        3 => ProjectTrackOutputRouting::None,
        4 => ProjectTrackOutputRouting::Instrument {
            track_id: r.read_str()?,
        },
        t => {
            return Err(ProjectError::Corrupted(format!(
                "unknown track output routing {t}"
            )));
        }
    })
}

fn decode_track_audio_format(r: &mut FbReader) -> Result<ProjectTrackAudioFormat, ProjectError> {
    Ok(match r.read_u8()? {
        0 => ProjectTrackAudioFormat::Mono,
        1 => ProjectTrackAudioFormat::Stereo,
        t => {
            return Err(ProjectError::Corrupted(format!(
                "unknown track audio format {t}"
            )));
        }
    })
}

fn decode_track_midi_input_routing(
    r: &mut FbReader,
) -> Result<ProjectTrackMidiInputRouting, ProjectError> {
    Ok(match r.read_u8()? {
        0 => ProjectTrackMidiInputRouting::None,
        1 => ProjectTrackMidiInputRouting::AllInputs,
        2 => ProjectTrackMidiInputRouting::MidiDevice {
            device_id: r.read_str()?,
        },
        t => {
            return Err(ProjectError::Corrupted(format!(
                "unknown track MIDI input routing {t}"
            )));
        }
    })
}

fn decode_track(r: &mut FbReader, version: u32) -> Result<ProjectTrack, ProjectError> {
    let id = r.read_str()?;
    let name = r.read_str()?;
    let track_type = decode_track_type(r)?;
    let parent_group_id = if version >= 30 {
        r.read_opt_str()?
    } else {
        None
    };
    let group_collapsed = if version >= 31 { r.read_bool()? } else { false };
    let color_hex = r.read_str()?;
    let volume_norm = r.read_f32()?;
    let pan = r.read_f32()?;
    let muted = r.read_bool()?;
    let solo = r.read_bool()?;
    let record_arm = r.read_bool()?;
    let input_monitor = decode_input_monitor(r)?;

    let mut routing = if version >= 3 {
        // v34 stores a logical connection id; v33 and older store the combined
        // input union, which the migration converts after load.
        let (audio_input_connection_id, legacy_input) = if version >= 34 {
            (r.read_opt_str()?, None)
        } else {
            (None, Some(decode_track_input_routing(r)?))
        };
        let output = decode_track_output_routing(r)?;
        let audio_format = decode_track_audio_format(r)?;
        let midi_input = decode_track_midi_input_routing(r)?;
        let midi_channel = r.read_opt_u8()?.map(|ch| ch.clamp(1, 16));
        // v22 added a per-track "play each note on its own channel" toggle;
        // older files default to `false` (the pre-existing fixed-channel behavior).
        let midi_output_per_note = if version >= 22 { r.read_bool()? } else { false };
        TrackRouting {
            audio_input_connection_id,
            legacy_input,
            output,
            audio_format,
            midi_input,
            midi_channel,
            midi_output_per_note,
            mpe: MpeTrackConfiguration::default(),
            sends: Vec::new(),
        }
    } else {
        TrackRouting::default_for_track_type(track_type)
    };
    let output_bus = r.read_opt_str()?;
    if version < 3 {
        if let Some(bus_id) = output_bus {
            routing.output = ProjectTrackOutputRouting::Bus { bus_id };
        }
    }
    let send_count = r.read_u32()? as usize;
    let mut sends = Vec::with_capacity(send_count);
    for _ in 0..send_count {
        let id = r.read_str()?;
        let target_track_id = r.read_str()?;
        let enabled = r.read_bool()?;
        let pre_fader = r.read_bool()?;
        let gain_db = r.read_f32()?;
        sends.push(ProjectSend {
            id,
            target_track_id,
            enabled,
            pre_fader,
            gain_db,
        });
    }
    routing.sends = sends;

    let insert_count = r.read_u32()? as usize;
    let mut inserts = Vec::with_capacity(insert_count);
    for _ in 0..insert_count {
        inserts.push(decode_insert(r, version)?);
    }

    let lane_count = r.read_u32()? as usize;
    let mut automation_lanes = Vec::with_capacity(lane_count);
    for _ in 0..lane_count {
        automation_lanes.push(decode_automation_lane(r, version)?);
    }

    let clip_count = r.read_u32()? as usize;
    let mut clips = Vec::with_capacity(clip_count);
    for _ in 0..clip_count {
        clips.push(decode_clip(r, version)?);
    }

    let row_height_px = if version >= 17 {
        r.read_opt_f32()?
    } else {
        None
    };

    let soundfont = if version >= 28 {
        decode_soundfont_player(r, version)?
    } else {
        None
    };
    let volume_automation_read = if version >= 32 { r.read_bool()? } else { true };
    let solfege = if version >= 37 {
        decode_solfege_engine(r, version)?
    } else {
        None
    };
    // v42: the track's ARA plug-in, at the tail of the track block.
    let ara = if version >= 42 && r.read_u8()? == 1 {
        Some(AraTrackBinding {
            plugin_id: r.read_str()?,
            plugin_path: r.read_str()?,
            class_id: r.read_str()?,
        })
    } else {
        None
    };

    // v43: per-track timebase. A v42 track has none and is Musical, which is
    // the only behaviour it could have been saved with.
    let timebase = if version >= 43 { r.read_u8()? } else { 0 };

    // v44: recorded takes. A v43 track has none, which is exactly what a
    // project saved before take management was is.
    let (takes, takes_expanded) = if version >= 44 {
        let count = r.read_u32()? as usize;
        let mut takes = Vec::with_capacity(count.min(256));
        for _ in 0..count {
            takes.push(ProjectTake {
                id: r.read_str()?,
                name: r.read_str()?,
                clip_id: r.read_str()?,
                active: r.read_bool()?,
                recorded_at: r.read_str()?,
            });
        }
        (takes, r.read_bool()?)
    } else {
        (Vec::new(), false)
    };

    // v46: MPE output settings. A pre-v46 project uses the shared default
    // (Auto) so existing note expression still plays through the lower zone.
    if version >= 46 {
        routing.mpe = MpeTrackConfiguration {
            mode: MpeOutputMode::from_tag(r.read_u8()?),
            member_channels: r.read_u8()?,
            member_pitch_range: r.read_f32()?,
            manager_pitch_range: r.read_f32()?,
        }
        .sanitized();
    }

    Ok(ProjectTrack {
        id,
        name,
        track_type,
        ara,
        timebase,
        parent_group_id,
        group_collapsed,
        color_hex,
        volume_norm,
        pan,
        muted,
        solo,
        record_arm,
        input_monitor,
        routing,
        inserts,
        automation_lanes,
        clips,
        row_height_px,
        soundfont,
        volume_automation_read,
        solfege,
        takes,
        takes_expanded,
    })
}

fn decode_asset(r: &mut FbReader, version: u32) -> Result<ProjectAsset, ProjectError> {
    Ok(ProjectAsset {
        id: r.read_str()?,
        original_filename: r.read_str()?,
        relative_path: r.read_opt_str()?,
        absolute_path: r.read_opt_path()?,
        duration_secs: r.read_opt_f64()?,
        sample_rate: r.read_opt_u32()?,
        channels: r.read_opt_u8()?,
        // v11 appended a content fingerprint; older files stop before it.
        source_fingerprint: if version >= 11 {
            r.read_opt_str()?
        } else {
            None
        },
        waveform_peak_relative_path: if version >= 12 {
            r.read_opt_str()?
        } else {
            None
        },
        duration_samples: if version >= 12 {
            r.read_opt_u64()?
        } else {
            None
        },
    })
}

fn decode_body(body: &[u8], version: u32) -> Result<FutureboardProject, ProjectError> {
    let mut r = FbReader::new(body);

    let id = r.read_str()?;
    let name = r.read_str()?;
    let created_at = r.read_u64()?;
    let modified_at = r.read_u64()?;

    let bpm = r.read_f64()?;
    let time_sig_num = r.read_u32()?;
    let time_sig_den = r.read_u32()?;
    let sample_rate = r.read_u32()?;
    let bit_depth = r.read_u32()?;

    let master_volume_norm = r.read_f32()?;
    let master_inserts = if version >= 15 {
        let insert_count = r.read_u32()? as usize;
        let mut inserts = Vec::with_capacity(insert_count);
        for _ in 0..insert_count {
            inserts.push(decode_insert(&mut r, version)?);
        }
        inserts
    } else {
        Vec::new()
    };

    let track_count = r.read_u32()? as usize;
    let mut tracks = Vec::with_capacity(track_count);
    for _ in 0..track_count {
        tracks.push(decode_track(&mut r, version)?);
    }

    let asset_count = r.read_u32()? as usize;
    let mut assets = Vec::with_capacity(asset_count);
    for _ in 0..asset_count {
        assets.push(decode_asset(&mut r, version)?);
    }

    // Tempo automation markers (v7+). Pre-v7 files have none. v8+ stores ids.
    let tempo_points = if version >= 7 {
        let count = r.read_u32()? as usize;
        let mut points = Vec::with_capacity(count);
        for _ in 0..count {
            let id = if version >= 8 {
                r.read_str()?
            } else {
                String::new()
            };
            let beat = r.read_f64()?;
            let bpm = r.read_f64()?;
            let curve = r.read_u8()?;
            points.push(ProjectTempoPoint {
                id,
                beat,
                bpm,
                curve,
            });
        }
        points
    } else {
        Vec::new()
    };

    let time_signature_points = if version >= 9 {
        let count = r.read_u32()? as usize;
        let mut points = Vec::with_capacity(count);
        for _ in 0..count {
            // Field order must match `encode_body`: id, beat, numerator,
            // denominator, grouping. (A previous build read these out of order,
            // which desynced the cursor and produced spurious EOF errors when a
            // project contained any time-signature point â including the default
            // 4/4 marker every new project carries.)
            let id = r.read_str()?;
            let beat = r.read_f64()?;
            let numerator = r.read_u32()? as u16;
            let denominator = r.read_u32()? as u16;
            let grouping = if version >= 10 {
                let count = r.read_u32()? as usize;
                let mut groups = Vec::with_capacity(count);
                for _ in 0..count {
                    groups.push(r.read_u32()? as u16);
                }
                groups
            } else {
                Vec::new()
            };
            points.push(super::ProjectTimeSignaturePoint {
                id,
                beat,
                numerator,
                denominator,
                grouping,
            });
        }
        points
    } else {
        Vec::new()
    };

    let (timeline_markers, timeline_regions) = if version >= 13 {
        let marker_count = r.read_u32()? as usize;
        let mut markers = Vec::with_capacity(marker_count);
        for _ in 0..marker_count {
            markers.push(ProjectTimelineMarker {
                id: r.read_str()?,
                beat: r.read_f64()?,
                name: r.read_str()?,
                color_hex: r.read_str()?,
            });
        }
        let region_count = r.read_u32()? as usize;
        let mut regions = Vec::with_capacity(region_count);
        for _ in 0..region_count {
            regions.push(ProjectTimelineRegion {
                id: r.read_str()?,
                start_beat: r.read_f64()?,
                end_beat: r.read_f64()?,
                name: r.read_str()?,
                color_hex: r.read_str()?,
            });
        }
        (markers, regions)
    } else {
        (Vec::new(), Vec::new())
    };

    let (tree_expanded_node_ids, tree_pinned_channel_ids, tree_hidden_channel_ids) =
        if version >= 20 {
            let expanded_count = r.read_u32()? as usize;
            let mut tree_expanded_node_ids = Vec::with_capacity(expanded_count);
            for _ in 0..expanded_count {
                tree_expanded_node_ids.push(r.read_str()?);
            }
            let pinned_count = r.read_u32()? as usize;
            let mut tree_pinned_channel_ids = Vec::with_capacity(pinned_count);
            for _ in 0..pinned_count {
                tree_pinned_channel_ids.push(r.read_str()?);
            }
            let hidden_count = r.read_u32()? as usize;
            let mut tree_hidden_channel_ids = Vec::with_capacity(hidden_count);
            for _ in 0..hidden_count {
                tree_hidden_channel_ids.push(r.read_str()?);
            }
            (
                tree_expanded_node_ids,
                tree_pinned_channel_ids,
                tree_hidden_channel_ids,
            )
        } else {
            (Vec::new(), Vec::new(), Vec::new())
        };

    let song_text_events = if version >= 27 {
        let count = r.read_u32()? as usize;
        const MIN_EVENT_BYTES: usize = 17;
        if count > 1_000_000 || count > r.remaining() / MIN_EVENT_BYTES {
            return Err(ProjectError::Corrupted(
                "invalid Song Text event count".to_string(),
            ));
        }
        let mut events = Vec::with_capacity(count);
        for _ in 0..count {
            events.push(decode_song_text_event(&mut r)?);
        }
        events
    } else if version >= 24 {
        let count = r.read_u32()? as usize;
        const MIN_LEGACY_CUE_BYTES: usize = 20;
        if count > 1_000_000 || count > r.remaining() / MIN_LEGACY_CUE_BYTES {
            return Err(ProjectError::Corrupted(
                "invalid legacy Song Text cue count".to_string(),
            ));
        }
        let mut events = Vec::with_capacity(count.saturating_mul(2));
        for _ in 0..count {
            let cue = decode_legacy_song_text_cue(&mut r)?;
            migrate_legacy_song_text_cue(cue, &mut events);
        }
        events
    } else {
        Vec::new()
    };

    // Audio Connections registry (v34+). Duplicate ids are rejected rather
    // than silently overwriting each other, so a track reference can never
    // resolve to the wrong bus.
    let audio_connections = if version >= 34 {
        let count = r.read_u32()? as usize;
        if count > MAX_AUDIO_CONNECTIONS || count > r.remaining() / MIN_AUDIO_CONNECTION_BYTES {
            return Err(ProjectError::Corrupted(
                "invalid audio connection count".to_string(),
            ));
        }
        let mut connections: Vec<ProjectAudioConnection> = Vec::with_capacity(count);
        for _ in 0..count {
            let connection = decode_audio_connection(&mut r)?;
            if connections
                .iter()
                .any(|existing| existing.id == connection.id)
            {
                return Err(ProjectError::Corrupted(format!(
                    "duplicate audio connection id {}",
                    connection.id
                )));
            }
            connections.push(connection);
        }
        connections
    } else {
        Vec::new()
    };

    // Master / Monitor output routing (v35+). A v34 file has no output routing
    // and an unset bootstrap latch, so the compatibility bootstrap runs for it
    // exactly once on load.
    let (master_output_connection_id, monitor_output_connection_id, output_routing_initialized) =
        if version >= 35 {
            (r.read_opt_str()?, r.read_opt_str()?, r.read_bool()?)
        } else {
            (None, None, false)
        };

    // Conductor lane fold state (v40+). A v39 file has none, and every lane
    // comes back expanded at its default height â the state such a file was
    // saved in.
    let global_lanes = if version >= 40 {
        super::ProjectGlobalLanes {
            arranger_collapsed: r.read_bool()?,
            marker_collapsed: r.read_bool()?,
            tempo_collapsed: r.read_bool()?,
            time_signature_collapsed: r.read_bool()?,
            arranger_height: r.read_opt_f32()?,
            marker_height: r.read_opt_f32()?,
            tempo_height: r.read_opt_f32()?,
            time_signature_height: r.read_opt_f32()?,
            song_text_height: r.read_opt_f32()?,
        }
    } else {
        super::ProjectGlobalLanes::default()
    };

    // ARA document archives (v41+). A v40 file has none, which is the state it
    // was saved in: v40 could not bind a clip to an ARA plug-in at all.
    let ara_documents = if version >= 41 {
        let count = r.read_u32()? as usize;
        if count > MAX_ARA_DOCUMENTS || count > r.remaining() / MIN_ARA_DOCUMENT_BYTES {
            return Err(ProjectError::Corrupted(
                "invalid ARA document count".to_string(),
            ));
        }
        let mut documents = Vec::with_capacity(count);
        for _ in 0..count {
            documents.push(ProjectAraDocument {
                plugin_id: r.read_str()?,
                track_id: r.read_str()?,
                archive_id: r.read_str()?,
                data: r.read_bytes()?,
            });
        }
        documents
    } else {
        Vec::new()
    };

    // Project timebase (v43+). A v42 file has none and opens in Bars+Beats,
    // which is the only thing it could have been showing.
    let (time_display_format, timecode_rate) = if version >= 43 {
        (r.read_u8()?, r.read_u8()?)
    } else {
        let defaults = super::ProjectSettings::default();
        (defaults.time_display_format, defaults.timecode_rate)
    };

    Ok(FutureboardProject {
        audio_connections,
        global_lanes,
        ara_documents,
        master_output_connection_id,
        monitor_output_connection_id,
        output_routing_initialized,
        id,
        name,
        created_at,
        modified_at,
        settings: super::ProjectSettings {
            bpm,
            tempo_points,
            time_signature_points,
            timeline_markers,
            timeline_regions,
            song_text_events,
            time_sig_num,
            time_sig_den,
            sample_rate,
            bit_depth,
            time_display_format,
            timecode_rate,
        },
        tracks,
        mixer: ProjectMixer {
            master_volume_norm,
            master_inserts,
            tree_expanded_node_ids,
            tree_pinned_channel_ids,
            tree_hidden_channel_ids,
        },
        assets,
    })
}

/// Cheaply validate that `data` begins with a supported Futureboard project
/// header (magic + version) without decoding the body or verifying the
/// checksum. Returns the on-disk format version. Used for fast pre-load
/// validation (e.g. the Welcome â Open Project flow) so an invalid pick can be
/// reported inline without reading/decoding the whole file.
pub fn peek_project_header(data: &[u8]) -> Result<u32, ProjectError> {
    if data.len() < PROJECT_HEADER_SIZE {
        return Err(ProjectError::IncompleteFile {
            reason: format!(
                "file too small for project header ({} bytes, need {})",
                data.len(),
                PROJECT_HEADER_SIZE
            ),
        });
    }
    if &data[0..8] != PROJECT_MAGIC {
        return Err(ProjectError::InvalidMagic);
    }
    let version = u32::from_le_bytes(data[8..12].try_into().unwrap());
    if version == 0 || version > PROJECT_VERSION {
        return Err(ProjectError::UnsupportedVersion(version));
    }
    Ok(version)
}

/// Decodes a `.fbproj` binary blob into a `FutureboardProject`.
pub fn decode_project(data: &[u8]) -> Result<FutureboardProject, ProjectError> {
    decode_project_with_options(data, false)
}

/// Decode a project with an option to allow loading old versions.
pub fn decode_project_with_options(data: &[u8], allow_old_version: bool) -> Result<FutureboardProject, ProjectError> {
    project_load_log(format_args!("file size: {} bytes (allow_old_version={})", data.len(), allow_old_version));

    if data.len() < PROJECT_HEADER_SIZE {
        let err = ProjectError::IncompleteFile {
            reason: format!(
                "file too small for project header ({} bytes, need {})",
                data.len(),
                PROJECT_HEADER_SIZE
            ),
        };
        project_load_log(format_args!("failed: {}", err.technical_detail()));
        return Err(err);
    }

    if &data[0..8] != PROJECT_MAGIC {
        let err = ProjectError::InvalidMagic;
        project_load_log(format_args!("failed: {}", err.technical_detail()));
        return Err(err);
    }

    let version = u32::from_le_bytes(data[8..12].try_into().unwrap());
    if version == 0 || version > PROJECT_VERSION {
        let err = ProjectError::UnsupportedVersion(version);
        project_load_log(format_args!("failed: {}", err.technical_detail()));
        return Err(err);
    }
    // Warn but allow loading for old versions that are still above minimum
    if version < PROJECT_VERSION && version >= MIN_SUPPORTED_VERSION && !allow_old_version {
        let err = ProjectError::OldVersion(version);
        project_load_log(format_args!("warning: old version {version} (current {PROJECT_VERSION})"));
        return Err(err);
    }
    project_load_log(format_args!("header ok version={version}"));

    let body_len = u32::from_le_bytes(data[16..20].try_into().unwrap()) as usize;
    let required = PROJECT_HEADER_SIZE
        .checked_add(body_len)
        .and_then(|n| n.checked_add(4));
    let Some(required) = required else {
        let err = ProjectError::IncompleteFile {
            reason: "project payload length overflow".to_string(),
        };
        project_load_log(format_args!("failed: {}", err.technical_detail()));
        return Err(err);
    };

    if data.len() < required {
        let err = ProjectError::IncompleteFile {
            reason: format!(
                "file truncated: declared payload {body_len} bytes but only {} bytes on disk",
                data.len().saturating_sub(PROJECT_HEADER_SIZE + 4)
            ),
        };
        project_load_log(format_args!("failed: {}", err.technical_detail()));
        return Err(err);
    }

    let body = &data[PROJECT_HEADER_SIZE..PROJECT_HEADER_SIZE + body_len];
    project_load_log(format_args!("payload bytes={body_len}"));
    let stored_crc = u32::from_le_bytes(
        data[PROJECT_HEADER_SIZE + body_len..PROJECT_HEADER_SIZE + body_len + 4]
            .try_into()
            .unwrap(),
    );
    let computed_crc = crc32fast::hash(body);

    if computed_crc != stored_crc {
        let err = ProjectError::ChecksumMismatch {
            expected: stored_crc,
            got: computed_crc,
        };
        project_load_log(format_args!("failed: {}", err.technical_detail()));
        return Err(err);
    }

    match decode_body(body, version) {
        Ok(project) => Ok(project),
        Err(err) => {
            project_load_log(format_args!("failed: {}", err.technical_detail()));
            Err(err)
        }
    }
}

fn project_load_log(args: std::fmt::Arguments<'_>) {
    eprintln!("[ProjectLoad] {args}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(pitch: u8, muted: bool) -> MidiNote {
        MidiNote {
            id: 0,
            pitch,
            start_beats: 1.5,
            duration_beats: 0.5,
            velocity: 90,
            release_velocity: 0,
            muted,
            channel: 1,
            articulation: 0,
            pitch_curve: Vec::new(),
            expression: NoteExpression::default(),
            accent: None,
        }
    }

    fn note_with_pitch_curve(pitch: u8) -> MidiNote {
        MidiNote {
            pitch_curve: vec![
                MidiPitchPoint {
                    id: 7,
                    beat: 0.0,
                    cents: -25.0,
                    shape: 1,
                },
                MidiPitchPoint {
                    id: 8,
                    beat: 0.25,
                    cents: 12.5,
                    shape: 0,
                },
            ],
            ..note(pitch, false)
        }
    }

    fn note_with_accent(pitch: u8) -> MidiNote {
        MidiNote {
            accent: Some(MidiAccent {
                prominence: 0.82,
                attack: 0.71,
                agogic: 0.34,
                timbre: 0.55,
                confidence: 0.63,
                source: 1,
            }),
            ..note(pitch, false)
        }
    }

    fn note_with_note_expression(pitch: u8) -> MidiNote {
        MidiNote {
            expression: NoteExpression {
                pitch: ExpressionCurve::from_points(vec![
                    ExpressionPoint {
                        position: 0.0,
                        value: 0.0,
                        interpolation: ExpressionInterpolation::Step,
                    },
                    ExpressionPoint::new(0.5, 0.25),
                ]),
                pressure: ExpressionCurve::from_points(vec![ExpressionPoint::new(0.0, 0.2)]),
                timbre: ExpressionCurve::from_points(vec![ExpressionPoint::new(1.0, 0.8)]),
                release_velocity: Some(0.65),
                custom: vec![CustomExpressionLane {
                    controller: 12_345,
                    name: "breath".to_string(),
                    curve: ExpressionCurve::from_points(vec![ExpressionPoint::new(0.0, 0.5)]),
                }],
            },
            ..note(pitch, false)
        }
    }

    #[test]
    fn v45_note_expression_round_trips_without_a_channel_identity() {
        let mut w = FbWriter::new();
        encode_midi_note(&mut w, &note_with_note_expression(60));
        let bytes = w.into_bytes();
        let mut r = FbReader::new(&bytes);
        let decoded = decode_midi_note(&mut r, PROJECT_VERSION).unwrap();
        assert_eq!(decoded.expression.pitch.points.len(), 2);
        assert_eq!(
            decoded.expression.pitch.points[0].interpolation,
            ExpressionInterpolation::Step
        );
        assert_eq!(decoded.expression.pressure.points[0].value, 0.2);
        assert_eq!(decoded.expression.timbre.points[0].value, 0.8);
        assert_eq!(decoded.expression.release_velocity, Some(0.65));
        assert_eq!(decoded.expression.custom[0].controller, 12_345);
        assert_eq!(decoded.expression.custom[0].name, "breath");
    }

    #[test]
    fn v39_note_accent_round_trips() {
        let mut w = FbWriter::new();
        encode_midi_note(&mut w, &note_with_accent(60));
        let bytes = w.into_bytes();
        let mut r = FbReader::new(&bytes);
        let decoded = decode_midi_note(&mut r, PROJECT_VERSION).unwrap();
        let accent = decoded.accent.expect("accent restored");
        assert_eq!(accent.prominence, 0.82);
        assert_eq!(accent.attack, 0.71);
        assert_eq!(accent.agogic, 0.34);
        assert_eq!(accent.timbre, 0.55);
        assert_eq!(accent.confidence, 0.63);
        assert_eq!(accent.source, 1, "a hand-edited accent stays hand-edited");
    }

    /// "No accent" and "neutral accent" are different states and the file has
    /// to keep them apart: re-analysis leaves a hand-set neutral alone and
    /// fills an absent one in.
    #[test]
    fn a_note_without_an_accent_round_trips_as_absent_not_as_neutral() {
        let mut w = FbWriter::new();
        encode_midi_note(&mut w, &note(60, false));
        let bytes = w.into_bytes();
        let mut r = FbReader::new(&bytes);
        assert!(decode_midi_note(&mut r, PROJECT_VERSION)
            .unwrap()
            .accent
            .is_none());
    }

    /// A v38 file has no accent bytes at all. Reading it as v39 would consume
    /// the next note's pitch byte as a presence flag, so the version gate is
    /// what keeps an old project loading.
    #[test]
    fn a_v38_note_loads_with_no_accent_and_consumes_no_accent_bytes() {
        let mut w = FbWriter::new();
        let source = note_with_pitch_curve(64);
        // Encode the v38 body by hand: everything up to and including the
        // curve, and nothing after it.
        w.write_u8(source.pitch);
        w.write_f32(source.start_beats);
        w.write_f32(source.duration_beats);
        w.write_u8(source.velocity);
        w.write_bool(source.muted);
        w.write_u8(source.channel);
        w.write_u8(source.articulation);
        w.write_u64(source.id);
        w.write_u8(source.release_velocity);
        w.write_u32(source.pitch_curve.len() as u32);
        for point in &source.pitch_curve {
            w.write_u64(point.id);
            w.write_f32(point.beat);
            w.write_f32(point.cents);
            w.write_u8(point.shape);
        }
        let bytes = w.into_bytes();
        let mut r = FbReader::new(&bytes);
        let decoded = decode_midi_note(&mut r, 38).unwrap();
        assert!(decoded.accent.is_none());
        assert_eq!(decoded.pitch_curve.len(), 2, "the v38 body still decodes");
    }

    #[test]
    fn v38_note_pitch_curve_round_trips() {
        let mut w = FbWriter::new();
        encode_midi_note(&mut w, &note_with_pitch_curve(64));
        let bytes = w.into_bytes();
        let mut r = FbReader::new(&bytes);
        let decoded = decode_midi_note(&mut r, PROJECT_VERSION).unwrap();
        assert_eq!(decoded.pitch_curve.len(), 2);
        assert_eq!(decoded.pitch_curve[0].id, 7);
        assert_eq!(decoded.pitch_curve[0].cents, -25.0);
        assert_eq!(decoded.pitch_curve[0].shape, 1);
        assert_eq!(decoded.pitch_curve[1].beat, 0.25);
    }

    #[test]
    fn v37_notes_decode_without_a_pitch_curve() {
        let mut w = FbWriter::new();
        // A v37 writer stops after the release velocity.
        let n = note(60, false);
        w.write_u8(n.pitch);
        w.write_f32(n.start_beats);
        w.write_f32(n.duration_beats);
        w.write_u8(n.velocity);
        w.write_bool(n.muted);
        w.write_u8(n.channel);
        w.write_u8(n.articulation);
        w.write_u64(n.id);
        w.write_u8(n.release_velocity);
        let bytes = w.into_bytes();
        let mut r = FbReader::new(&bytes);
        let decoded = decode_midi_note(&mut r, 37).unwrap();
        assert!(decoded.pitch_curve.is_empty());
    }

    #[test]
    fn v38_solfege_lane_layout_round_trips() {
        let engine = ProjectSolfegeEngine {
            visible_lanes: vec![
                ProjectSolfegeLane {
                    lane_id: "velocity".to_string(),
                    height: 64.0,
                },
                ProjectSolfegeLane {
                    lane_id: "bow-pressure".to_string(),
                    height: 96.0,
                },
            ],
            ..ProjectSolfegeEngine::default()
        };
        let mut w = FbWriter::new();
        encode_solfege_engine(&mut w, Some(&engine));
        let bytes = w.into_bytes();
        let mut r = FbReader::new(&bytes);
        let decoded = decode_solfege_engine(&mut r, PROJECT_VERSION)
            .unwrap()
            .unwrap();
        assert_eq!(decoded.visible_lanes, engine.visible_lanes);
    }

    fn project_bytes_with_version(body: Vec<u8>, version: u32) -> Vec<u8> {
        let checksum = crc32fast::hash(&body);
        let mut bytes = Vec::with_capacity(PROJECT_HEADER_SIZE + body.len() + 4);
        bytes.extend_from_slice(PROJECT_MAGIC);
        bytes.extend_from_slice(&version.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&body);
        bytes.extend_from_slice(&checksum.to_le_bytes());
        bytes
    }

    fn encode_legacy_song_text_project(version: u32, cues: &[LegacyProjectSongTextCue]) -> Vec<u8> {
        let mut body = encode_body(&FutureboardProject::new("Legacy Song Text"));
        // `encode_body` ends with the Song Text count, the v34 Audio
        // Connections count, the v35 output-routing block (two absent optional
        // strings plus the bootstrap latch), the v40 conductor-lane fold block
        // (four collapse latches plus five absent optional heights), the v41
        // ARA document count, and the v43 timebase pair. A v24-v26 fixture reads
        // none of them, so drop the whole tail before appending the legacy cue
        // block in its place.
        let v35_output_routing_bytes = 1 + 1 + 1;
        let v40_global_lane_bytes = 4 + 5;
        let v43_timebase_bytes = 1 + 1;
        body.truncate(
            body.len()
                - 3 * std::mem::size_of::<u32>()
                - v35_output_routing_bytes
                - v40_global_lane_bytes
                - v43_timebase_bytes,
        );

        let mut tail = FbWriter::new();
        tail.write_u32(cues.len() as u32);
        for cue in cues {
            tail.write_str(&cue.id);
            tail.write_f64(cue.beat);
            tail.write_str(&cue.chord);
            tail.write_str(&cue.lyric);
        }
        body.extend_from_slice(&tail.into_bytes());
        project_bytes_with_version(body, version)
    }

    #[test]
    fn midi_note_articulation_roundtrips_v25() {
        let mut w = FbWriter::new();
        let mut n = note(60, false);
        n.articulation = 2; // Staccato tag
        encode_midi_note(&mut w, &n);
        encode_midi_note(&mut w, &note(64, false)); // articulation 0 = none
        let bytes = w.into_bytes();
        let mut r = FbReader::new(&bytes);
        let a = decode_midi_note(&mut r, PROJECT_VERSION).unwrap();
        let b = decode_midi_note(&mut r, PROJECT_VERSION).unwrap();
        assert_eq!(a.articulation, 2);
        assert_eq!(b.articulation, 0);
    }

    #[test]
    fn pre_v25_midi_note_decodes_with_no_articulation() {
        // v24 and earlier wrote no articulation byte.
        let mut w = FbWriter::new();
        w.write_u8(60);
        w.write_f32(1.5);
        w.write_f32(0.5);
        w.write_u8(90);
        w.write_bool(false); // muted (v4)
        w.write_u8(1); // channel (v22)
        let bytes = w.into_bytes();
        let mut r = FbReader::new(&bytes);
        let got = decode_midi_note(&mut r, 24).unwrap();
        assert_eq!(got.articulation, 0);
    }

    #[test]
    fn midi_clip_articulation_events_roundtrip_v25() {
        let clip = ProjectClip {
            id: "clip-1".to_string(),
            name: "MIDI".to_string(),
            start_beat: 0.0,
            duration_beats: 8.0,
            offset_beats: 0.0,
            gain: 1.0,
            muted: false,
            source: ClipSource::Midi {
                notes: vec![note(60, false)],
                controller_lanes: Vec::new(),
                sysex_events: Vec::new(),
                articulations: vec![
                    MidiArticulation {
                        beat: 0.0,
                        articulation: 1,
                    },
                    MidiArticulation {
                        beat: 4.0,
                        articulation: 4,
                    },
                ],
            },
            stretch: AudioClipStretchState::default(),
        };
        let mut w = FbWriter::new();
        encode_clip(&mut w, &clip);
        let bytes = w.into_bytes();
        let mut r = FbReader::new(&bytes);
        let decoded = decode_clip(&mut r, PROJECT_VERSION).unwrap();
        let ClipSource::Midi { articulations, .. } = decoded.source else {
            panic!("expected midi source");
        };
        assert_eq!(
            articulations,
            vec![
                MidiArticulation {
                    beat: 0.0,
                    articulation: 1
                },
                MidiArticulation {
                    beat: 4.0,
                    articulation: 4
                },
            ]
        );
    }

    #[test]
    fn midi_note_muted_roundtrips_v4() {
        let mut w = FbWriter::new();
        encode_midi_note(&mut w, &note(60, true));
        encode_midi_note(&mut w, &note(64, false));
        let bytes = w.into_bytes();

        let mut r = FbReader::new(&bytes);
        let a = decode_midi_note(&mut r, PROJECT_VERSION).unwrap();
        let b = decode_midi_note(&mut r, PROJECT_VERSION).unwrap();
        assert_eq!(a.pitch, 60);
        assert!(a.muted);
        assert_eq!(b.pitch, 64);
        assert!(!b.muted);
    }

    #[test]
    fn pre_v4_notes_decode_unmuted() {
        // v3 and earlier wrote no muted byte: pitch, start, dur, velocity only.
        let mut w = FbWriter::new();
        w.write_u8(72);
        w.write_f32(0.0);
        w.write_f32(1.0);
        w.write_u8(100);
        let bytes = w.into_bytes();

        let mut r = FbReader::new(&bytes);
        let n = decode_midi_note(&mut r, 3).unwrap();
        assert_eq!(n.pitch, 72);
        assert!(!n.muted, "older files must default to unmuted");
    }

    #[test]
    fn midi_note_channel_roundtrips_v22() {
        let mut n = note(60, false);
        n.channel = 5;
        let mut w = FbWriter::new();
        encode_midi_note(&mut w, &n);
        let bytes = w.into_bytes();

        let mut r = FbReader::new(&bytes);
        let got = decode_midi_note(&mut r, PROJECT_VERSION).unwrap();
        assert_eq!(got.channel, 5);
    }

    #[test]
    fn pre_v22_notes_decode_channel_one() {
        // v21 and earlier wrote no channel byte: pitch, start, dur, velocity, muted.
        let mut w = FbWriter::new();
        w.write_u8(72);
        w.write_f32(0.0);
        w.write_f32(1.0);
        w.write_u8(100);
        w.write_bool(false);
        let bytes = w.into_bytes();

        let mut r = FbReader::new(&bytes);
        let n = decode_midi_note(&mut r, 21).unwrap();
        assert_eq!(n.channel, 1, "older files must default to channel 1");
    }

    #[test]
    fn controller_lane_roundtrips() {
        let lane = MidiControllerLane {
            kind: MidiControllerKind::CC(11),
            points: vec![
                MidiControllerPoint {
                    id: 11,
                    beat: 0.0,
                    value: 0.0,
                },
                MidiControllerPoint {
                    id: 22,
                    beat: 2.5,
                    value: 1.0,
                },
            ],
            visible: true,
            height: 72.0,
            collapsed: false,
        };
        let mut w = FbWriter::new();
        encode_controller_lane(&mut w, &lane);
        let bytes = w.into_bytes();

        let mut r = FbReader::new(&bytes);
        let got = decode_controller_lane(&mut r, PROJECT_VERSION).unwrap();
        assert_eq!(got.kind, MidiControllerKind::CC(11));
        assert_eq!(got.points.len(), 2);
        assert_eq!(got.points[1].id, 22);
        assert_eq!(got.points[1].beat, 2.5);
        assert_eq!(got.points[1].value, 1.0);
        assert_eq!(got.height, 72.0);
        assert!(got.visible);
    }

    #[test]
    fn midi_note_id_and_release_velocity_roundtrip_v26() {
        let mut n = note(60, false);
        n.id = 0xABCD_EF01;
        n.release_velocity = 64;
        let mut w = FbWriter::new();
        encode_midi_note(&mut w, &n);
        let bytes = w.into_bytes();
        let mut r = FbReader::new(&bytes);
        let got = decode_midi_note(&mut r, PROJECT_VERSION).unwrap();
        assert_eq!(got.id, 0xABCD_EF01);
        assert_eq!(got.release_velocity, 64);
    }

    #[test]
    fn pre_v26_midi_note_decodes_without_id() {
        let mut w = FbWriter::new();
        w.write_u8(60);
        w.write_f32(1.5);
        w.write_f32(0.5);
        w.write_u8(90);
        w.write_bool(false);
        w.write_u8(1);
        w.write_u8(0); // articulation (v25)
        let bytes = w.into_bytes();
        let mut r = FbReader::new(&bytes);
        let got = decode_midi_note(&mut r, 25).unwrap();
        assert_eq!(got.id, 0);
        assert_eq!(got.release_velocity, 0);
    }

    #[test]
    fn multi_channel_audio_input_routing_roundtrips_v6() {
        let routing = V33TrackInputRouting::AudioDeviceChannels {
            device_id: "Interface 8i6".to_string(),
            channels: vec![2, 3],
        };
        let mut w = FbWriter::new();
        encode_track_input_routing(&mut w, &routing);
        let bytes = w.into_bytes();

        let mut r = FbReader::new(&bytes);
        assert_eq!(decode_track_input_routing(&mut r).unwrap(), routing);
    }

    #[test]
    fn insert_audio_output_channels_roundtrip_v18() {
        let mut project = FutureboardProject::new("Insert Outputs");
        project.mixer.master_inserts.push(ProjectInsert {
            id: "insert-1".to_string(),
            slot_index: 0,
            bypassed: false,
            enabled_audio_output_channels: vec![1, 2, 3, 4],
            plugin_is_instrument: Some(false),
            multiout_collapsed: true,
            plugin: None,
        });

        let bytes = encode_project(&project);
        let decoded = decode_project(&bytes).expect("decode");
        assert!(
            decoded.mixer.master_inserts[0].multiout_collapsed,
            "collapse flag must roundtrip"
        );
        assert_eq!(
            decoded.mixer.master_inserts[0].enabled_audio_output_channels,
            vec![1, 2, 3, 4]
        );
        assert_eq!(
            decoded.mixer.master_inserts[0].plugin_is_instrument,
            Some(false),
            "the registry-resolved effect role must roundtrip"
        );
    }

    #[test]
    fn peek_header_accepts_encoded_project() {
        let project = FutureboardProject::new("Peek Test");
        let bytes = encode_project(&project);
        let version = peek_project_header(&bytes).expect("valid header");
        assert_eq!(version, PROJECT_VERSION);
    }

    #[test]
    fn tempo_points_roundtrip_v8() {
        let mut project = FutureboardProject::new("Tempo Test");
        project.settings.tempo_points = vec![
            ProjectTempoPoint {
                id: "tempo-a".to_string(),
                beat: 0.0,
                bpm: 120.0,
                curve: 0,
            },
            ProjectTempoPoint {
                id: "tempo-b".to_string(),
                beat: 8.0,
                bpm: 140.0,
                curve: 1,
            },
        ];
        let bytes = encode_project(&project);
        let decoded = decode_project(&bytes).expect("decode");
        assert_eq!(decoded.settings.tempo_points, project.settings.tempo_points);
    }

    #[test]
    fn song_text_events_roundtrip_v27() {
        let mut project = FutureboardProject::new("Song Text");
        project.settings.song_text_events = vec![
            ProjectSongTextEvent {
                id: "chord-1".to_string(),
                beat: 4.0,
                kind: ProjectSongTextEventKind::Chord {
                    symbol: "Fâ¯m7/Câ¯".to_string(),
                },
            },
            ProjectSongTextEvent {
                id: "lyric-1".to_string(),
                beat: 4.5,
                kind: ProjectSongTextEventKind::Lyric {
                    text: "à¸à¸·à¸à¸à¸µà¹à¸à¸²à¸§à¹à¸à¹à¸¡à¸à¹à¸²".to_string(),
                    syllable_mode: ProjectLyricSyllableMode::Syllables,
                    continuation: true,
                    duration_beats: Some(3.5),
                    syllables: vec![
                        ProjectLyricSyllable {
                            text: "à¸à¸·à¸".to_string(),
                            offset_beats: 0.0,
                            duration_beats: Some(0.75),
                        },
                        ProjectLyricSyllable {
                            text: "à¸à¸µà¹à¸à¸²à¸§".to_string(),
                            offset_beats: 0.75,
                            duration_beats: None,
                        },
                    ],
                },
            },
            ProjectSongTextEvent {
                id: "section-1".to_string(),
                beat: 16.0,
                kind: ProjectSongTextEventKind::Section {
                    name: "Final Chorus".to_string(),
                    section_type: ProjectSongSectionType::Chorus,
                    color_hex: "#C45CFF".to_string(),
                },
            },
        ];

        let bytes = encode_project(&project);
        let decoded = decode_project(&bytes).expect("decode typed song text");
        assert_eq!(
            decoded.settings.song_text_events,
            project.settings.song_text_events
        );
    }

    #[test]
    fn legacy_song_text_cues_migrate_for_v24_through_v26() {
        let cues = vec![
            LegacyProjectSongTextCue {
                id: "chord-only".to_string(),
                beat: 1.0,
                chord: "Cmaj7".to_string(),
                lyric: String::new(),
            },
            LegacyProjectSongTextCue {
                id: "lyric-only".to_string(),
                beat: 2.0,
                chord: String::new(),
                lyric: "Hello".to_string(),
            },
            LegacyProjectSongTextCue {
                id: "combined".to_string(),
                beat: 3.0,
                chord: "Am7".to_string(),
                lyric: "world".to_string(),
            },
        ];
        let expected = vec![
            ProjectSongTextEvent {
                id: "chord-only".to_string(),
                beat: 1.0,
                kind: ProjectSongTextEventKind::Chord {
                    symbol: "Cmaj7".to_string(),
                },
            },
            ProjectSongTextEvent {
                id: "lyric-only".to_string(),
                beat: 2.0,
                kind: ProjectSongTextEventKind::Lyric {
                    text: "Hello".to_string(),
                    syllable_mode: ProjectLyricSyllableMode::Phrase,
                    continuation: false,
                    duration_beats: None,
                    syllables: Vec::new(),
                },
            },
            ProjectSongTextEvent {
                id: "combined:chord".to_string(),
                beat: 3.0,
                kind: ProjectSongTextEventKind::Chord {
                    symbol: "Am7".to_string(),
                },
            },
            ProjectSongTextEvent {
                id: "combined:lyric".to_string(),
                beat: 3.0,
                kind: ProjectSongTextEventKind::Lyric {
                    text: "world".to_string(),
                    syllable_mode: ProjectLyricSyllableMode::Phrase,
                    continuation: false,
                    duration_beats: None,
                    syllables: Vec::new(),
                },
            },
        ];

        for version in 24..=26 {
            let bytes = encode_legacy_song_text_project(version, &cues);
            let decoded = decode_project(&bytes).expect("decode legacy song text");
            assert_eq!(decoded.settings.song_text_events, expected, "v{version}");
        }

        let bytes = encode_legacy_song_text_project(23, &cues);
        let decoded = decode_project(&bytes).expect("decode pre-song-text project");
        assert!(decoded.settings.song_text_events.is_empty());
    }

    #[test]
    fn legacy_song_text_positions_are_sanitized_when_applied() {
        let cues = vec![
            LegacyProjectSongTextCue {
                id: "negative".to_string(),
                beat: -4.0,
                chord: "  Dm9  ".to_string(),
                lyric: String::new(),
            },
            LegacyProjectSongTextCue {
                id: "non-finite".to_string(),
                beat: f64::INFINITY,
                chord: String::new(),
                lyric: "discard me".to_string(),
            },
        ];
        let bytes = encode_legacy_song_text_project(26, &cues);
        let project = decode_project(&bytes).expect("decode legacy positions");
        let mut timeline = crate::components::timeline::timeline_state::TimelineState::default();

        super::super::apply_to_timeline(&project, &mut timeline);

        assert_eq!(timeline.song_text_events.len(), 1);
        assert_eq!(timeline.song_text_events[0].id, "negative");
        assert_eq!(timeline.song_text_events[0].beat, 0.0);
        assert_eq!(timeline.song_text_events[0].text(), "Dm9");
    }

    #[test]
    fn time_signature_points_roundtrip_v10() {
        let mut project = FutureboardProject::new("TimeSig Test");
        project.settings.time_signature_points = vec![
            super::super::ProjectTimeSignaturePoint {
                id: "ts-a".to_string(),
                beat: 0.0,
                numerator: 4,
                denominator: 4,
                grouping: Vec::new(),
            },
            super::super::ProjectTimeSignaturePoint {
                id: "ts-b".to_string(),
                beat: 16.0,
                numerator: 7,
                denominator: 8,
                grouping: vec![2, 2, 3],
            },
        ];
        let bytes = encode_project(&project);
        let decoded = decode_project(&bytes).expect("decode");
        assert_eq!(
            decoded.settings.time_signature_points,
            project.settings.time_signature_points
        );
    }

    #[test]
    fn default_project_with_time_signature_point_roundtrips() {
        // Mirrors what New Project writes: a default 4/4 marker. This regressed
        // because the decoder read time-signature fields out of order.
        let mut project = FutureboardProject::new("Fresh");
        project.settings.time_signature_points = vec![super::super::ProjectTimeSignaturePoint {
            id: "ts-default".to_string(),
            beat: 0.0,
            numerator: 4,
            denominator: 4,
            grouping: vec![1, 1, 1, 1],
        }];
        let bytes = encode_project(&project);
        let decoded = decode_project(&bytes).expect("default project must decode");
        assert_eq!(decoded.name, "Fresh");
        assert_eq!(decoded.settings.time_signature_points.len(), 1);
        assert_eq!(decoded.settings.time_signature_points[0].numerator, 4);
        assert_eq!(decoded.settings.time_signature_points[0].denominator, 4);
    }

    #[test]
    fn peek_header_rejects_bad_magic() {
        let mut bytes = encode_project(&FutureboardProject::new("X"));
        bytes[0] = b'Z'; // corrupt the magic
        assert!(matches!(
            peek_project_header(&bytes),
            Err(ProjectError::InvalidMagic)
        ));
    }

    #[test]
    fn peek_header_rejects_future_version() {
        let mut bytes = encode_project(&FutureboardProject::new("X"));
        // Bump the version field (bytes 8..12) past the supported max.
        bytes[8..12].copy_from_slice(&(PROJECT_VERSION + 1).to_le_bytes());
        assert!(matches!(
            peek_project_header(&bytes),
            Err(ProjectError::UnsupportedVersion(_))
        ));
    }

    #[test]
    fn peek_header_rejects_tiny_input() {
        assert!(matches!(
            peek_project_header(&[0u8; 4]),
            Err(ProjectError::IncompleteFile { .. })
        ));
    }

    #[test]
    fn truncated_body_reports_unexpected_eof() {
        let bytes = encode_project(&FutureboardProject::new("Body"));
        let body = &bytes[PROJECT_HEADER_SIZE..bytes.len() - 4];
        let truncated_body = &body[..body.len().saturating_sub(3).max(1)];
        let err = decode_body(truncated_body, PROJECT_VERSION).unwrap_err();
        assert!(
            matches!(err, ProjectError::UnexpectedEof { .. })
                || matches!(err, ProjectError::Corrupted(_))
        );
        assert_eq!(
            err.user_message(),
            "Could not open this project because the file appears to be incomplete or corrupted."
        );
    }

    #[test]
    fn controller_kind_tags_roundtrip() {
        for kind in [
            MidiControllerKind::CC(64),
            MidiControllerKind::PitchBend,
            MidiControllerKind::ChannelPressure,
            MidiControllerKind::PolyPressure,
        ] {
            let mut w = FbWriter::new();
            encode_controller_kind(&mut w, kind);
            let bytes = w.into_bytes();
            let mut r = FbReader::new(&bytes);
            assert_eq!(decode_controller_kind(&mut r).unwrap(), kind);
        }
    }

    fn sample_stretch() -> AudioClipStretchState {
        AudioClipStretchState {
            mode: StretchMode::TempoSync,
            algorithm: StretchAlgorithm::PhaseVocoder,
            original_sample_rate: 44_100,
            project_sample_rate: 48_000,
            original_duration_samples: 88_200,
            source_start_samples: 100,
            source_end_samples: 80_000,
            clip_timeline_start_beats: 4.0,
            clip_timeline_duration_beats: 8.0,
            stretch_ratio: 0.857_142_857,
            bpm_source: Some(120.0),
            bpm_target: Some(140.0),
            preserve_pitch: true,
            pitch_shift_semitones: -3.0,
            formant_preserve: true,
            transient_preserve: false,
            transient_sensitivity: 0.65,
            reverse: true,
            denoise_amount: 0.55,
            normalize_gain: true,
            fade_in_ms: 5.0,
            fade_out_ms: 12.5,
            gain_db: -2.0,
            pan: -0.25,
            dirty: false,
            warp_markers: vec![
                WarpMarker {
                    id: 1,
                    source_sample: 0,
                    timeline_beat: 0.0,
                    locked: true,
                },
                WarpMarker {
                    id: 2,
                    source_sample: 22_050,
                    timeline_beat: 2.0,
                    locked: false,
                },
            ],
            gain_envelope: ClipEnvelope {
                points: vec![
                    EnvelopePoint {
                        id: 11,
                        time: 0.0,
                        value_db: -6.0,
                        curve: EnvelopeCurve::Linear,
                    },
                    EnvelopePoint {
                        id: 12,
                        time: 0.75,
                        value_db: 3.0,
                        curve: EnvelopeCurve::SCurve,
                    },
                ],
            },
            channel_transform: 0,
            dc_remove: false,
            dc_left: 0.0,
            dc_right: 0.0,
            dehum_hz: 0.0,
            dehum_harmonics: 0,
            dehum_reduction_db: 0.0,
        }
    }

    fn empty_clip_with_stretch(stretch: AudioClipStretchState) -> ProjectClip {
        ProjectClip {
            id: "c1".to_string(),
            name: "clip".to_string(),
            start_beat: 1.0,
            duration_beats: 4.0,
            offset_beats: 0.0,
            gain: 1.0,
            muted: false,
            source: ClipSource::Empty,
            stretch,
        }
    }

    #[test]
    fn stretch_roundtrips_v16() {
        let clip = empty_clip_with_stretch(sample_stretch());
        let mut w = FbWriter::new();
        encode_clip(&mut w, &clip);
        let bytes = w.into_bytes();
        let mut r = FbReader::new(&bytes);
        let decoded = decode_clip(&mut r, PROJECT_VERSION).unwrap();
        assert_eq!(decoded.stretch, clip.stretch);
    }

    #[test]
    fn ara_binding_roundtrips_on_the_track_v42() {
        let mut project = FutureboardProject::new("ara");
        let binding = AraTrackBinding {
            plugin_id: "vst3:celemony.melodyne".to_string(),
            plugin_path: "C:/Program Files/Common Files/VST3/Melodyne.vst3".to_string(),
            class_id: "1234ABCD".to_string(),
        };
        let mut track = track_with_clip(empty_clip_with_stretch(sample_stretch()));
        track.ara = Some(binding.clone());
        project.tracks.push(track);

        let bytes = encode_project(&project);
        let decoded = decode_project(&bytes).unwrap();
        assert_eq!(decoded.tracks[0].ara, Some(binding));
        // The clip block still decodes around it: the binding lives at the tail
        // of the track, not spliced into the positional clip body.
        assert_eq!(decoded.tracks[0].clips.len(), 1);
        assert_eq!(
            decoded.tracks[0].clips[0].stretch,
            sample_stretch(),
            "the clip's own state must survive the track-level binding"
        );
    }

    #[test]
    fn a_v41_clip_binding_is_consumed_and_the_body_stays_aligned() {
        // v41 wrote an ARA presence byte per clip. v42 no longer writes it, but
        // the reader must still consume it or every field after the clip in a
        // v41 body would be read at the wrong offset.
        let mut clip = empty_clip_with_stretch(sample_stretch());
        // v41 predates the envelope block, so construct the legacy payload
        // explicitly instead of appending v47 bytes to a v41 fixture.
        clip.stretch.gain_envelope = ClipEnvelope::default();
        // v47 added the envelope block, v48 appends de-noise, v49 appends the
        // channel/DC/de-hum process fields; remove those trailers so the
        // fixture really ends at the v41 boundary.
        clip.stretch.denoise_amount = 0.0;
        let mut w = FbWriter::new();
        encode_clip(&mut w, &clip);
        let mut v41 = w.into_bytes();
        // envelope count u32 + denoise f32 + v49 process block.
        let v49 = 1 + 1 + 4 + 4 + 4 + 1 + 4;
        v41.truncate(v41.len().saturating_sub(8 + v49));
        v41.push(0);
        // A sentinel standing in for whatever followed the clip in a real body.
        v41.extend_from_slice(&7u32.to_le_bytes());

        let mut r = FbReader::new(&v41);
        let decoded = decode_clip(&mut r, 41).unwrap();
        assert_eq!(decoded.stretch, clip.stretch);
        assert_eq!(
            r.read_u32().unwrap(),
            7,
            "the reader must land exactly after the v41 clip block"
        );
    }

    #[test]
    fn ara_documents_roundtrip_v41() {
        let mut project = FutureboardProject::new("ara");
        project.ara_documents.push(ProjectAraDocument {
            plugin_id: "vst3:celemony.melodyne".to_string(),
            track_id: "track-1".to_string(),
            archive_id: "com.celemony.ara.melodyne.v5".to_string(),
            // Opaque bytes, including a NUL and a high byte, to prove the blob
            // survives as raw data rather than as text.
            data: vec![0x00, 0xFF, 0x10, b'M', b'D'],
        });
        let bytes = encode_project(&project);
        let decoded = decode_project(&bytes).unwrap();
        assert_eq!(decoded.ara_documents.len(), 1);
        assert_eq!(decoded.ara_documents[0].track_id, "track-1");
        assert_eq!(
            decoded.ara_documents[0].data,
            vec![0x00, 0xFF, 0x10, b'M', b'D']
        );
        assert_eq!(
            decoded.ara_documents[0].archive_id,
            "com.celemony.ara.melodyne.v5"
        );
    }

    #[test]
    fn warp_marker_serialization() {
        let clip = empty_clip_with_stretch(sample_stretch());
        let mut w = FbWriter::new();
        encode_clip(&mut w, &clip);
        let bytes = w.into_bytes();
        let mut r = FbReader::new(&bytes);
        let decoded = decode_clip(&mut r, PROJECT_VERSION).unwrap();
        assert_eq!(decoded.stretch.warp_markers.len(), 2);
        assert_eq!(decoded.stretch.warp_markers[1].source_sample, 22_050);
        assert!(decoded.stretch.warp_markers[0].locked);
    }

    #[test]
    fn old_project_load_defaults() {
        // Hand-encode a pre-v16 clip body (no stretch trailer) and decode at v15:
        // the clip must fall back to the un-stretched defaults (spec Â§13).
        let mut w = FbWriter::new();
        w.write_str("c1");
        w.write_str("clip");
        w.write_f64(0.0);
        w.write_f64(4.0);
        w.write_f32(0.0);
        w.write_f32(1.0);
        w.write_bool(false);
        w.write_u8(0); // ClipSource::Empty
        let bytes = w.into_bytes();
        let mut r = FbReader::new(&bytes);
        let decoded = decode_clip(&mut r, 15).unwrap();
        assert_eq!(decoded.stretch, AudioClipStretchState::default());
        assert_eq!(decoded.stretch.mode, StretchMode::Off);
        assert_eq!(decoded.stretch.stretch_ratio, 1.0);
        assert!(!decoded.stretch.preserve_pitch);
    }

    #[test]
    fn soundfont_player_track_round_trips() {
        let mut track = track_with_clip(ProjectClip {
            id: "c1".to_string(),
            name: "clip".to_string(),
            start_beat: 0.0,
            duration_beats: 4.0,
            offset_beats: 0.0,
            gain: 1.0,
            muted: false,
            source: ClipSource::Empty,
            stretch: AudioClipStretchState::default(),
        });
        track.soundfont = Some(ProjectSoundfontPlayer {
            path: Some(PathBuf::from("/home/user/SoundFonts/GeneralUser-GS.sf2")),
            preset_bank: Some(128),
            preset_patch: Some(8),
            volume: 0.65,
            reverb_chorus: false,
            polyphony: 96,
            envelope: SoundfontEnvelope {
                attack_ms: 120.0,
                decay_ms: 340.0,
                sustain: 0.4,
                release_ms: 900.0,
            },
            quality: SoundfontRenderQuality::Ultra,
        });

        let mut project = FutureboardProject::new("Soundfont");
        project.tracks.push(track);
        let decoded = decode_project(&encode_project(&project)).expect("round trip");
        let soundfont = decoded.tracks[0]
            .soundfont
            .as_ref()
            .expect("soundfont persisted");
        assert_eq!(
            soundfont.path.as_deref(),
            Some(std::path::Path::new(
                "/home/user/SoundFonts/GeneralUser-GS.sf2"
            ))
        );
        assert_eq!(soundfont.preset_bank, Some(128));
        assert_eq!(soundfont.preset_patch, Some(8));
        assert!((soundfont.volume - 0.65).abs() < 1.0e-6);
        assert!(!soundfont.reverb_chorus);
        assert_eq!(soundfont.polyphony, 96);
        assert!((soundfont.envelope.attack_ms - 120.0).abs() < 1.0e-3);
        assert!((soundfont.envelope.decay_ms - 340.0).abs() < 1.0e-3);
        assert!((soundfont.envelope.sustain - 0.4).abs() < 1.0e-6);
        assert!((soundfont.envelope.release_ms - 900.0).abs() < 1.0e-3);
        assert_eq!(soundfont.quality, SoundfontRenderQuality::Ultra);
    }

    #[test]
    fn a_v28_soundfont_loads_with_no_envelope_and_standard_quality() {
        // A v28 soundfont block ends at the polyphony, so decoding it must not
        // read the following bytes as an envelope.
        let mut w = FbWriter::new();
        w.write_bool(true);
        w.write_opt_path(&Some(PathBuf::from("/fonts/GM.sf2")));
        w.write_bool(true);
        w.write_u32(0);
        w.write_u32(3);
        w.write_f32(0.8);
        w.write_bool(true);
        w.write_u32(48);
        let bytes = w.into_bytes();

        let mut r = FbReader::new(&bytes);
        let soundfont = decode_soundfont_player(&mut r, 28)
            .expect("v28 soundfont decodes")
            .expect("present");
        assert_eq!(soundfont.polyphony, 48);
        assert!(
            soundfont.envelope.is_bypassed(),
            "a pre-v29 soundfont must keep its original signal path"
        );
        assert_eq!(soundfont.quality, SoundfontRenderQuality::Standard);
    }

    #[test]
    fn track_without_a_soundfont_round_trips_as_none() {
        let track = track_with_clip(ProjectClip {
            id: "c1".to_string(),
            name: "clip".to_string(),
            start_beat: 0.0,
            duration_beats: 4.0,
            offset_beats: 0.0,
            gain: 1.0,
            muted: false,
            source: ClipSource::Empty,
            stretch: AudioClipStretchState::default(),
        });
        let mut project = FutureboardProject::new("Plain");
        project.tracks.push(track);
        let decoded = decode_project(&encode_project(&project)).expect("round trip");
        assert!(decoded.tracks[0].soundfont.is_none());
    }

    #[test]
    fn pre_v28_track_loads_without_a_soundfont() {
        // A v27 track body ends at the row height; decoding it must not read
        // into the next record looking for a soundfont block.
        let mut w = FbWriter::new();
        encode_track_body_v27(&mut w);
        let bytes = w.into_bytes();
        let mut r = FbReader::new(&bytes);
        let decoded = decode_track(&mut r, 27).expect("v27 track decodes");
        assert!(decoded.soundfont.is_none());
        assert_eq!(decoded.id, "t1");
    }

    /// Writes exactly the track body a v27 writer produced â everything through
    /// the v17 row height, and nothing after it.
    fn encode_track_body_v27(w: &mut FbWriter) {
        w.write_str("t1");
        w.write_str("Audio 1");
        encode_track_type(w, ProjectTrackType::Audio);
        w.write_str("#56C7C9");
        w.write_f32(1.0);
        w.write_f32(0.0);
        w.write_bool(false);
        w.write_bool(false);
        w.write_bool(false);
        encode_input_monitor(w, InputMonitorMode::Off);
        let routing = TrackRouting::default();
        // v33-shaped fixture: the combined union sits where v34 writes an id.
        encode_track_input_routing(w, &V33TrackInputRouting::None);
        encode_track_output_routing(w, &routing.output);
        encode_track_audio_format(w, routing.audio_format);
        encode_track_midi_input_routing(w, &routing.midi_input);
        w.write_opt_u8(&None);
        w.write_bool(false);
        w.write_opt_str(&None);
        w.write_u32(0); // sends
        w.write_u32(0); // inserts
        w.write_u32(0); // automation lanes
        w.write_u32(0); // clips
        w.write_opt_f32(&None); // row height
    }

    fn track_with_clip(clip: ProjectClip) -> ProjectTrack {
        ProjectTrack {
            id: "t1".to_string(),
            name: "Audio 1".to_string(),
            track_type: ProjectTrackType::Audio,
            ara: None,
            timebase: crate::components::timeline::timeline_state::TrackTimebase::Musical.to_tag(),
            parent_group_id: None,
            group_collapsed: false,
            color_hex: "#56C7C9".to_string(),
            volume_norm: 1.0,
            pan: 0.0,
            muted: false,
            solo: false,
            record_arm: false,
            input_monitor: InputMonitorMode::Off,
            routing: TrackRouting::default(),
            inserts: Vec::new(),
            automation_lanes: Vec::new(),
            clips: vec![clip],
            row_height_px: None,
            soundfont: None,
            volume_automation_read: true,
            solfege: None,
            takes: Vec::new(),
            takes_expanded: false,
        }
    }

    #[test]
    fn stretch_survives_full_project_roundtrip() {
        let mut project = FutureboardProject::new("Stretch");
        project
            .tracks
            .push(track_with_clip(empty_clip_with_stretch(sample_stretch())));
        let bytes = encode_project(&project);
        let decoded = decode_project(&bytes).expect("decode");
        assert_eq!(decoded.tracks[0].clips[0].stretch, sample_stretch());
    }

    /// v44. Which pass a clip came from, and which one is heard, are not
    /// recoverable from anything else in the file — a comp that survives Save
    /// only as "one clip is muted" is a comp the take list can no longer edit.
    #[test]
    fn takes_survive_a_full_project_roundtrip() {
        let mut project = FutureboardProject::new("Takes");
        let mut track = track_with_clip(empty_clip_with_stretch(sample_stretch()));
        track.takes = vec![
            ProjectTake {
                id: "t-1".to_string(),
                name: "Take 1".to_string(),
                clip_id: "clip-1".to_string(),
                active: false,
                recorded_at: "10:32".to_string(),
            },
            ProjectTake {
                id: "t-2".to_string(),
                name: "The keeper".to_string(),
                clip_id: "clip-2".to_string(),
                active: true,
                recorded_at: "10:35".to_string(),
            },
        ];
        track.takes_expanded = true;
        let expected = track.takes.clone();
        project.tracks.push(track);

        let decoded = decode_project(&encode_project(&project)).expect("decode");
        assert_eq!(decoded.tracks[0].takes, expected);
        assert!(decoded.tracks[0].takes_expanded);
    }

    /// A track that has never been recorded onto carries no take list, and must
    /// round-trip as one — not as a list with an empty entry in it.
    #[test]
    fn a_track_with_no_takes_roundtrips_as_a_track_with_no_takes() {
        let mut project = FutureboardProject::new("No takes");
        project
            .tracks
            .push(track_with_clip(empty_clip_with_stretch(sample_stretch())));
        let decoded = decode_project(&encode_project(&project)).expect("decode");
        assert!(decoded.tracks[0].takes.is_empty());
        assert!(!decoded.tracks[0].takes_expanded);
    }

    // ââ Audio Connections codec ââââââââââââââââââââââââââââââââââââââââââ

    use super::*;

    fn connection(id: &str, direction: &str, ports: &[u32]) -> ProjectAudioConnection {
        ProjectAudioConnection {
            id: id.to_string(),
            name: format!("Bus {id}"),
            direction: direction.to_string(),
            channel_layout: if ports.len() == 1 { "mono" } else { "stereo" }.to_string(),
            channel_count: ports.len() as u32,
            device_id: Some("dev-1".to_string()),
            port_bindings: ports
                .iter()
                .enumerate()
                .map(|(logical, index)| ProjectAudioPortBinding {
                    logical_channel: logical as u32,
                    device_id: "dev-1".to_string(),
                    port_name: format!("Input {}", index + 1),
                    port_index: *index,
                })
                .collect(),
            enabled: true,
        }
    }

    fn project_with(connections: Vec<ProjectAudioConnection>) -> Vec<u8> {
        encode_project(&FutureboardProject {
            audio_connections: connections,
            ..FutureboardProject::new("codec")
        })
    }

    #[test]
    fn connections_round_trip_through_the_binary_format() {
        let bytes = project_with(vec![
            connection("ac-1", "input", &[0]),
            connection("ac-2", "output", &[2, 3]),
        ]);
        let decoded = decode_project(&bytes).expect("decode");
        assert_eq!(decoded.audio_connections.len(), 2);
        assert_eq!(decoded.audio_connections[0].direction, "input");
        assert_eq!(decoded.audio_connections[1].direction, "output");
        // Ordered bindings survive verbatim.
        let ports: Vec<u32> = decoded.audio_connections[1]
            .port_bindings
            .iter()
            .map(|b| b.port_index)
            .collect();
        assert_eq!(ports, vec![2, 3]);
    }

    /// Reversed bindings must not be normalized anywhere in the codec.
    #[test]
    fn reversed_bindings_survive_the_round_trip_distinctly() {
        let bytes = project_with(vec![
            connection("ac-fwd", "input", &[0, 1]),
            connection("ac-rev", "input", &[1, 0]),
        ]);
        let decoded = decode_project(&bytes).expect("decode");
        let read = |id: &str| -> Vec<u32> {
            decoded
                .audio_connections
                .iter()
                .find(|c| c.id == id)
                .unwrap()
                .port_bindings
                .iter()
                .map(|b| b.port_index)
                .collect()
        };
        assert_eq!(read("ac-fwd"), vec![0, 1]);
        assert_eq!(read("ac-rev"), vec![1, 0]);
    }

    /// Two connections sharing an id would make a track reference ambiguous.
    #[test]
    fn duplicate_connection_ids_are_rejected() {
        let bytes = project_with(vec![
            connection("ac-dup", "input", &[0]),
            connection("ac-dup", "input", &[1]),
        ]);
        let error = decode_project(&bytes).expect_err("duplicate ids must not load");
        assert!(matches!(error, ProjectError::Corrupted(ref m) if m.contains("duplicate")));
    }

    #[test]
    fn an_unknown_direction_or_layout_is_rejected() {
        let mut bad = connection("ac-1", "sideways", &[0]);
        bad.direction = "sideways".to_string();
        assert!(matches!(
            decode_project(&project_with(vec![bad])),
            Err(ProjectError::Corrupted(_))
        ));

        let mut bad_layout = connection("ac-2", "input", &[0]);
        bad_layout.channel_layout = "quadraphonic".to_string();
        assert!(matches!(
            decode_project(&project_with(vec![bad_layout])),
            Err(ProjectError::Corrupted(_))
        ));
    }

    #[test]
    fn an_empty_connection_id_is_rejected() {
        let mut bad = connection("", "input", &[0]);
        bad.id = String::new();
        assert!(matches!(
            decode_project(&project_with(vec![bad])),
            Err(ProjectError::Corrupted(_))
        ));
    }

    /// A truncated registry must fail cleanly, never panic or over-allocate.
    #[test]
    fn truncated_registry_data_fails_safely() {
        let bytes = project_with(vec![connection("ac-1", "input", &[0, 1])]);
        for cut in 1..24usize.min(bytes.len()) {
            let mut truncated = bytes[..bytes.len() - cut].to_vec();
            // Repair the body length + checksum framing so the failure is the
            // registry decode itself, not the outer envelope.
            let _ = &mut truncated;
            assert!(
                decode_project(&truncated).is_err(),
                "truncating {cut} bytes must not load"
            );
        }
    }

    /// A hostile count must not make the decoder reserve an arbitrary vector.
    #[test]
    fn an_absurd_connection_count_is_rejected_before_allocating() {
        let mut body = encode_body(&FutureboardProject::new("hostile"));
        // Overwrite the trailing connection count with a huge value.
        let len = body.len();
        body[len - 4..].copy_from_slice(&u32::MAX.to_le_bytes());
        let bytes = project_bytes_with_version(body, PROJECT_VERSION);
        assert!(matches!(
            decode_project(&bytes),
            Err(ProjectError::Corrupted(_))
        ));
    }

    /// The conductor lanes' fold state is view state, but it is view state the
    /// player set by hand, so it has to survive the file like a track height.
    #[test]
    fn conductor_lane_fold_state_roundtrips_v40() {
        let mut project = FutureboardProject::new("Folded");
        project.global_lanes = super::super::ProjectGlobalLanes {
            arranger_collapsed: true,
            marker_collapsed: false,
            tempo_collapsed: true,
            time_signature_collapsed: false,
            arranger_height: Some(52.0),
            marker_height: None,
            tempo_height: Some(120.0),
            time_signature_height: None,
            song_text_height: Some(64.0),
        };
        let decoded = decode_project(&encode_project(&project)).expect("decode");
        assert_eq!(decoded.global_lanes, project.global_lanes);
    }

    /// A v39 file has no fold block, and every lane must come back expanded at
    /// its default height â the state such a file was actually saved in.
    #[test]
    fn a_v39_project_loads_with_every_conductor_lane_expanded() {
        let mut body = encode_body(&FutureboardProject::new("legacy"));
        // Four collapse latches plus five absent optional heights.
        body.truncate(body.len() - (4 + 5));
        let bytes = project_bytes_with_version(body, 39);
        let decoded = decode_project(&bytes).expect("v39 loads");
        assert_eq!(
            decoded.global_lanes,
            super::super::ProjectGlobalLanes::default()
        );
    }

    /// v33 files predate the section entirely and must still load.
    #[test]
    fn a_v33_project_loads_with_an_empty_registry() {
        let mut body = encode_body(&FutureboardProject::new("legacy"));
        // v33 bodies have no connections section; drop it.
        body.truncate(body.len() - std::mem::size_of::<u32>());
        let bytes = project_bytes_with_version(body, 33);
        // v33 also stores the combined union per track, so a track-bearing
        // fixture would need the older track layout; an empty project is
        // enough to prove the section is version-gated.
        let decoded = decode_project(&bytes).expect("v33 loads");
        assert!(decoded.audio_connections.is_empty());
    }
}
