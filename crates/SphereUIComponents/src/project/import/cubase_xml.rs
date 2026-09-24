//! Cubase / Nuendo XML track-archive import.
//!
//! Steinberg's XML export writes the arrangement as a generic object tree:
//! `<obj class="…" ID="…">` is a definition, a childless `<obj name="…"
//! ID="…"/>` is a reference back to one, and scalars hang off an object as
//! `<int|float|string name="…" value="…"/>`.
//!
//! What is mapped:
//!
//! | Cubase | Futureboard |
//! |---|---|
//! | `PArrangeSetup` | project sample rate |
//! | `MTempoEvent` | tempo + tempo points |
//! | `MTimeSignatureEvent` | time signature + signature points |
//! | `MAudioTrackEvent` | one audio track (named from its `MListNode`) |
//! | `MAudioEvent` | one audio clip: position, length, offset, event gain |
//! | `FNPath` / `AudioFile` | the media file, its rate, frames and channels |
//!
//! Event `Volume` is renormalized so Cubase's info-line 0.00 dB (stored as
//! ~0.2927, not 1.0) becomes Futureboard gain 1.0. Mixer channel/insert state
//! still lives in Cubase's opaque binary blob and is not guessed at.
//!
//! Track timebase (`MListNode/Domain/Type`) controls how `Start` is read:
//! musical (`0`) uses PPQ ticks, sample / linear domains use arrangement
//! samples. Event `Length` that matches the referenced file's frame count is
//! always treated as samples so audio duration stays physical wall-time.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use roxmltree::{Document, Node, NodeId};

use crate::components::timeline::timeline_state::volume;
use crate::project::{
    AudioClipStretchState, ClipSource, FutureboardProject, InputMonitorMode, ProjectAsset,
    ProjectClip, ProjectTempoPoint, ProjectTimeSignaturePoint, ProjectTrack, ProjectTrackType,
    TrackRouting, format::ProjectError, new_id,
};

/// Ticks per quarter note in the arrangement domain.
///
/// Derived from the archive itself: a track event of 988_799.953 units in a
/// 206 BPM / 600 s arrangement is 2060 quarter notes, i.e. 480 units each —
/// the same PPQN Cubase uses everywhere else.
const TICKS_PER_QUARTER: f64 = 480.0;

/// Cubase XML `MAudioEvent/Volume` at the info-line 0.00 dB default.
///
/// Steinberg does not store linear unity as `1.0` here — a freshly created
/// stereo event writes this constant instead. Dividing through maps 0 dB to
/// Futureboard clip gain `1.0` while preserving relative boosts/cuts.
const CUBASE_EVENT_VOLUME_UNITY: f64 = 0.292_682_886_123_657_23;

/// How a track measures event start positions (`MListNode/Domain`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrackTimebase {
    /// Bars & beats — `Start` is PPQ ticks (`TICKS_PER_QUARTER` per beat).
    Musical,
    /// Linear / sample — `Start` is arrangement samples at the project rate.
    Samples,
}

/// Directories under the archive's own folder that are searched for media
/// before falling back to a recursive scan.
const MEDIA_SUBDIRS: &[&str] = &["Audio", "Audio Files", "Media"];

/// Depth limit for the last-resort media scan, so a project sitting in a home
/// directory cannot walk the whole disk.
const MEDIA_SCAN_MAX_DEPTH: usize = 3;

/// Whether `bytes` look like a Cubase XML track archive.
///
/// Cheap enough for the open-project validation step: it only looks at the
/// leading bytes, never parses the document.
pub(super) fn sniff(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(4096)];
    let text = String::from_utf8_lossy(head);
    text.contains("<tracklist")
}

/// Parse `path` into an in-memory project. The archive is never written back;
/// callers bind the result as an untitled session.
pub(super) fn import(path: &Path) -> Result<FutureboardProject, ProjectError> {
    let bytes = std::fs::read(path).map_err(ProjectError::Io)?;
    // Cubase writes UTF-8, but an archive that travelled through a different
    // locale is still worth reading — the parts this importer needs are ASCII.
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let doc = Document::parse(&text)
        .map_err(|error| ProjectError::Corrupted(format!("XML parse failed: {error}")))?;
    let archive = Archive::index(&doc);

    let sample_rate = archive
        .find_class("PArrangeSetup")
        .and_then(|setup| prim_f64(setup, "SampleRate"))
        .filter(|rate| *rate >= 8_000.0 && *rate <= 768_000.0)
        .map(|rate| rate as u32)
        .unwrap_or(48_000);

    let tempo = TempoMap::from_archive(&archive);
    let name = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("Imported Project")
        .to_string();
    let mut project = FutureboardProject::new(name);
    project.settings.sample_rate = sample_rate;
    project.settings.bpm = tempo.first_bpm();
    project.settings.tempo_points = tempo.project_points();

    let signatures = signature_points(&archive);
    if let Some(first) = signatures.first() {
        project.settings.time_sig_num = first.numerator as u32;
        project.settings.time_sig_den = first.denominator as u32;
    }
    // A single signature at bar 1 is fully described by the pair above; only
    // keep the list when the arrangement actually changes signature.
    if signatures.len() > 1 {
        project.settings.time_signature_points = signatures;
    }

    let media_root = path.parent().unwrap_or(Path::new("."));
    let mut media = MediaResolver::new(media_root);
    let mut assets: HashMap<PathBuf, String> = HashMap::new();
    let mut missing_media = 0usize;

    for (index, track_node) in archive.iter_class("MAudioTrackEvent").enumerate() {
        let node = archive.child_class(track_node, "MListNode");
        let track_name = node
            .and_then(|node| prim_str(node, "Name"))
            .map(str::to_string)
            .or_else(|| device_name(&archive, track_node))
            .unwrap_or_else(|| format!("Audio {}", index + 1));

        let mut clips = Vec::new();
        // Carried out of the clip loop so the imported track keeps the timebase
        // Cubase gave it: a linear track there is a Linear track here, and a
        // later tempo change moves its clips the same way the original did.
        let mut track_domain = TrackTimebase::Musical;
        if let Some(node) = node {
            let timebase = track_timebase(node);
            track_domain = timebase;
            for event in archive.list_objects(node, "Events", "MAudioEvent") {
                if let Some(clip) = audio_clip(
                    &archive,
                    event,
                    &tempo,
                    sample_rate,
                    timebase,
                    &mut media,
                    &mut assets,
                    &mut project.assets,
                    &mut missing_media,
                ) {
                    clips.push(clip);
                }
            }
        }

        project.tracks.push(ProjectTrack {
            id: new_id(),
            name: track_name,
            track_type: ProjectTrackType::Audio,
            ara: None,
            timebase: match track_domain {
                TrackTimebase::Musical => {
                    crate::components::timeline::timeline_state::TrackTimebase::Musical
                }
                TrackTimebase::Samples => {
                    crate::components::timeline::timeline_state::TrackTimebase::Linear
                }
            }
            .to_tag(),
            parent_group_id: None,
            group_collapsed: false,
            color_hex: crate::project::rgba_to_hex(crate::color::auto_color_for_index(index)),
            // Cubase keeps fader/pan inside an opaque binary channel blob, so
            // imported tracks start at unity rather than at an invented value.
            volume_norm: volume::db_to_norm(0.0),
            pan: 0.0,
            muted: false,
            solo: false,
            record_arm: false,
            input_monitor: InputMonitorMode::Off,
            routing: TrackRouting::default(),
            inserts: Vec::new(),
            automation_lanes: Vec::new(),
            clips,
            row_height_px: None,
            soundfont: None,
            volume_automation_read: true,
            solfege: None,
            takes: Vec::new(),
            takes_expanded: false,
        });
    }

    if project.tracks.is_empty() {
        return Err(ProjectError::Corrupted(
            "no audio tracks found in the XML track archive".to_string(),
        ));
    }

    eprintln!(
        "[ProjectImport] cubase-xml: {} tracks, {} clips, {} assets, {} clips without media",
        project.tracks.len(),
        project
            .tracks
            .iter()
            .map(|track| track.clips.len())
            .sum::<usize>(),
        project.assets.len(),
        missing_media,
    );

    Ok(project)
}

// ── Object graph ─────────────────────────────────────────────────────────────

/// The archive's object tree plus an ID index, so a childless `<obj ID="…"/>`
/// reference can be followed to the definition that carries the data.
struct Archive<'a, 'input> {
    doc: &'a Document<'input>,
    by_id: HashMap<&'a str, NodeId>,
}

impl<'a, 'input> Archive<'a, 'input> {
    fn index(doc: &'a Document<'input>) -> Self {
        let mut by_id = HashMap::new();
        for node in doc.descendants() {
            if !node.has_tag_name("obj") {
                continue;
            }
            let Some(id) = node.attribute("ID") else {
                continue;
            };
            // Definitions carry the class and the payload; references are bare.
            if node.attribute("class").is_some() || node.children().any(|c| c.is_element()) {
                by_id.entry(id).or_insert_with(|| node.id());
            }
        }
        Self { doc, by_id }
    }

    /// Follow `node` to its definition when it is only a reference.
    fn resolve(&self, node: Node<'a, 'input>) -> Node<'a, 'input> {
        if node.children().any(|child| child.is_element()) {
            return node;
        }
        node.attribute("ID")
            .and_then(|id| self.by_id.get(id))
            .and_then(|id| self.doc.get_node(*id))
            .unwrap_or(node)
    }

    fn iter_class(&self, class: &'static str) -> impl Iterator<Item = Node<'a, 'input>> + '_ {
        self.doc
            .descendants()
            .filter(move |node| node.attribute("class") == Some(class))
    }

    fn find_class(&self, class: &'static str) -> Option<Node<'a, 'input>> {
        self.iter_class(class).next()
    }

    /// First direct child object of `class`, following references.
    fn child_class(&self, parent: Node<'a, 'input>, class: &str) -> Option<Node<'a, 'input>> {
        parent
            .children()
            .filter(|child| child.has_tag_name("obj"))
            .map(|child| self.resolve(child))
            .find(|child| child.attribute("class") == Some(class))
    }

    /// Objects of `class` inside the `<list name="…">` of `parent`.
    fn list_objects(
        &self,
        parent: Node<'a, 'input>,
        list_name: &str,
        class: &'static str,
    ) -> Vec<Node<'a, 'input>> {
        parent
            .children()
            .filter(|child| {
                child.has_tag_name("list") && child.attribute("name") == Some(list_name)
            })
            .flat_map(|list| list.children())
            .filter(|child| child.has_tag_name("obj"))
            .map(|child| self.resolve(child))
            .filter(|child| child.attribute("class") == Some(class))
            .collect()
    }

    /// First descendant object of `class`, following references on the way down.
    fn descendant_class(&self, parent: Node<'a, 'input>, class: &str) -> Option<Node<'a, 'input>> {
        let mut stack = vec![parent];
        let mut seen = 0usize;
        while let Some(node) = stack.pop() {
            // Archives can nest deeply (hit points, automation); the cap keeps a
            // pathological file from turning one lookup into a full-tree walk.
            seen += 1;
            if seen > 20_000 {
                break;
            }
            for child in node.children().filter(|child| child.is_element()) {
                let child = if child.has_tag_name("obj") {
                    self.resolve(child)
                } else {
                    child
                };
                if child.attribute("class") == Some(class) {
                    return Some(child);
                }
                stack.push(child);
            }
        }
        None
    }
}

fn prim<'a, 'input>(node: Node<'a, 'input>, name: &str) -> Option<&'a str> {
    node.children()
        .filter(|child| {
            matches!(child.tag_name().name(), "int" | "float" | "string")
                && child.attribute("name") == Some(name)
        })
        .find_map(|child| child.attribute("value"))
}

fn prim_str<'a, 'input>(node: Node<'a, 'input>, name: &str) -> Option<&'a str> {
    prim(node, name).filter(|value| !value.is_empty())
}

fn prim_f64(node: Node<'_, '_>, name: &str) -> Option<f64> {
    prim(node, name)?.parse::<f64>().ok()
}

/// `MAudioTrack/DeviceAttributes/Name/String` — the mixer channel name, used
/// when the arrangement node has none.
fn device_name(archive: &Archive<'_, '_>, track_event: Node<'_, '_>) -> Option<String> {
    let device = archive.descendant_class(track_event, "MAudioTrack")?;
    let attributes = device.children().find(|child| {
        child.has_tag_name("member") && child.attribute("name") == Some("DeviceAttributes")
    })?;
    let name = attributes
        .children()
        .find(|child| child.has_tag_name("member") && child.attribute("name") == Some("Name"))?;
    prim_str(name, "String").map(str::to_string)
}

// ── Tempo and signature ──────────────────────────────────────────────────────

/// Piecewise-constant tempo map used to place sample positions on the beat
/// grid. Cubase ramp tempi are imported as jumps at their start beat.
struct TempoMap {
    /// `(beat, bpm, seconds at that beat)`, ordered by beat.
    points: Vec<(f64, f64, f64)>,
}

impl TempoMap {
    fn from_archive(archive: &Archive<'_, '_>) -> Self {
        let mut raw: Vec<(f64, f64)> = archive
            .iter_class("MTempoEvent")
            .filter_map(|event| {
                // Cubase stores tempo as f32, so 206 BPM comes back as
                // 205.99998…; round off that noise instead of importing it.
                let bpm = (prim_f64(event, "BPM")? * 1000.0).round() / 1000.0;
                if !(1.0..=999.0).contains(&bpm) {
                    return None;
                }
                let beat = prim_f64(event, "PPQ").unwrap_or(0.0) / TICKS_PER_QUARTER;
                Some((beat.max(0.0), bpm))
            })
            .collect();
        raw.sort_by(|a, b| a.0.total_cmp(&b.0));
        raw.dedup_by(|a, b| a.0 == b.0);
        if raw.is_empty() {
            raw.push((0.0, 120.0));
        }
        if raw[0].0 > 0.0 {
            let first_bpm = raw[0].1;
            raw.insert(0, (0.0, first_bpm));
        }

        let mut points = Vec::with_capacity(raw.len());
        let mut seconds = 0.0;
        let mut previous: Option<(f64, f64)> = None;
        for (beat, bpm) in raw {
            if let Some((prev_beat, prev_bpm)) = previous {
                seconds += (beat - prev_beat) * 60.0 / prev_bpm;
            }
            points.push((beat, bpm, seconds));
            previous = Some((beat, bpm));
        }
        Self { points }
    }

    fn first_bpm(&self) -> f64 {
        self.points.first().map(|point| point.1).unwrap_or(120.0)
    }

    fn seconds_to_beats(&self, seconds: f64) -> f64 {
        let mut current = self.points[0];
        for point in &self.points {
            if point.2 <= seconds {
                current = *point;
            } else {
                break;
            }
        }
        let (beat, bpm, at_seconds) = current;
        beat + (seconds - at_seconds) * bpm / 60.0
    }

    fn project_points(&self) -> Vec<ProjectTempoPoint> {
        if self.points.len() < 2 {
            return Vec::new();
        }
        self.points
            .iter()
            .map(|(beat, bpm, _)| ProjectTempoPoint {
                id: new_id(),
                beat: *beat,
                bpm: *bpm,
                curve: 0,
                tension: 0.0,
            })
            .collect()
    }
}

fn signature_points(archive: &Archive<'_, '_>) -> Vec<ProjectTimeSignaturePoint> {
    let mut points: Vec<ProjectTimeSignaturePoint> = archive
        .iter_class("MTimeSignatureEvent")
        .filter_map(|event| {
            let numerator = prim_f64(event, "Numerator")? as u16;
            let denominator = prim_f64(event, "Denominator")? as u16;
            if numerator == 0 || denominator == 0 {
                return None;
            }
            // Archives write either `Position` (ticks) or `Start` (same units);
            // prefer Position when both exist.
            let ticks = prim_f64(event, "Position")
                .or_else(|| prim_f64(event, "Start"))
                .unwrap_or(0.0);
            let beat = ticks / TICKS_PER_QUARTER;
            Some(ProjectTimeSignaturePoint {
                id: new_id(),
                beat: beat.max(0.0),
                numerator,
                denominator,
                grouping: Vec::new(),
            })
        })
        .collect();
    points.sort_by(|a, b| a.beat.total_cmp(&b.beat));
    points
}

/// Map Cubase's event `Volume` field onto Futureboard linear clip gain.
fn cubase_event_gain(raw: f64) -> f32 {
    if !raw.is_finite() || raw <= 0.0 {
        return 1.0;
    }
    ((raw / CUBASE_EVENT_VOLUME_UNITY) as f32).clamp(0.0, 4.0)
}

/// Resolve a track's arrangement timebase from its list-node Domain.
///
/// Cubase: Musical → Start in beats/ticks, Linear → Start in seconds/samples.
/// Type `0` plus a Tempo Track reference covers musical exports; Type `10`
/// (sample period) covers linear. A Domain that embeds a tempo track without
/// an explicit Type is also treated as musical.
fn track_timebase(list_node: Node<'_, '_>) -> TrackTimebase {
    let Some(domain) = list_node
        .children()
        .find(|child| child.has_tag_name("member") && child.attribute("name") == Some("Domain"))
    else {
        return TrackTimebase::Samples;
    };
    if let Some(kind) = prim_f64(domain, "Type") {
        return match kind as i32 {
            0 => TrackTimebase::Musical,
            _ => TrackTimebase::Samples,
        };
    }
    let musical = domain.children().any(|child| {
        child.has_tag_name("obj")
            && (child.attribute("class") == Some("MTempoTrackEvent")
                || child.attribute("name") == Some("Tempo Track"))
    });
    if musical {
        TrackTimebase::Musical
    } else {
        TrackTimebase::Samples
    }
}

// ── Clips and media ──────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn audio_clip(
    archive: &Archive<'_, '_>,
    event: Node<'_, '_>,
    tempo: &TempoMap,
    sample_rate: u32,
    timebase: TrackTimebase,
    media: &mut MediaResolver,
    assets: &mut HashMap<PathBuf, String>,
    project_assets: &mut Vec<ProjectAsset>,
    missing_media: &mut usize,
) -> Option<ProjectClip> {
    let rate = sample_rate.max(1) as f64;
    let start_raw = prim_f64(event, "Start").unwrap_or(0.0).max(0.0);
    let length_raw = prim_f64(event, "Length").unwrap_or(0.0).max(0.0);

    // Start follows the track timebase. Length that describes audible media is
    // in samples (it matches AudioFile/FrameCount on every clip in real
    // archives) even when Start is musical PPQ.
    let start_beat = match timebase {
        TrackTimebase::Musical => start_raw / TICKS_PER_QUARTER,
        TrackTimebase::Samples => tempo.seconds_to_beats(start_raw / rate),
    };
    let length_seconds = length_raw / rate;
    if length_seconds <= 0.0 {
        return None;
    }
    let start_seconds = match timebase {
        TrackTimebase::Musical => {
            // Invert the tempo map at start_beat for a wall-clock duration span.
            // For a constant-tempo project this is start_beat * 60 / bpm.
            beat_to_seconds(tempo, start_beat)
        }
        TrackTimebase::Samples => start_raw / rate,
    };
    let end_beat = tempo.seconds_to_beats(start_seconds + length_seconds);
    let duration_beats = (end_beat - start_beat).max(0.0);
    if duration_beats <= 0.0 {
        return None;
    }

    let clip_obj = archive.child_class(event, "PAudioClip");
    let name = prim_str(event, "Description")
        .or_else(|| clip_obj.and_then(|clip| prim_str(clip, "Name")))
        .unwrap_or("Audio")
        .to_string();

    let file = clip_obj.and_then(|clip| media_file(archive, clip))?;
    let resolved = media.resolve(&file.recorded_dir, &file.file_name);
    // An unresolved file keeps its clip: losing the arrangement is worse than
    // showing a clip whose media has to be relinked. The recorded path is kept
    // so the user can see where it came from.
    let path = resolved.clone().unwrap_or_else(|| {
        PathBuf::from(file.recorded_dir.replace('\\', "/")).join(&file.file_name)
    });
    let asset_id = assets
        .entry(path.clone())
        .or_insert_with(|| {
            let id = new_id();
            project_assets.push(ProjectAsset {
                id: id.clone(),
                original_filename: file.file_name.clone(),
                relative_path: None,
                absolute_path: Some(path.clone()),
                duration_secs: file.duration_secs(),
                sample_rate: file.sample_rate,
                channels: file.channels,
                source_fingerprint: None,
                waveform_peak_relative_path: None,
                duration_samples: file.frames,
            });
            id
        })
        .clone();
    let source = ClipSource::Audio {
        asset_id,
        source_path: resolved.clone().or(Some(path)),
    };
    if resolved.is_none() {
        *missing_media += 1;
    }

    // Source-domain offset: event Offset first, else AudioCluster/Segments.
    let file_rate = file.sample_rate.unwrap_or(sample_rate).max(1) as f64;
    let event_offset_samples = prim_f64(event, "Offset").unwrap_or(0.0).max(0.0);
    let segment_offset_samples = clip_obj
        .and_then(|clip| segment_source_offset(archive, clip))
        .unwrap_or(0.0)
        .max(0.0);
    let offset_samples = if event_offset_samples > 0.0 {
        event_offset_samples
    } else {
        segment_offset_samples
    };
    let offset_seconds = offset_samples / file_rate;

    Some(ProjectClip {
        id: new_id(),
        name,
        start_beat,
        duration_beats,
        // Beat-domain offset into the source, same conversion as the position.
        offset_beats: (tempo.seconds_to_beats(offset_seconds) - tempo.seconds_to_beats(0.0)) as f32,
        // Cubase's default 0 dB is ~0.2927, not 1.0 — normalize first.
        gain: cubase_event_gain(prim_f64(event, "Volume").unwrap_or(CUBASE_EVENT_VOLUME_UNITY)),
        muted: false,
        source,
        stretch: AudioClipStretchState::default(),
    })
}

/// Wall-clock seconds at `beat` according to the piecewise tempo map.
fn beat_to_seconds(tempo: &TempoMap, beat: f64) -> f64 {
    let mut current = tempo.points[0];
    for point in &tempo.points {
        if point.0 <= beat {
            current = *point;
        } else {
            break;
        }
    }
    let (at_beat, bpm, at_seconds) = current;
    at_seconds + (beat - at_beat) * 60.0 / bpm
}

/// A media file referenced by a clip, with the format facts the archive already
/// knows — so importing does not have to decode anything.
struct MediaFile {
    /// Directory recorded by the exporting machine, e.g. `C:\…\Audio\`.
    recorded_dir: String,
    file_name: String,
    sample_rate: Option<u32>,
    channels: Option<u8>,
    frames: Option<u64>,
}

impl MediaFile {
    fn duration_secs(&self) -> Option<f64> {
        match (self.frames, self.sample_rate) {
            (Some(frames), Some(rate)) if rate > 0 => Some(frames as f64 / rate as f64),
            _ => None,
        }
    }
}

fn media_file(archive: &Archive<'_, '_>, clip: Node<'_, '_>) -> Option<MediaFile> {
    // Prefer the clip's own FNPath / AudioCluster children. A deep walk would
    // otherwise burn the hit-point list (often thousands of MHitPointEvent
    // nodes) before finding the path, and the walk cap can skip the media.
    let path_obj = archive
        .child_class(clip, "FNPath")
        .or_else(|| archive.descendant_class(clip, "FNPath"))?;
    let file_name = prim_str(path_obj, "Name")?.to_string();
    let recorded_dir = prim_str(path_obj, "Path").unwrap_or("");

    let audio_file = archive
        .child_class(clip, "AudioCluster")
        .and_then(|cluster| archive.descendant_class(cluster, "AudioFile"))
        .or_else(|| archive.descendant_class(clip, "AudioFile"));
    let sample_rate = audio_file
        .and_then(|file| prim_f64(file, "Rate"))
        .filter(|rate| *rate > 0.0)
        .map(|rate| rate as u32);
    let frames = audio_file
        .and_then(|file| prim_f64(file, "FrameCount"))
        .filter(|frames| *frames >= 0.0)
        .map(|frames| frames as u64);
    let channels = audio_file.and_then(channel_count);

    Some(MediaFile {
        recorded_dir: recorded_dir.to_string(),
        file_name,
        sample_rate,
        channels,
        frames,
    })
}

/// Sample offset into the source from `AudioCluster/Segments[0]/Offset`.
fn segment_source_offset(archive: &Archive<'_, '_>, clip: Node<'_, '_>) -> Option<f64> {
    let cluster = archive.child_class(clip, "AudioCluster")?;
    let segments = cluster
        .children()
        .find(|child| child.has_tag_name("list") && child.attribute("name") == Some("Segments"))?;
    let first = segments
        .children()
        .find(|child| child.has_tag_name("item") || child.has_tag_name("obj"))?;
    prim_f64(first, "Offset")
}

/// `SpeakerArr/Type` lists one entry per channel.
fn channel_count(audio_file: Node<'_, '_>) -> Option<u8> {
    let arrangement = audio_file.children().find(|child| {
        child.has_tag_name("member") && child.attribute("name") == Some("SpeakerArr")
    })?;
    let list = arrangement
        .children()
        .find(|child| child.has_tag_name("list") && child.attribute("name") == Some("Type"))?;
    let count = list.children().filter(|item| item.is_element()).count();
    (count > 0).then(|| count.min(u8::MAX as usize) as u8)
}

/// Finds the media a clip refers to. Archives carry the recording machine's
/// absolute paths (`C:\Users\…\Audio\`), so the file is looked up next to the
/// archive first and the recorded path is only a fallback.
struct MediaResolver {
    root: PathBuf,
    /// Lower-cased file name → path, filled lazily by the recursive scan.
    scanned: Option<HashMap<String, PathBuf>>,
}

impl MediaResolver {
    fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            scanned: None,
        }
    }

    fn resolve(&mut self, recorded_dir: &str, file_name: &str) -> Option<PathBuf> {
        for subdir in MEDIA_SUBDIRS {
            let candidate = self.root.join(subdir).join(file_name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        let beside = self.root.join(file_name);
        if beside.is_file() {
            return Some(beside);
        }
        // The archive's own folder name for media, when it is not one of the
        // usual suspects (e.g. a renamed pool folder).
        if let Some(recorded_leaf) = recorded_leaf(recorded_dir) {
            let candidate = self.root.join(recorded_leaf).join(file_name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        // Same machine as the export: the recorded path still resolves.
        let recorded = PathBuf::from(recorded_dir.replace('\\', "/")).join(file_name);
        if recorded.is_file() {
            return Some(recorded);
        }
        let index = self
            .scanned
            .get_or_insert_with(|| scan_media(&self.root, MEDIA_SCAN_MAX_DEPTH));
        index.get(&file_name.to_lowercase()).cloned()
    }
}

fn recorded_leaf(recorded_dir: &str) -> Option<String> {
    recorded_dir
        .replace('\\', "/")
        .rsplit('/')
        .find(|part| !part.is_empty())
        .map(str::to_string)
}

fn scan_media(root: &Path, max_depth: usize) -> HashMap<String, PathBuf> {
    let mut index = HashMap::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if depth < max_depth {
                    stack.push((path, depth + 1));
                }
                continue;
            }
            if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
                index.entry(name.to_lowercase()).or_insert(path);
            }
        }
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const ARCHIVE: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<tracklist>
   <obj class="PArrangeSetup" name="Setup" ID="1">
      <float name="SampleRate" value="44100"/>
   </obj>
   <obj class="MAudioTrackEvent" ID="10">
      <obj class="MListNode" name="Node" ID="11">
         <string name="Name" value="KICK" wide="true"/>
         <member name="Domain">
            <int name="Type" value="0"/>
            <obj class="MTempoTrackEvent" name="Tempo Track" ID="12">
               <list name="TempoEvent" type="obj">
                  <obj class="MTempoEvent" ID="13">
                     <float name="BPM" value="120"/>
                     <float name="PPQ" value="0"/>
                  </obj>
               </list>
            </obj>
            <obj class="MSignatureTrackEvent" name="Signature Track" ID="14">
               <list name="SignatureEvent" type="obj">
                  <obj class="MTimeSignatureEvent" ID="15">
                     <int name="Numerator" value="3"/>
                     <int name="Denominator" value="4"/>
                     <int name="Position" value="0"/>
                  </obj>
               </list>
            </obj>
         </member>
         <list name="Events" type="obj">
            <obj class="MAudioEvent" ID="16">
               <!-- Musical Start: 4 beats * 480 PPQ. Length stays samples. -->
               <float name="Start" value="1920"/>
               <float name="Length" value="44100"/>
               <float name="Volume" value="0.5"/>
               <string name="Description" value="Kick take" wide="true"/>
               <obj class="PAudioClip" name="AudioClip" ID="17">
                  <string name="Name" value="Kick" wide="true"/>
                  <obj class="FNPath" name="Path" ID="18">
                     <string name="Name" value="kick.wav" wide="true"/>
                     <string name="Path" value="C:\Somewhere\Else\Audio\" wide="true"/>
                  </obj>
                  <obj class="AudioCluster" name="Cluster" ID="19">
                     <list name="Substreams" type="obj">
                        <obj class="AudioFile" ID="20">
                           <obj name="FPath" ID="18"/>
                           <int name="FrameCount" value="44100"/>
                           <member name="SpeakerArr">
                              <list name="Type" type="int">
                                 <item value="1"/>
                                 <item value="2"/>
                              </list>
                           </member>
                           <float name="Rate" value="44100"/>
                        </obj>
                     </list>
                  </obj>
               </obj>
            </obj>
         </list>
      </obj>
   </obj>
</tracklist>
"#;

    fn archive_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fb_cubase_import_{label}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir_all(dir.join("Audio")).unwrap();
        fs::write(dir.join("Audio/kick.wav"), b"RIFF").unwrap();
        fs::write(dir.join("Song.xml"), ARCHIVE).unwrap();
        dir
    }

    #[test]
    fn sniff_accepts_a_track_archive_and_rejects_other_xml() {
        assert!(sniff(ARCHIVE.as_bytes()));
        assert!(!sniff(b"<?xml version=\"1.0\"?><svg></svg>"));
    }

    #[test]
    fn import_maps_tracks_clips_tempo_and_signature() {
        let dir = archive_dir("basic");
        let project = import(&dir.join("Song.xml")).unwrap();

        assert_eq!(project.name, "Song");
        assert_eq!(project.settings.sample_rate, 44_100);
        assert_eq!(project.settings.bpm, 120.0);
        assert_eq!(project.settings.time_sig_num, 3);
        assert_eq!(project.settings.time_sig_den, 4);
        // One signature needs no marker list; the pair above already says it.
        assert!(project.settings.time_signature_points.is_empty());

        assert_eq!(project.tracks.len(), 1);
        let track = &project.tracks[0];
        assert_eq!(track.name, "KICK");
        assert_eq!(track.clips.len(), 1);

        let clip = &track.clips[0];
        assert_eq!(clip.name, "Kick take");
        // Musical Start=1920 PPQ → 4 beats. Length=44100 samples → 1 s → 2 beats @120.
        assert!(
            (clip.start_beat - 4.0).abs() < 1e-6,
            "start {}",
            clip.start_beat
        );
        assert!(
            (clip.duration_beats - 2.0).abs() < 1e-6,
            "duration {}",
            clip.duration_beats
        );
        // Fixture Volume=0.5 → relative to Cubase 0 dB unity (~0.2927) ≈ 1.708.
        assert!(
            (clip.gain - cubase_event_gain(0.5)).abs() < 1e-5,
            "gain {}",
            clip.gain
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn media_is_resolved_next_to_the_archive_not_at_the_recorded_path() {
        let dir = archive_dir("media");
        let project = import(&dir.join("Song.xml")).unwrap();

        assert_eq!(project.assets.len(), 1);
        let asset = &project.assets[0];
        assert_eq!(asset.original_filename, "kick.wav");
        assert_eq!(
            asset.absolute_path.as_deref(),
            Some(dir.join("Audio/kick.wav").as_path())
        );
        assert_eq!(asset.sample_rate, Some(44_100));
        assert_eq!(asset.channels, Some(2));
        assert_eq!(asset.duration_samples, Some(44_100));
        assert_eq!(asset.duration_secs, Some(1.0));

        match &project.tracks[0].clips[0].source {
            ClipSource::Audio {
                asset_id,
                source_path,
            } => {
                assert_eq!(asset_id, &asset.id);
                assert_eq!(
                    source_path.as_deref(),
                    Some(dir.join("Audio/kick.wav").as_path())
                );
            }
            other => panic!("expected an audio clip source, got {other:?}"),
        }

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cubase_default_event_volume_maps_to_unity_gain() {
        assert!((cubase_event_gain(CUBASE_EVENT_VOLUME_UNITY) - 1.0).abs() < 1e-5);
        assert!((cubase_event_gain(0.0) - 1.0).abs() < 1e-5);
        // ≈ +7.82 dB relative boost still lands inside our 4.0 clamp.
        let boosted = cubase_event_gain(0.720_092_95);
        assert!(boosted > 2.0 && boosted < 3.0, "boosted={boosted}");
    }

    #[test]
    fn musical_start_is_ppq_not_samples() {
        // Minimal musical-track event: Start=251520 ticks → 524 beats, Length is
        // still samples so duration stays 26.5 s (= 91 beats @206 BPM).
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<tracklist>
   <obj class="PArrangeSetup" name="Setup" ID="1">
      <float name="SampleRate" value="44100"/>
   </obj>
   <obj class="MAudioTrackEvent" ID="10">
      <obj class="MListNode" name="Node" ID="11">
         <string name="Name" value="GTSOLO" wide="true"/>
         <member name="Domain">
            <int name="Type" value="0"/>
            <obj class="MTempoTrackEvent" name="Tempo Track" ID="12">
               <list name="TempoEvent" type="obj">
                  <obj class="MTempoEvent" ID="13">
                     <float name="BPM" value="206"/>
                     <float name="PPQ" value="0"/>
                  </obj>
               </list>
            </obj>
         </member>
         <list name="Events" type="obj">
            <obj class="MAudioEvent" ID="16">
               <float name="Start" value="251520"/>
               <float name="Length" value="1168864"/>
               <float name="Volume" value="0.29268288612365723"/>
               <string name="Description" value="GT2" wide="true"/>
               <obj class="PAudioClip" name="AudioClip" ID="17">
                  <string name="Name" value="GT2" wide="true"/>
                  <obj class="FNPath" name="Path" ID="18">
                     <string name="Name" value="kick.wav" wide="true"/>
                     <string name="Path" value="C:\Somewhere\Else\Audio\" wide="true"/>
                  </obj>
                  <obj class="AudioCluster" name="Cluster" ID="19">
                     <list name="Substreams" type="obj">
                        <obj class="AudioFile" ID="20">
                           <obj name="FPath" ID="18"/>
                           <int name="FrameCount" value="1168864"/>
                           <member name="SpeakerArr">
                              <list name="Type" type="int">
                                 <item value="1"/>
                                 <item value="2"/>
                              </list>
                           </member>
                           <float name="Rate" value="44100"/>
                        </obj>
                     </list>
                  </obj>
               </obj>
            </obj>
         </list>
      </obj>
   </obj>
</tracklist>
"#;
        let dir = std::env::temp_dir().join(format!(
            "fb_cubase_musical_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir_all(dir.join("Audio")).unwrap();
        fs::write(dir.join("Audio/kick.wav"), b"RIFF").unwrap();
        let path = dir.join("Song.xml");
        fs::write(&path, xml).unwrap();
        let project = import(&path).unwrap();
        let clip = &project.tracks[0].clips[0];
        assert!(
            (clip.start_beat - 524.0).abs() < 1e-6,
            "start {}",
            clip.start_beat
        );
        assert!(
            (clip.duration_beats - 91.0).abs() < 1e-3,
            "duration {}",
            clip.duration_beats
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn archive_without_audio_tracks_is_rejected() {
        let dir = archive_dir("empty");
        let path = dir.join("Empty.xml");
        fs::write(&path, "<?xml version=\"1.0\"?><tracklist></tracklist>").unwrap();

        let error = import(&path).unwrap_err();
        assert!(matches!(error, ProjectError::Corrupted(_)), "got {error:?}");

        fs::remove_dir_all(&dir).ok();
    }

    /// Local ad-hoc dump against the real NIKKE archive (not run in CI).
    #[test]
    #[ignore = "requires local /home/arizkami/Downloads/Nikke1/NIKKE.xml"]
    fn dump_nikke_archive() {
        let path = PathBuf::from("/home/arizkami/Downloads/Nikke1/NIKKE.xml");
        let project = import(&path).expect("import");
        eprintln!(
            "nikke: tracks={} bpm={} sr={} sig={}/{} tempo_pts={} assets={} missing_check_paths={}",
            project.tracks.len(),
            project.settings.bpm,
            project.settings.sample_rate,
            project.settings.time_sig_num,
            project.settings.time_sig_den,
            project.settings.tempo_points.len(),
            project.assets.len(),
            project
                .assets
                .iter()
                .filter(|a| a
                    .absolute_path
                    .as_ref()
                    .map(|p| !p.is_file())
                    .unwrap_or(true))
                .count(),
        );
        for track in &project.tracks {
            let clip = track.clips.first();
            eprintln!(
                "  track='{}' clips={} first_start={:?} first_dur={:?} gain={:?} source={:?}",
                track.name,
                track.clips.len(),
                clip.map(|c| c.start_beat),
                clip.map(|c| c.duration_beats),
                clip.map(|c| c.gain),
                clip.map(|c| match &c.source {
                    ClipSource::Audio { source_path, .. } => source_path
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default(),
                    _ => String::new(),
                }),
            );
        }
    }
}
