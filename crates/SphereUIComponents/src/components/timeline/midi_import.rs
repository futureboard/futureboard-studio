use std::collections::HashMap;

use super::timeline_state::{
    MidiChannel, MidiControllerKind, MidiControllerLane, MidiControllerPoint, MidiNoteState,
};

#[derive(Debug, Clone, PartialEq)]
pub struct ImportedMidiClip {
    pub notes: Vec<MidiNoteState>,
    pub controller_lanes: Vec<MidiControllerLane>,
    pub sysex_events: Vec<ImportedSysExEvent>,
    pub markers: Vec<ImportedMidiMarker>,
    pub duration_beats: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImportedMidiTrack {
    pub name: Option<String>,
    pub channel_hint: Option<MidiChannel>,
    pub clip: ImportedMidiClip,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportedSysExKind {
    Normal,
    Escaped,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImportedSysExEvent {
    pub kind: ImportedSysExKind,
    pub absolute_tick: u64,
    pub beat: f32,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImportedMidiMarker {
    pub text: String,
    pub absolute_tick: u64,
    pub beat: f32,
}

/// What an import brings in besides the notes.
///
/// Notes are never optional — everything here is payload a file may carry in
/// bulk (a type-0 song file routinely ships hundreds of markers and a dense
/// CC/bend stream), which the import dialog offers per file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MidiImportOptions {
    pub include_markers: bool,
    /// CC, pitch bend, and channel pressure lanes.
    pub include_controllers: bool,
    pub include_sysex: bool,
}

impl Default for MidiImportOptions {
    /// Everything — the behavior before the dialog existed.
    fn default() -> Self {
        Self {
            include_markers: true,
            include_controllers: true,
            include_sysex: true,
        }
    }
}

impl MidiImportOptions {
    pub const NOTES_ONLY: Self = Self {
        include_markers: false,
        include_controllers: false,
        include_sysex: false,
    };
}

/// How much of each optional payload a parsed file carries, so the import
/// dialog can name the amount instead of asking about an unknown quantity.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MidiImportSummary {
    pub tracks: usize,
    pub notes: usize,
    pub markers: usize,
    pub controller_lanes: usize,
    pub controller_points: usize,
    pub sysex_events: usize,
}

impl MidiImportSummary {
    pub fn of(tracks: &[ImportedMidiTrack]) -> Self {
        let mut summary = Self {
            tracks: tracks.len(),
            ..Self::default()
        };
        for track in tracks {
            summary.notes += track.clip.notes.len();
            summary.markers += track.clip.markers.len();
            summary.controller_lanes += track.clip.controller_lanes.len();
            summary.controller_points += track
                .clip
                .controller_lanes
                .iter()
                .map(|lane| lane.points.len())
                .sum::<usize>();
            summary.sysex_events += track.clip.sysex_events.len();
        }
        summary
    }

    /// True when the file carries something worth asking about. A plain
    /// note-only export has nothing optional in it, so it still imports on the
    /// drop without a dialog in the way.
    pub fn has_optional_payload(&self) -> bool {
        self.markers > 0 || self.controller_lanes > 0 || self.sysex_events > 0
    }
}

/// Drop the payload the user turned off. Applied after parsing so the same
/// parse answers both the dialog's counts and the import itself.
pub fn apply_import_options(tracks: &mut [ImportedMidiTrack], options: MidiImportOptions) {
    for track in tracks {
        if !options.include_markers {
            track.clip.markers.clear();
        }
        if !options.include_controllers {
            track.clip.controller_lanes.clear();
        }
        if !options.include_sysex {
            track.clip.sysex_events.clear();
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MidiImportError {
    Truncated(&'static str),
    InvalidHeader,
    UnsupportedDivision,
    UnsupportedFormat(u16),
}

impl std::fmt::Display for MidiImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Truncated(section) => write!(f, "truncated MIDI {section}"),
            Self::InvalidHeader => write!(f, "invalid MIDI header"),
            Self::UnsupportedDivision => write!(f, "SMPTE MIDI timing is not supported yet"),
            Self::UnsupportedFormat(format) => write!(f, "unsupported MIDI format {format}"),
        }
    }
}

impl std::error::Error for MidiImportError {}

pub fn parse_smf_notes(data: &[u8]) -> Result<ImportedMidiClip, MidiImportError> {
    let tracks = parse_smf_tracks(data)?;
    let mut merged = empty_imported_clip();
    for track in tracks {
        merged.notes.extend(track.clip.notes);
        merge_controller_lanes(&mut merged.controller_lanes, track.clip.controller_lanes);
        merged.sysex_events.extend(track.clip.sysex_events);
        merged.markers.extend(track.clip.markers);
        merged.duration_beats = merged.duration_beats.max(track.clip.duration_beats);
    }
    finalize_imported_clip(&mut merged, 1);
    Ok(merged)
}

pub fn parse_smf_tracks(data: &[u8]) -> Result<Vec<ImportedMidiTrack>, MidiImportError> {
    let mut r = Reader::new(data);
    if r.read_exact(4)? != b"MThd" {
        return Err(MidiImportError::InvalidHeader);
    }
    let header_len = r.read_u32()? as usize;
    if header_len < 6 {
        return Err(MidiImportError::InvalidHeader);
    }
    let format = r.read_u16()?;
    let track_count = r.read_u16()?;
    let division = r.read_u16()?;
    if format > 1 {
        return Err(MidiImportError::UnsupportedFormat(format));
    }
    if division & 0x8000 != 0 {
        return Err(MidiImportError::UnsupportedDivision);
    }
    let ticks_per_beat = (division as u32).max(1);
    r.skip(header_len - 6)?;

    let mut imported_tracks = Vec::new();
    let mut tracks_read = 0u16;
    while tracks_read < track_count {
        if r.remaining() < 8 {
            break;
        }
        let chunk_type = r.read_exact(4)?;
        let len = r.read_u32()? as usize;
        // SMF readers skip a chunk they do not recognize and keep going. A
        // sequencer-specific chunk before or between tracks is not a broken
        // file, and rejecting it used to fail the drop with no status text.
        if chunk_type != b"MTrk" {
            r.skip(len)?;
            continue;
        }
        let track_data = r.read_exact(len)?;
        tracks_read = tracks_read.saturating_add(1);
        let mut clip = empty_imported_clip();
        let mut channel_lanes = ChannelControllerLanes::new();
        let mut track_name = None;
        let mut max_tick = 0u64;
        parse_track(
            track_data,
            ticks_per_beat,
            &mut clip.notes,
            &mut channel_lanes,
            &mut clip.sysex_events,
            &mut clip.markers,
            &mut max_tick,
            &mut track_name,
        )?;
        clip.duration_beats = ticks_to_beats(max_tick, ticks_per_beat) as f32;
        let track_name = track_name.filter(|name| !name.is_empty());
        imported_tracks.extend(split_imported_track_by_channel(
            track_name,
            clip,
            &channel_lanes,
        ));
    }

    Ok(imported_tracks)
}

fn empty_imported_clip() -> ImportedMidiClip {
    ImportedMidiClip {
        notes: Vec::new(),
        controller_lanes: Vec::new(),
        sysex_events: Vec::new(),
        markers: Vec::new(),
        duration_beats: 0.0,
    }
}

fn finalize_imported_clip(clip: &mut ImportedMidiClip, _ticks_per_beat: u32) {
    clip.notes.sort_by(|a, b| {
        a.start
            .total_cmp(&b.start)
            .then(a.pitch.cmp(&b.pitch))
            .then(a.id.cmp(&b.id))
    });
    clip.controller_lanes.retain(|lane| !lane.points.is_empty());
    clip.controller_lanes
        .sort_by(|a, b| controller_kind_sort_key(a.kind).cmp(&controller_kind_sort_key(b.kind)));
    let note_end = clip
        .notes
        .iter()
        .map(|note| note.start + note.duration)
        .fold(0.0_f32, f32::max);
    let controller_end = clip
        .controller_lanes
        .iter()
        .flat_map(|lane| lane.points.iter().map(|point| point.beat))
        .fold(0.0_f32, f32::max);
    clip.duration_beats = clip.duration_beats.max(note_end).max(controller_end);
}

/// Controller lanes gathered per source MIDI channel while one MTrk is parsed.
///
/// A clip's lane model is channel-less, so a type-0 file that packs several
/// channels into a single MTrk has to keep the streams apart *here*. Handing
/// every split clip the whole track's controller data gave each one every other
/// channel's pitch bend, which detunes the notes it does own — the reason a
/// dragged-in multi-channel file played out of tune.
struct ChannelControllerLanes {
    per_channel: [Vec<MidiControllerLane>; MidiChannel::COUNT as usize],
}

impl ChannelControllerLanes {
    fn new() -> Self {
        Self {
            per_channel: std::array::from_fn(|_| Vec::new()),
        }
    }

    fn push(&mut self, channel: u8, kind: MidiControllerKind, beat: f64, value: f32) {
        let lanes = &mut self.per_channel[(channel & 0x0f) as usize];
        push_controller_point(lanes, kind, beat, value);
    }

    /// Only this channel's lanes — what a channel-split clip owns.
    fn for_channel(&self, channel: MidiChannel) -> Vec<MidiControllerLane> {
        self.per_channel[channel.raw() as usize].clone()
    }

    /// Every channel folded together. Used only when the track has no notes to
    /// attribute the streams to (a conductor / controller-only track), where
    /// dropping them would lose the data outright.
    fn merged(&self) -> Vec<MidiControllerLane> {
        let mut merged = Vec::new();
        for lanes in &self.per_channel {
            merge_controller_lanes(&mut merged, lanes.clone());
        }
        merged
    }
}

fn split_imported_track_by_channel(
    track_name: Option<String>,
    clip: ImportedMidiClip,
    channel_lanes: &ChannelControllerLanes,
) -> Vec<ImportedMidiTrack> {
    let mut channels: Vec<MidiChannel> = Vec::new();
    for note in &clip.notes {
        if !channels.contains(&note.channel) {
            channels.push(note.channel);
        }
    }
    channels.sort_by_key(|channel| channel.raw());

    if channels.len() <= 1 {
        let mut clip = clip;
        clip.controller_lanes = match channels.first() {
            Some(channel) => channel_lanes.for_channel(*channel),
            None => channel_lanes.merged(),
        };
        finalize_imported_clip(&mut clip, 1);
        return vec![ImportedMidiTrack {
            name: track_name,
            channel_hint: channels.first().copied(),
            clip,
        }];
    }

    channels
        .into_iter()
        .enumerate()
        .map(|(index, channel)| {
            let mut channel_clip = ImportedMidiClip {
                notes: clip
                    .notes
                    .iter()
                    .filter(|note| note.channel == channel)
                    .cloned()
                    .collect(),
                controller_lanes: channel_lanes.for_channel(channel),
                // SysEx and markers belong to the source track once, not once
                // per channel it happens to carry. Copying them onto every
                // split clip is what turned a handful of markers in a type-0
                // file into a wall of them on the ruler.
                sysex_events: if index == 0 {
                    clip.sysex_events.clone()
                } else {
                    Vec::new()
                },
                markers: if index == 0 {
                    clip.markers.clone()
                } else {
                    Vec::new()
                },
                duration_beats: clip.duration_beats,
            };
            finalize_imported_clip(&mut channel_clip, 1);
            ImportedMidiTrack {
                name: track_name
                    .as_ref()
                    .map(|name| format!("{} Ch {}", name, channel.ui())),
                channel_hint: Some(channel),
                clip: channel_clip,
            }
        })
        .collect()
}

fn merge_controller_lanes(target: &mut Vec<MidiControllerLane>, lanes: Vec<MidiControllerLane>) {
    for lane in lanes {
        if lane.points.is_empty() {
            continue;
        }
        if let Some(existing) = target.iter_mut().find(|item| item.kind == lane.kind) {
            existing.points =
                merge_sorted_points(std::mem::take(&mut existing.points), lane.points);
        } else {
            target.push(lane);
        }
    }
}

/// Merge two beat-sorted lanes in one pass. The same beat keeps the incoming
/// point, which is what a later track's event used to do when it overwrote.
fn merge_sorted_points(
    left: Vec<MidiControllerPoint>,
    right: Vec<MidiControllerPoint>,
) -> Vec<MidiControllerPoint> {
    let mut merged = Vec::with_capacity(left.len() + right.len());
    let mut left = left.into_iter().peekable();
    let mut right = right.into_iter().peekable();
    loop {
        match (left.peek(), right.peek()) {
            (None, None) => break,
            (Some(_), None) => {
                merged.extend(left);
                break;
            }
            (None, Some(_)) => {
                merged.extend(right);
                break;
            }
            (Some(a), Some(b)) => match a.beat.total_cmp(&b.beat) {
                std::cmp::Ordering::Less => merged.push(left.next().expect("peeked")),
                std::cmp::Ordering::Greater => merged.push(right.next().expect("peeked")),
                std::cmp::Ordering::Equal => {
                    left.next();
                    merged.push(right.next().expect("peeked"));
                }
            },
        }
    }
    merged
}

fn parse_track(
    data: &[u8],
    ticks_per_beat: u32,
    notes: &mut Vec<MidiNoteState>,
    controller_lanes: &mut ChannelControllerLanes,
    sysex_events: &mut Vec<ImportedSysExEvent>,
    markers: &mut Vec<ImportedMidiMarker>,
    max_tick: &mut u64,
    track_name: &mut Option<String>,
) -> Result<(), MidiImportError> {
    let mut r = Reader::new(data);
    let mut tick = 0u64;
    let mut running_status: Option<u8> = None;
    let mut active: HashMap<(u8, u8), Vec<(u64, u8)>> = HashMap::new();

    while r.remaining() > 0 {
        tick = tick.saturating_add(r.read_vlq()? as u64);
        *max_tick = (*max_tick).max(tick);
        let first = r.read_u8()?;
        let status = if first & 0x80 != 0 {
            first
        } else if let Some(status) = running_status {
            r.unread_one();
            status
        } else {
            return Err(MidiImportError::InvalidHeader);
        };

        match status {
            0x80..=0x9f => {
                running_status = Some(status);
                let pitch = r.read_u8()?.min(127);
                let velocity = r.read_u8()?.min(127);
                let channel = status & 0x0f;
                if status & 0xf0 == 0x90 && velocity > 0 {
                    active
                        .entry((channel, pitch))
                        .or_default()
                        .push((tick, velocity));
                } else if let Some(starts) = active.get_mut(&(channel, pitch)) {
                    if let Some((start_tick, start_velocity)) = starts.pop() {
                        push_note(
                            notes,
                            ticks_per_beat,
                            pitch,
                            start_tick,
                            tick,
                            start_velocity,
                            channel,
                        );
                    }
                }
            }
            0xa0..=0xaf | 0xb0..=0xbf | 0xe0..=0xef => {
                running_status = Some(status);
                let data1 = r.read_u8()?;
                let data2 = r.read_u8()?;
                let channel = status & 0x0f;
                match status & 0xf0 {
                    0xb0 => controller_lanes.push(
                        channel,
                        MidiControllerKind::CC(data1.min(127)),
                        ticks_to_beats(tick, ticks_per_beat),
                        data2.min(127) as f32 / 127.0,
                    ),
                    0xe0 => {
                        let value14 = ((data2 as u16) << 7) | data1 as u16;
                        controller_lanes.push(
                            channel,
                            MidiControllerKind::PitchBend,
                            ticks_to_beats(tick, ticks_per_beat),
                            value14 as f32 / 16383.0,
                        );
                    }
                    // Poly pressure needs per-note association. The current lane
                    // model has only one normalized stream, so preserve the data
                    // model contract by not importing it as a misleading global
                    // lane yet.
                    _ => {}
                }
            }
            0xc0..=0xdf => {
                running_status = Some(status);
                let data = r.read_u8()?;
                if status & 0xf0 == 0xd0 {
                    controller_lanes.push(
                        status & 0x0f,
                        MidiControllerKind::ChannelPressure,
                        ticks_to_beats(tick, ticks_per_beat),
                        data.min(127) as f32 / 127.0,
                    );
                }
            }
            0xff => {
                running_status = None;
                let meta_type = r.read_u8()?;
                let len = r.read_vlq()? as usize;
                let payload = r.read_exact(len)?;
                if meta_type == 0x03 {
                    let name = decode_midi_text(payload);
                    if !name.is_empty() {
                        *track_name = Some(name);
                    }
                }
                if meta_type == 0x06 {
                    markers.push(ImportedMidiMarker {
                        text: decode_midi_text(payload),
                        absolute_tick: tick,
                        beat: ticks_to_beats(tick, ticks_per_beat) as f32,
                    });
                }
                if meta_type == 0x2f {
                    break;
                }
            }
            0xf0 | 0xf7 => {
                running_status = None;
                let len = r.read_vlq()? as usize;
                let payload = r.read_exact(len)?;
                sysex_events.push(ImportedSysExEvent {
                    kind: if status == 0xf0 {
                        ImportedSysExKind::Normal
                    } else {
                        ImportedSysExKind::Escaped
                    },
                    absolute_tick: tick,
                    beat: ticks_to_beats(tick, ticks_per_beat) as f32,
                    data: payload.to_vec(),
                });
            }
            _ => return Err(MidiImportError::InvalidHeader),
        }
    }

    Ok(())
}

fn decode_midi_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_string()
}

fn ticks_to_beats(tick: u64, ticks_per_beat: u32) -> f64 {
    // Divide in f64 so the quotient is exact before it is stored. Stored
    // beats are still `f32`, so this does not keep every tick apart: at 9600
    // ticks per quarter, neighbouring ticks round to the same f32 beat beyond
    // about beat 1024 and then merge. Keeping them apart there needs a wider
    // stored type, which is a separate project-format change.
    tick as f64 / f64::from(ticks_per_beat.max(1))
}

/// Append one controller event. Track data is in tick order, so this is
/// amortized O(1): replace the last point when this tick lands on the same
/// stored (`f32`) beat, otherwise push. A fixed 1e-3-beat window used to merge
/// neighboring ticks in high-PPQ files, and sorting the whole lane on every
/// event froze the UI on a dense controller stream.
fn push_controller_point(
    lanes: &mut Vec<MidiControllerLane>,
    kind: MidiControllerKind,
    beat: f64,
    value: f32,
) {
    let beat = beat.max(0.0) as f32;
    let value = value.clamp(0.0, 1.0);
    let Some(lane) = lanes.iter_mut().find(|lane| lane.kind == kind) else {
        lanes.push(MidiControllerLane {
            kind,
            points: vec![MidiControllerPoint::new(beat, value)],
            visible: true,
            height: 80.0,
            collapsed: false,
        });
        return;
    };
    enum Place {
        Replace,
        Append,
        Insert,
    }
    let place = match lane.points.last() {
        Some(last) if last.beat == beat => Place::Replace,
        Some(last) if last.beat < beat => Place::Append,
        None => Place::Append,
        Some(_) => Place::Insert,
    };
    match place {
        Place::Replace => {
            if let Some(last) = lane.points.last_mut() {
                last.value = value;
            }
        }
        Place::Append => lane.points.push(MidiControllerPoint::new(beat, value)),
        Place::Insert => {
            let index = lane.points.partition_point(|point| point.beat < beat);
            if index < lane.points.len() && lane.points[index].beat == beat {
                lane.points[index].value = value;
            } else {
                lane.points
                    .insert(index, MidiControllerPoint::new(beat, value));
            }
        }
    }
}

fn controller_kind_sort_key(kind: MidiControllerKind) -> (u8, u8) {
    match kind {
        MidiControllerKind::CC(n) => (0, n),
        MidiControllerKind::PitchBend => (1, 0),
        MidiControllerKind::ChannelPressure => (2, 0),
        MidiControllerKind::PolyPressure => (3, 0),
    }
}

fn push_note(
    notes: &mut Vec<MidiNoteState>,
    ticks_per_beat: u32,
    pitch: u8,
    start_tick: u64,
    end_tick: u64,
    velocity: u8,
    channel: u8,
) {
    if end_tick <= start_tick {
        return;
    }
    let start = start_tick as f32 / ticks_per_beat as f32;
    let duration = (end_tick - start_tick) as f32 / ticks_per_beat as f32;
    let mut note = MidiNoteState::new(pitch, start, duration, velocity.max(1));
    note.channel = MidiChannel::from_raw(channel);
    notes.push(note);
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    fn read_exact(&mut self, len: usize) -> Result<&'a [u8], MidiImportError> {
        if self.remaining() < len {
            return Err(MidiImportError::Truncated("chunk"));
        }
        let start = self.pos;
        self.pos += len;
        Ok(&self.data[start..self.pos])
    }

    fn read_u8(&mut self) -> Result<u8, MidiImportError> {
        Ok(*self
            .read_exact(1)?
            .first()
            .ok_or(MidiImportError::Truncated("byte"))?)
    }

    fn read_u16(&mut self) -> Result<u16, MidiImportError> {
        let b = self.read_exact(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    fn read_u32(&mut self) -> Result<u32, MidiImportError> {
        let b = self.read_exact(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn read_vlq(&mut self) -> Result<u32, MidiImportError> {
        let mut value = 0u32;
        for _ in 0..4 {
            let byte = self.read_u8()?;
            value = (value << 7) | (byte & 0x7f) as u32;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(MidiImportError::InvalidHeader)
    }

    fn skip(&mut self, len: usize) -> Result<(), MidiImportError> {
        self.read_exact(len).map(|_| ())
    }

    fn unread_one(&mut self) {
        self.pos = self.pos.saturating_sub(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn smf(track: &[u8]) -> Vec<u8> {
        smf_format(0, &[track])
    }

    fn smf_format(format: u16, tracks: &[&[u8]]) -> Vec<u8> {
        smf_division(format, 480, tracks)
    }

    fn smf_division(format: u16, division: u16, tracks: &[&[u8]]) -> Vec<u8> {
        let mut data = vec![b'M', b'T', b'h', b'd', 0, 0, 0, 6];
        data.extend_from_slice(&format.to_be_bytes());
        data.extend_from_slice(&(tracks.len() as u16).to_be_bytes());
        data.extend_from_slice(&division.to_be_bytes());
        for track in tracks {
            data.extend_from_slice(b"MTrk");
            data.extend_from_slice(&(track.len() as u32).to_be_bytes());
            data.extend_from_slice(track);
        }
        data
    }

    #[test]
    fn parses_note_on_off_track() {
        let data = smf(&[0, 0x90, 60, 100, 0x83, 0x60, 0x80, 60, 0, 0, 0xff, 0x2f, 0]);
        let imported = parse_smf_notes(&data).unwrap();
        assert_eq!(imported.notes.len(), 1);
        assert_eq!(imported.notes[0].pitch, 60);
        assert_eq!(imported.notes[0].velocity, 100);
        assert!((imported.notes[0].duration - 1.0).abs() < 1.0e-4);
    }

    #[test]
    fn parses_format_one_tracks_separately() {
        let track_a = [
            0, 0xff, 0x03, 5, b'P', b'i', b'a', b'n', b'o', 0, 0x90, 60, 100, 0x83, 0x60, 0x80, 60,
            0, 0, 0xff, 0x2f, 0,
        ];
        let track_b = [
            0, 0xff, 0x03, 4, b'B', b'a', b's', b's', 0, 0x91, 48, 96, 0x83, 0x60, 0x81, 48, 0, 0,
            0xff, 0x2f, 0,
        ];
        let data = smf_format(1, &[&track_a, &track_b]);
        let tracks = parse_smf_tracks(&data).unwrap();

        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].name.as_deref(), Some("Piano"));
        assert_eq!(tracks[1].name.as_deref(), Some("Bass"));
        assert_eq!(tracks[0].clip.notes.len(), 1);
        assert_eq!(tracks[1].clip.notes.len(), 1);
        assert_eq!(tracks[0].clip.notes[0].pitch, 60);
        assert_eq!(tracks[1].clip.notes[0].pitch, 48);

        let merged = parse_smf_notes(&data).unwrap();
        assert_eq!(merged.notes.len(), 2);
    }

    #[test]
    fn splits_single_track_multichannel_midi_by_channel() {
        let track = [
            0, 0xff, 0x03, 5, b'S', b'o', b'n', b'g', b'1', 0, 0x90, 60, 100, 0, 0x91, 48, 96,
            0x83, 0x60, 0x80, 60, 0, 0, 0x81, 48, 0, 0, 0xff, 0x2f, 0,
        ];
        let data = smf(&track);
        let tracks = parse_smf_tracks(&data).unwrap();

        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].name.as_deref(), Some("Song1 Ch 1"));
        assert_eq!(tracks[1].name.as_deref(), Some("Song1 Ch 2"));
        assert_eq!(tracks[0].channel_hint.unwrap().ui(), 1);
        assert_eq!(tracks[1].channel_hint.unwrap().ui(), 2);
        assert_eq!(tracks[0].clip.notes.len(), 1);
        assert_eq!(tracks[1].clip.notes.len(), 1);
        assert_eq!(tracks[0].clip.notes[0].channel.ui(), 1);
        assert_eq!(tracks[1].clip.notes[0].channel.ui(), 2);
    }

    #[test]
    fn parses_controller_lanes() {
        let data = smf(&[
            0, 0xb0, 1, 64, 0x83, 0x60, 0xe0, 0, 0x40, 0x83, 0x60, 0xd0, 100, 0, 0xff, 0x2f, 0,
        ]);
        let imported = parse_smf_notes(&data).unwrap();
        assert_eq!(imported.controller_lanes.len(), 3);

        let cc1 = imported
            .controller_lanes
            .iter()
            .find(|lane| lane.kind == MidiControllerKind::CC(1))
            .unwrap();
        assert_eq!(cc1.points.len(), 1);
        assert!((cc1.points[0].value - 64.0 / 127.0).abs() < 1.0e-4);

        let bend = imported
            .controller_lanes
            .iter()
            .find(|lane| lane.kind == MidiControllerKind::PitchBend)
            .unwrap();
        assert!((bend.points[0].beat - 1.0).abs() < 1.0e-4);
        assert!((bend.points[0].value - 8192.0 / 16383.0).abs() < 1.0e-4);

        let pressure = imported
            .controller_lanes
            .iter()
            .find(|lane| lane.kind == MidiControllerKind::ChannelPressure)
            .unwrap();
        assert!((pressure.points[0].beat - 2.0).abs() < 1.0e-4);
        assert!((pressure.points[0].value - 100.0 / 127.0).abs() < 1.0e-4);
    }

    #[test]
    fn preserves_normal_and_escaped_sysex_events() {
        let data = smf(&[
            0, 0xf0, 3, 0x43, 0x12, 0x00, 0x83, 0x60, 0xf7, 2, 0x7d, 0x01, 0, 0xff, 0x2f, 0,
        ]);
        let imported = parse_smf_notes(&data).unwrap();
        assert_eq!(imported.sysex_events.len(), 2);
        assert_eq!(imported.sysex_events[0].kind, ImportedSysExKind::Normal);
        assert_eq!(imported.sysex_events[0].absolute_tick, 0);
        assert_eq!(imported.sysex_events[0].data, vec![0x43, 0x12, 0x00]);
        assert_eq!(imported.sysex_events[1].kind, ImportedSysExKind::Escaped);
        assert!((imported.sysex_events[1].beat - 1.0).abs() < 1.0e-4);
        assert_eq!(imported.sysex_events[1].data, vec![0x7d, 0x01]);
    }

    /// One MTrk carrying notes on channels 1 and 2, a pitch bend on channel 2
    /// only, a marker, and a SysEx block.
    fn multichannel_track_with_channel_two_bend() -> Vec<u8> {
        smf(&[
            0, 0xff, 0x03, 4, b'S', b'o', b'n', b'g', // marker at tick 0
            0, 0xff, 0x06, 5, b'I', b'n', b't', b'r', b'o', // SysEx at tick 0
            0, 0xf0, 2, 0x7d, 0x01, // notes on ch 1 and ch 2
            0, 0x90, 60, 100, 0, 0x91, 48, 96, // full-up bend on channel 2 only
            0, 0xe1, 0x7f, 0x7f, 0x83, 0x60, 0x80, 60, 0, 0, 0x81, 48, 0, 0, 0xff, 0x2f, 0,
        ])
    }

    #[test]
    fn a_channels_pitch_bend_stays_on_that_channels_clip() {
        // Regression: every split clip used to inherit the whole track's
        // controller lanes, so channel 2's bend also bent channel 1 and the
        // dragged-in file played out of tune.
        let tracks = parse_smf_tracks(&multichannel_track_with_channel_two_bend()).unwrap();
        assert_eq!(tracks.len(), 2);

        let bend = |track: &ImportedMidiTrack| {
            track
                .clip
                .controller_lanes
                .iter()
                .any(|lane| lane.kind == MidiControllerKind::PitchBend)
        };
        assert!(!bend(&tracks[0]), "channel 1 must not inherit ch 2's bend");
        assert!(bend(&tracks[1]), "channel 2 keeps its own bend");
    }

    #[test]
    fn markers_and_sysex_are_not_duplicated_across_split_channels() {
        // Regression: markers/SysEx were cloned onto every channel clip, so a
        // 16-channel type-0 file imported each marker sixteen times.
        let tracks = parse_smf_tracks(&multichannel_track_with_channel_two_bend()).unwrap();
        let markers: usize = tracks.iter().map(|t| t.clip.markers.len()).sum();
        let sysex: usize = tracks.iter().map(|t| t.clip.sysex_events.len()).sum();
        assert_eq!(markers, 1);
        assert_eq!(sysex, 1);

        let summary = MidiImportSummary::of(&tracks);
        assert_eq!(summary.markers, 1);
        assert_eq!(summary.sysex_events, 1);
        assert_eq!(summary.notes, 2);
        assert!(summary.has_optional_payload());
    }

    #[test]
    fn import_options_drop_only_what_was_turned_off() {
        let mut tracks = parse_smf_tracks(&multichannel_track_with_channel_two_bend()).unwrap();
        apply_import_options(
            &mut tracks,
            MidiImportOptions {
                include_markers: false,
                include_controllers: true,
                include_sysex: false,
            },
        );
        let summary = MidiImportSummary::of(&tracks);
        assert_eq!(summary.markers, 0);
        assert_eq!(summary.sysex_events, 0);
        assert_eq!(summary.notes, 2, "notes are never optional");
        assert_eq!(summary.controller_lanes, 1, "the bend lane is kept");

        apply_import_options(&mut tracks, MidiImportOptions::NOTES_ONLY);
        assert_eq!(MidiImportSummary::of(&tracks).controller_lanes, 0);
    }

    #[test]
    fn a_note_only_file_has_nothing_to_ask_about() {
        let data = smf(&[0, 0x90, 60, 100, 0x83, 0x60, 0x80, 60, 0, 0, 0xff, 0x2f, 0]);
        let tracks = parse_smf_tracks(&data).unwrap();
        assert!(!MidiImportSummary::of(&tracks).has_optional_payload());
    }

    #[test]
    fn an_unrecognized_chunk_before_the_track_still_imports() {
        let track = [0, 0x90, 60, 100, 0x83, 0x60, 0x80, 60, 0, 0, 0xff, 0x2f, 0];
        // Format 0, one track, 480 PPQ, then a private chunk, then the track.
        let mut data = vec![b'M', b'T', b'h', b'd', 0, 0, 0, 6, 0, 0, 0, 1, 0x01, 0xE0];
        data.extend_from_slice(b"XFIH");
        data.extend_from_slice(&4u32.to_be_bytes());
        data.extend_from_slice(&[9, 8, 7, 6]);
        data.extend_from_slice(b"MTrk");
        data.extend_from_slice(&(track.len() as u32).to_be_bytes());
        data.extend_from_slice(&track);

        let imported = parse_smf_notes(&data).expect("foreign chunk is skipped");
        assert_eq!(imported.notes.len(), 1);
        assert_eq!(imported.notes[0].pitch, 60);
    }

    #[test]
    fn an_unrecognized_chunk_between_tracks_is_skipped() {
        let track_a = [0, 0x90, 60, 80, 0x83, 0x60, 0x80, 60, 0, 0, 0xff, 0x2f, 0];
        let track_b = [0, 0x91, 48, 90, 0x83, 0x60, 0x81, 48, 0, 0, 0xff, 0x2f, 0];
        let mut data = smf_format(1, &[&track_a]);
        // The helper wrote a track count of 1. This file has two tracks.
        data[10] = 0;
        data[11] = 2;
        data.extend_from_slice(b"SEQ ");
        data.extend_from_slice(&2u32.to_be_bytes());
        data.extend_from_slice(&[0, 1]);
        data.extend_from_slice(b"MTrk");
        data.extend_from_slice(&(track_b.len() as u32).to_be_bytes());
        data.extend_from_slice(&track_b);

        let tracks = parse_smf_tracks(&data).expect("chunk between tracks");
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].clip.notes[0].pitch, 60);
        assert_eq!(tracks[1].clip.notes[0].pitch, 48);
    }

    #[test]
    fn a_file_that_is_not_midi_still_fails() {
        let err = parse_smf_tracks(b"RIFF not a midi file").unwrap_err();
        assert_eq!(err, MidiImportError::InvalidHeader);
        assert!(!err.to_string().is_empty());
    }

    #[test]
    fn high_ppq_keeps_one_controller_point_per_tick() {
        // 9600 ticks per quarter. One tick is ~0.000104 beats, inside the old
        // 1e-3 merge window, which collapsed this ramp to a handful of points.
        let mut track = Vec::new();
        for tick in 0..64u32 {
            track.push(if tick == 0 { 0 } else { 1 });
            track.extend_from_slice(&[0xb0, 1, (tick % 128) as u8]);
        }
        track.extend_from_slice(&[0, 0xff, 0x2f, 0]);
        let data = smf_division(0, 9600, &[&track]);
        let imported = parse_smf_notes(&data).unwrap();
        let cc = imported
            .controller_lanes
            .iter()
            .find(|lane| lane.kind == MidiControllerKind::CC(1))
            .unwrap();
        assert_eq!(cc.points.len(), 64);
        assert!(cc.points.windows(2).all(|pair| pair[1].beat > pair[0].beat));
        assert!((cc.points[63].value - 63.0 / 127.0).abs() < 1.0e-4);
    }

    #[test]
    fn the_same_tick_keeps_the_latest_controller_value() {
        let data = smf(&[0, 0xb0, 1, 10, 0, 0xb0, 1, 80, 0, 0xff, 0x2f, 0]);
        let imported = parse_smf_notes(&data).unwrap();
        let cc = imported
            .controller_lanes
            .iter()
            .find(|lane| lane.kind == MidiControllerKind::CC(1))
            .unwrap();
        assert_eq!(cc.points.len(), 1);
        assert!((cc.points[0].value - 80.0 / 127.0).abs() < 1.0e-4);
    }

    #[test]
    fn merged_tracks_keep_controller_points_in_beat_order() {
        let track_a = [0, 0xb0, 1, 10, 0x83, 0x60, 0xb0, 1, 40, 0, 0xff, 0x2f, 0];
        // Tick 240 is half a beat at 480 PPQ, between the two events above.
        let track_b = [0x81, 0x70, 0xb0, 1, 20, 0, 0xff, 0x2f, 0];
        let data = smf_format(1, &[&track_a, &track_b]);
        let imported = parse_smf_notes(&data).unwrap();
        let cc = imported
            .controller_lanes
            .iter()
            .find(|lane| lane.kind == MidiControllerKind::CC(1))
            .unwrap();
        assert_eq!(cc.points.len(), 3);
        let values: Vec<u8> = cc
            .points
            .iter()
            .map(|point| (point.value * 127.0).round() as u8)
            .collect();
        assert_eq!(values, vec![10, 20, 40]);
        assert!(cc.points.windows(2).all(|pair| pair[1].beat > pair[0].beat));
    }

    #[test]
    fn a_dense_controller_lane_keeps_every_tick() {
        let count = 4_000u32;
        let mut track = Vec::with_capacity(count as usize * 4);
        for tick in 0..count {
            track.push(if tick == 0 { 0 } else { 1 });
            track.extend_from_slice(&[0xe0, (tick & 0x7f) as u8, ((tick >> 7) & 0x7f) as u8]);
        }
        track.extend_from_slice(&[0, 0xff, 0x2f, 0]);
        let imported = parse_smf_notes(&smf(&track)).unwrap();
        let bend = imported
            .controller_lanes
            .iter()
            .find(|lane| lane.kind == MidiControllerKind::PitchBend)
            .unwrap();
        assert_eq!(bend.points.len(), count as usize);
        assert!(bend
            .points
            .windows(2)
            .all(|pair| pair[1].beat > pair[0].beat));
    }

    #[test]
    fn parses_marker_meta_events() {
        let data = smf(&[
            0x83, 0x60, 0xff, 0x06, 5, b'V', b'e', b'r', b's', b'e', 0, 0xff, 0x2f, 0,
        ]);
        let imported = parse_smf_notes(&data).unwrap();
        assert_eq!(imported.markers.len(), 1);
        assert_eq!(imported.markers[0].text, "Verse");
        assert_eq!(imported.markers[0].absolute_tick, 480);
        assert!((imported.markers[0].beat - 1.0).abs() < 1.0e-4);
    }
}
