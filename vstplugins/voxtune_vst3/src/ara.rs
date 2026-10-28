//! ARA 2: Voxtune as a document controller and a plug-in extension.
//!
//! The host's ARA model maps onto Voxtune like this:
//!
//! - **Audio source** → the take. Its samples are read through the host's
//!   audio reader on a thread of our own once the host enables access, mixed
//!   to mono at the engine rate, and analysed once.
//! - **Audio modification** → one set of edits of that take: its own engine
//!   ([`Shared`]) and [`VoxDocument`]. Several regions may play one
//!   modification.
//! - **Playback region** → a stretch of a modification placed on the
//!   timeline. A plug-in instance bound as playback renderer plays the regions
//!   the host assigns it, following the host transport.
//! - **Editor view** → the region the host selects is the one the editor
//!   shows.
//!
//! Edits are stored in the host's ARA archive as JSON keyed by the
//! modification's persistent ID; a restored document carries its notes, so a
//! reopened project needs no new analysis.
//!
//! Threads: every document-controller callback comes on the host's main
//! thread; the reader thread touches only the host's audio reader (allowed
//! from any thread) and our own state; the audio thread only `try_lock`s.
//! Model updates are queued and reported in `notifyModelUpdates`, as ARA
//! requires. No callback unwinds into the host.

#![allow(non_snake_case, clippy::missing_safety_doc)]

use std::collections::HashMap;
use std::ffi::{c_char, c_void, CStr};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::thread::JoinHandle;

use ara2_bridge_sys::*;
use parking_lot::Mutex;
use sphere_voxtune::audio_io::{compute_waveform_peaks, resample_mono_cubic, DecodedAudio};
use sphere_voxtune::engine::{analyze_samples, Analysis, AnalysisProgress, CantoEngine, ORN_NONE};
use sphere_voxtune::session::VoxDocument;
use sphere_voxtune::shared::Shared;

/// Host rate, set by the first `initialize` of any instance. Engines are made
/// at this rate so they play at the host's speed.
pub static HOST_RATE: AtomicU32 = AtomicU32::new(0);

const ARCHIVE_VERSION: u32 = 1;

fn guard<R>(fallback: R, f: impl FnOnce() -> R) -> R {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(fallback)
}

/// Raw pointer that may cross threads (host refs are opaque handles).
#[derive(Debug, Clone, Copy)]
struct Handle(*mut c_void);
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

unsafe fn c_string(s: *const c_char) -> String {
    if s.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(s) }.to_string_lossy().into_owned()
    }
}

// =============================================================================
// Factory
// =============================================================================

struct SyncFactory(ARAFactory);
unsafe impl Sync for SyncFactory {}

/// Must equal the VST3 class name (`Plugin::NAME`): hosts pair them by name.
pub const PLUGIN_NAME: &CStr = c"Solfege Voxtune";

static FACTORY: SyncFactory = SyncFactory(ARAFactory {
    structSize: std::mem::size_of::<ARAFactory>(),
    lowestSupportedApiGeneration: kARAAPIGeneration_2_0_Final as ARAAPIGeneration,
    highestSupportedApiGeneration: kARAAPIGeneration_2_3_Final as ARAAPIGeneration,
    factoryID: c"com.futureboard.solfege-voxtune.ara".as_ptr(),
    initializeARAWithConfiguration: Some(initialize_ara),
    uninitializeARA: Some(uninitialize_ara),
    plugInName: PLUGIN_NAME.as_ptr(),
    manufacturerName: c"Futureboard".as_ptr(),
    informationURL: c"https://futureboard.studio".as_ptr(),
    version: c"0.1.0".as_ptr(),
    createDocumentControllerWithDocument: Some(create_document_controller),
    documentArchiveID: c"com.futureboard.solfege-voxtune.archive.1".as_ptr(),
    compatibleDocumentArchiveIDsCount: 0,
    compatibleDocumentArchiveIDs: ptr::null(),
    analyzeableContentTypesCount: 0,
    analyzeableContentTypes: ptr::null(),
    // No time-stretching: a region plays its modification 1:1.
    supportedPlaybackTransformationFlags: kARAPlaybackTransformationNoChanges as i32,
    supportsStoringAudioFileChunks: kARAFalse,
});

pub fn factory() -> *const c_void {
    &FACTORY.0 as *const ARAFactory as *const c_void
}

unsafe extern "C" fn initialize_ara(_config: *const ARAInterfaceConfiguration) {}
unsafe extern "C" fn uninitialize_ara() {}

// =============================================================================
// Model
// =============================================================================

/// What the host lent the document controller.
struct HostApi {
    audio_ref: Handle,
    audio: *const ARAAudioAccessControllerInterface,
    archive_ref: Handle,
    archive: *const ARAArchivingControllerInterface,
    update_ref: Handle,
    update: *const ARAModelUpdateControllerInterface,
}
unsafe impl Send for HostApi {}
unsafe impl Sync for HostApi {}

/// Reader state of an audio source.
const SOURCE_EMPTY: u32 = 0;
const SOURCE_READING: u32 = 1;
const SOURCE_ANALYZING: u32 = 2;
const SOURCE_READY: u32 = 3;
const SOURCE_FAILED: u32 = 4;

pub struct AudioSource {
    host_ref: Handle,
    pub name: Mutex<String>,
    persistent_id: Mutex<String>,
    sample_rate: Mutex<f64>,
    channels: Mutex<i32>,
    sample_count: Mutex<i64>,
    state: AtomicU32,
    /// Last state reported to the host.
    reported: AtomicU32,
    progress: Arc<AnalysisProgress>,
    reader: Mutex<Option<(JoinHandle<()>, Arc<AtomicBool>)>>,
    /// Mono samples at the engine rate, once read.
    audio: Mutex<Option<Arc<DecodedAudio>>>,
    /// The analysis new modifications start from.
    analysis: Mutex<Option<Analysis>>,
    modifications: Mutex<Vec<Weak<AudioModification>>>,
}

pub struct AudioModification {
    host_ref: Handle,
    pub source: Arc<AudioSource>,
    persistent_id: Mutex<String>,
    pub name: Mutex<String>,
    /// Plays the edits; created at the engine rate.
    pub engine: Arc<Shared>,
    /// Current edits (None until the take is loaded).
    pub doc: Mutex<Option<VoxDocument>>,
    /// Edits restored from an archive, installed once the take is read.
    restored: Mutex<Option<VoxDocument>>,
    /// Bumped when the engine got (new) audio or edits from outside the
    /// editor (load, restore).
    pub generation: AtomicU64,
    /// Edited since the host was last told.
    dirty: AtomicBool,
}

pub struct PlaybackRegion {
    /// The host's handle (kept for future content notifications).
    _host_ref: Handle,
    pub modification: Arc<AudioModification>,
    start_mod: AtomicU64,
    start_play: AtomicU64,
    dur_play: AtomicU64,
    pub name: Mutex<String>,
}

impl PlaybackRegion {
    pub fn start_in_modification(&self) -> f64 {
        f64::from_bits(self.start_mod.load(Ordering::Relaxed))
    }
    pub fn start_in_playback(&self) -> f64 {
        f64::from_bits(self.start_play.load(Ordering::Relaxed))
    }
    pub fn duration(&self) -> f64 {
        f64::from_bits(self.dur_play.load(Ordering::Relaxed))
    }

    unsafe fn set_props(&self, p: *const ARAPlaybackRegionProperties) {
        if p.is_null() {
            return;
        }
        let p = unsafe { ptr::read_unaligned(p) };
        self.start_mod
            .store(p.startInModificationTime.to_bits(), Ordering::Relaxed);
        self.start_play
            .store(p.startInPlaybackTime.to_bits(), Ordering::Relaxed);
        self.dur_play
            .store(p.durationInPlaybackTime.to_bits(), Ordering::Relaxed);
        if p.structSize > std::mem::offset_of!(ARAPlaybackRegionProperties, name) {
            *self.name.lock() = unsafe { c_string(p.name) };
        }
    }
}

impl AudioModification {
    /// The editor committed an edit: keep it for the archive and tell the
    /// host on its next `notifyModelUpdates`.
    pub fn store_edit(&self, doc: &VoxDocument) {
        *self.doc.lock() = Some(doc.clone());
        self.dirty.store(true, Ordering::Release);
    }

    /// Installs the source's audio with `doc` (or the source's analysis).
    fn install(&self, audio: &DecodedAudio, doc: Option<VoxDocument>) {
        let analysis_doc = match doc {
            Some(d) => d,
            None => match self.source.analysis.lock().as_ref() {
                Some(a) => VoxDocument {
                    notes: a.notes.clone(),
                    key: a.key,
                    scale: a.scale,
                    bpm: a.bpm,
                    ..VoxDocument::default()
                },
                None => return,
            },
        };
        {
            let mut engine = self.engine.lock();
            engine.install_analyzed(audio.clone(), analysis_doc.to_analysis());
            analysis_doc.apply_to(&mut engine);
        }
        *self.doc.lock() = Some(analysis_doc);
        self.generation.fetch_add(1, Ordering::Release);
    }
}

/// The document controller's state, shared with bound plug-in instances.
pub struct DcShared {
    host: HostApi,
    sources: Mutex<Vec<Arc<AudioSource>>>,
    modifications: Mutex<Vec<Arc<AudioModification>>>,
    regions: Mutex<Vec<Arc<PlaybackRegion>>>,
    /// Archive being restored: persistent ID → edits.
    restoring: Mutex<Option<HashMap<String, VoxDocument>>>,
}

#[repr(C)]
struct DocumentController {
    instance: ARADocumentControllerInstance,
    shared: Arc<DcShared>,
}

unsafe fn dc<'a>(r: ARADocumentControllerRef) -> &'a DocumentController {
    unsafe { &*(r as *const DocumentController) }
}

/// Our objects are the `Arc`s the controller owns; the host holds their
/// addresses.
unsafe fn obj<'a, T>(r: *mut impl Sized) -> &'a T {
    unsafe { &*(r as *const T) }
}

fn find<T>(list: &[Arc<T>], r: *const c_void) -> Option<Arc<T>> {
    list.iter()
        .find(|a| Arc::as_ptr(a) as *const c_void == r)
        .cloned()
}

fn engine_rate(source_rate: f64) -> u32 {
    match HOST_RATE.load(Ordering::Relaxed) {
        0 => source_rate.round().max(8_000.0) as u32,
        r => r,
    }
}

// =============================================================================
// Reading and analysis
// =============================================================================

impl DcShared {
    /// Starts reading `source` through the host's audio reader, then
    /// analysing it, unless it is already loaded or loading.
    fn start_reading(self: &Arc<Self>, source: &Arc<AudioSource>) {
        let state = source.state.load(Ordering::Acquire);
        if state == SOURCE_READY || state == SOURCE_ANALYZING || state == SOURCE_READING {
            return;
        }
        source.state.store(SOURCE_READING, Ordering::Release);
        let cancel = Arc::new(AtomicBool::new(false));
        let dc = self.clone();
        let src = source.clone();
        let stop = cancel.clone();
        let handle = std::thread::Builder::new()
            .name("voxtune-ara-read".into())
            .spawn(move || {
                let ok = guard(false, || dc.read_and_analyze(&src, &stop));
                if !ok && !stop.load(Ordering::Relaxed) {
                    src.state.store(SOURCE_FAILED, Ordering::Release);
                }
            });
        match handle {
            Ok(h) => *source.reader.lock() = Some((h, cancel)),
            Err(_) => source.state.store(SOURCE_FAILED, Ordering::Release),
        }
    }

    /// Stops a running reader and waits for it (before the host revokes
    /// access or destroys the source).
    fn stop_reading(&self, source: &AudioSource) {
        let job = source.reader.lock().take();
        if let Some((handle, cancel)) = job {
            cancel.store(true, Ordering::Relaxed);
            source.progress.cancel.store(true, Ordering::Relaxed);
            let _ = handle.join();
            source.progress.cancel.store(false, Ordering::Relaxed);
            if source.state.load(Ordering::Acquire) != SOURCE_READY {
                source.state.store(SOURCE_EMPTY, Ordering::Release);
            }
        }
    }

    fn read_and_analyze(&self, source: &Arc<AudioSource>, cancel: &AtomicBool) -> bool {
        let rate = *source.sample_rate.lock();
        let channels = (*source.channels.lock()).max(1) as usize;
        let count = (*source.sample_count.lock()).max(0) as usize;
        if count == 0 || rate <= 0.0 {
            return false;
        }
        // Read every channel and mix to mono.
        let mono = unsafe {
            let audio = &*self.host.audio;
            let (Some(create), Some(read), Some(destroy)) = (
                audio.createAudioReaderForSource,
                audio.readAudioSamples,
                audio.destroyAudioReader,
            ) else {
                return false;
            };
            let reader = create(
                self.host.audio_ref.0 as ARAAudioAccessControllerHostRef,
                source.host_ref.0 as ARAAudioSourceHostRef,
                kARAFalse,
            );
            if reader.is_null() {
                return false;
            }
            const CHUNK: usize = 1 << 16;
            let mut mono = Vec::with_capacity(count);
            let mut buffers = vec![vec![0.0f32; CHUNK]; channels];
            let mut pos = 0usize;
            let mut ok = true;
            while pos < count {
                if cancel.load(Ordering::Relaxed) {
                    ok = false;
                    break;
                }
                let n = CHUNK.min(count - pos);
                let ptrs: Vec<*mut c_void> = buffers
                    .iter_mut()
                    .map(|b| b.as_mut_ptr() as *mut c_void)
                    .collect();
                if read(
                    self.host.audio_ref.0 as ARAAudioAccessControllerHostRef,
                    reader,
                    pos as ARASamplePosition,
                    n as ARASampleCount,
                    ptrs.as_ptr(),
                ) == kARAFalse
                {
                    ok = false;
                    break;
                }
                for i in 0..n {
                    let sum: f32 = buffers.iter().map(|b| b[i]).sum();
                    mono.push(sum / channels as f32);
                }
                pos += n;
                source
                    .progress
                    .permille
                    .store((pos * 300 / count) as u32, Ordering::Relaxed);
            }
            destroy(
                self.host.audio_ref.0 as ARAAudioAccessControllerHostRef,
                reader,
            );
            if !ok {
                return false;
            }
            mono
        };

        // Resample to the engine rate, then analyse unless every
        // modification already has restored edits.
        let target = engine_rate(rate);
        let src_rate = rate.round() as u32;
        let samples = if src_rate == target {
            mono
        } else {
            resample_mono_cubic(&mono, src_rate, target)
        };
        let audio = Arc::new(DecodedAudio {
            duration_sec: samples.len() as f64 / target as f64,
            waveform_peaks: compute_waveform_peaks(&samples, 240),
            samples,
            sample_rate: target,
            source_sample_rate: src_rate,
            source_channels: channels,
        });
        *source.audio.lock() = Some(audio.clone());
        let mods: Vec<Arc<AudioModification>> = source
            .modifications
            .lock()
            .iter()
            .filter_map(Weak::upgrade)
            .collect();
        let all_restored = !mods.is_empty() && mods.iter().all(|m| m.restored.lock().is_some());
        if !all_restored {
            source.state.store(SOURCE_ANALYZING, Ordering::Release);
            let Some(analysis) = analyze_samples(&audio.samples, target as f32, &source.progress)
            else {
                return false;
            };
            *source.analysis.lock() = Some(analysis);
        }
        for m in &mods {
            let restored = m.restored.lock().take();
            m.install(&audio, restored);
        }
        source.state.store(SOURCE_READY, Ordering::Release);
        true
    }
}

// =============================================================================
// Archive
// =============================================================================

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Archive {
    version: u32,
    /// Modification persistent ID → edits.
    modifications: HashMap<String, VoxDocument>,
}

impl DcShared {
    unsafe fn read_archive(&self, reader: ARAArchiveReaderHostRef) -> Option<Archive> {
        let api = unsafe { &*self.host.archive };
        let size = unsafe { api.getArchiveSize?(self.host.archive_ref.0 as _, reader) };
        if size == 0 {
            return Some(Archive::default());
        }
        let mut bytes = vec![0u8; size];
        let ok = unsafe {
            api.readBytesFromArchive?(
                self.host.archive_ref.0 as _,
                reader,
                0,
                size,
                bytes.as_mut_ptr(),
            )
        };
        if ok == kARAFalse {
            return None;
        }
        serde_json::from_slice(&bytes).ok()
    }

    unsafe fn write_archive(&self, writer: ARAArchiveWriterHostRef, archive: &Archive) -> bool {
        let Ok(bytes) = serde_json::to_vec(archive) else {
            return false;
        };
        let api = unsafe { &*self.host.archive };
        let Some(write) = api.writeBytesToArchive else {
            return false;
        };
        unsafe {
            write(
                self.host.archive_ref.0 as _,
                writer,
                0,
                bytes.len(),
                bytes.as_ptr(),
            ) != kARAFalse
        }
    }

    fn archive_of(&self, mods: &[Arc<AudioModification>]) -> Archive {
        Archive {
            version: ARCHIVE_VERSION,
            modifications: mods
                .iter()
                .filter_map(|m| {
                    let doc = m.doc.lock().clone().or_else(|| m.restored.lock().clone())?;
                    Some((m.persistent_id.lock().clone(), doc))
                })
                .collect(),
        }
    }

    /// Applies restored edits to `m`: now if its take is loaded, else when it is.
    fn restore_into(&self, m: &Arc<AudioModification>, doc: VoxDocument) {
        let audio = m.source.audio.lock().clone();
        match audio {
            Some(audio) => m.install(&audio, Some(doc)),
            None => *m.restored.lock() = Some(doc),
        }
    }
}

// =============================================================================
// Document controller callbacks
// =============================================================================

unsafe extern "C" fn create_document_controller(
    host: *const ARADocumentControllerHostInstance,
    _properties: *const ARADocumentProperties,
) -> *const ARADocumentControllerInstance {
    guard(ptr::null(), || unsafe {
        if host.is_null() {
            return ptr::null();
        }
        let h = ptr::read_unaligned(host);
        if h.audioAccessControllerInterface.is_null()
            || h.archivingControllerInterface.is_null()
            || h.modelUpdateControllerInterface.is_null()
        {
            return ptr::null();
        }
        let shared = Arc::new(DcShared {
            host: HostApi {
                audio_ref: Handle(h.audioAccessControllerHostRef as *mut c_void),
                audio: h.audioAccessControllerInterface,
                archive_ref: Handle(h.archivingControllerHostRef as *mut c_void),
                archive: h.archivingControllerInterface,
                update_ref: Handle(h.modelUpdateControllerHostRef as *mut c_void),
                update: h.modelUpdateControllerInterface,
            },
            sources: Mutex::new(Vec::new()),
            modifications: Mutex::new(Vec::new()),
            regions: Mutex::new(Vec::new()),
            restoring: Mutex::new(None),
        });
        let dc = Box::into_raw(Box::new(DocumentController {
            instance: ARADocumentControllerInstance {
                structSize: std::mem::size_of::<ARADocumentControllerInstance>(),
                documentControllerRef: ptr::null_mut(),
                documentControllerInterface: &DC_INTERFACE.0,
            },
            shared,
        }));
        (*dc).instance.documentControllerRef = dc as ARADocumentControllerRef;
        ptr::addr_of!((*dc).instance)
    })
}

unsafe extern "C" fn destroy_document_controller(r: ARADocumentControllerRef) {
    guard((), || unsafe {
        let dc = Box::from_raw(r as *mut DocumentController);
        let sources: Vec<_> = dc.shared.sources.lock().drain(..).collect();
        for s in &sources {
            dc.shared.stop_reading(s);
        }
        drop(dc);
    })
}

unsafe extern "C" fn get_factory(_r: ARADocumentControllerRef) -> *const ARAFactory {
    &FACTORY.0
}

unsafe extern "C" fn no_op(_r: ARADocumentControllerRef) {}

/// Reports analysis progress and edited modifications to the host.
unsafe extern "C" fn notify_model_updates(r: ARADocumentControllerRef) {
    guard((), || unsafe {
        let s = &dc(r).shared;
        let api = &*s.host.update;
        let host = s.host.update_ref.0 as ARAModelUpdateControllerHostRef;
        for src in s.sources.lock().iter() {
            let state = src.state.load(Ordering::Acquire);
            let reported = src.reported.swap(state, Ordering::AcqRel);
            if let Some(notify) = api.notifyAudioSourceAnalysisProgress {
                let busy = state == SOURCE_READING || state == SOURCE_ANALYZING;
                let value = src.progress.permille.load(Ordering::Relaxed) as f32 / 1000.0;
                if busy && reported != SOURCE_READING && reported != SOURCE_ANALYZING {
                    notify(
                        host,
                        src.host_ref.0 as _,
                        kARAAnalysisProgressStarted as i32,
                        0.0,
                    );
                } else if busy {
                    notify(
                        host,
                        src.host_ref.0 as _,
                        kARAAnalysisProgressUpdated as i32,
                        value,
                    );
                } else if state != reported && (state == SOURCE_READY || state == SOURCE_FAILED) {
                    notify(
                        host,
                        src.host_ref.0 as _,
                        kARAAnalysisProgressCompleted as i32,
                        1.0,
                    );
                }
            }
        }
        if let Some(notify) = api.notifyAudioModificationContentChanged {
            for m in s.modifications.lock().iter() {
                if m.dirty.swap(false, Ordering::AcqRel) {
                    notify(
                        host,
                        m.host_ref.0 as _,
                        ptr::null(),
                        kARAContentUpdateEverythingChanged as i32,
                    );
                }
            }
        }
    })
}

unsafe extern "C" fn begin_restoring_document(
    r: ARADocumentControllerRef,
    reader: ARAArchiveReaderHostRef,
) -> ARABool {
    guard(kARAFalse, || unsafe {
        let s = &dc(r).shared;
        match s.read_archive(reader) {
            Some(a) => {
                *s.restoring.lock() = Some(a.modifications);
                kARATrue
            }
            None => kARAFalse,
        }
    })
}

unsafe extern "C" fn end_restoring_document(
    r: ARADocumentControllerRef,
    _reader: ARAArchiveReaderHostRef,
) -> ARABool {
    guard(kARAFalse, || unsafe {
        let s = &dc(r).shared;
        let Some(mut docs) = s.restoring.lock().take() else {
            return kARATrue;
        };
        let mods: Vec<_> = s.modifications.lock().clone();
        for m in &mods {
            let id = m.persistent_id.lock().clone();
            if let Some(doc) = docs.remove(&id) {
                s.restore_into(m, doc);
            }
        }
        kARATrue
    })
}

unsafe extern "C" fn store_document(
    r: ARADocumentControllerRef,
    writer: ARAArchiveWriterHostRef,
) -> ARABool {
    guard(kARAFalse, || unsafe {
        let s = &dc(r).shared;
        let mods: Vec<_> = s.modifications.lock().clone();
        if s.write_archive(writer, &s.archive_of(&mods)) {
            kARATrue
        } else {
            kARAFalse
        }
    })
}

unsafe extern "C" fn store_objects(
    r: ARADocumentControllerRef,
    writer: ARAArchiveWriterHostRef,
    filter: *const ARAStoreObjectsFilter,
) -> ARABool {
    guard(kARAFalse, || unsafe {
        let s = &dc(r).shared;
        let all: Vec<_> = s.modifications.lock().clone();
        let mods = if filter.is_null() {
            all
        } else {
            let f = ptr::read_unaligned(filter);
            let refs: &[ARAAudioModificationRef] = if f.audioModificationRefs.is_null() {
                &[]
            } else {
                std::slice::from_raw_parts(f.audioModificationRefs, f.audioModificationRefsCount)
            };
            refs.iter()
                .filter_map(|&r| find(&all, r as *const c_void))
                .collect()
        };
        if s.write_archive(writer, &s.archive_of(&mods)) {
            kARATrue
        } else {
            kARAFalse
        }
    })
}

unsafe extern "C" fn restore_objects(
    r: ARADocumentControllerRef,
    reader: ARAArchiveReaderHostRef,
    filter: *const ARARestoreObjectsFilter,
) -> ARABool {
    guard(kARAFalse, || unsafe {
        let s = &dc(r).shared;
        let Some(mut archive) = s.read_archive(reader) else {
            return kARAFalse;
        };
        // Archive ID → current ID, when the host renamed objects.
        let mut rename: HashMap<String, String> = HashMap::new();
        if !filter.is_null() {
            let f = ptr::read_unaligned(filter);
            if !f.audioModificationArchiveIDs.is_null() && !f.audioModificationCurrentIDs.is_null()
            {
                let n = f.audioModificationIDsCount;
                let from = std::slice::from_raw_parts(f.audioModificationArchiveIDs, n);
                let to = std::slice::from_raw_parts(f.audioModificationCurrentIDs, n);
                for (a, b) in from.iter().zip(to) {
                    rename.insert(c_string(*a), c_string(*b));
                }
            }
        }
        let docs: HashMap<String, VoxDocument> = archive
            .modifications
            .drain()
            .map(|(id, doc)| (rename.get(&id).cloned().unwrap_or(id), doc))
            .collect();
        let mods: Vec<_> = s.modifications.lock().clone();
        for m in &mods {
            let id = m.persistent_id.lock().clone();
            if let Some(doc) = docs.get(&id) {
                s.restore_into(m, doc.clone());
            }
        }
        kARATrue
    })
}

unsafe extern "C" fn update_document_properties(
    _r: ARADocumentControllerRef,
    _p: *const ARADocumentProperties,
) {
}

unsafe extern "C" fn create_musical_context(
    _r: ARADocumentControllerRef,
    host_ref: ARAMusicalContextHostRef,
    _p: *const ARAMusicalContextProperties,
) -> ARAMusicalContextRef {
    // Tempo and key come from the take's own analysis; the context is only
    // a handle the host needs back.
    host_ref as ARAMusicalContextRef
}

unsafe extern "C" fn update_musical_context_properties(
    _r: ARADocumentControllerRef,
    _c: ARAMusicalContextRef,
    _p: *const ARAMusicalContextProperties,
) {
}

unsafe extern "C" fn update_musical_context_content(
    _r: ARADocumentControllerRef,
    _c: ARAMusicalContextRef,
    _range: *const ARAContentTimeRange,
    _flags: ARAContentUpdateFlags,
) {
}

unsafe extern "C" fn destroy_musical_context(
    _r: ARADocumentControllerRef,
    _c: ARAMusicalContextRef,
) {
}

unsafe fn source_props(src: &AudioSource, p: *const ARAAudioSourceProperties) {
    if p.is_null() {
        return;
    }
    let p = unsafe { ptr::read_unaligned(p) };
    *src.name.lock() = unsafe { c_string(p.name) };
    *src.persistent_id.lock() = unsafe { c_string(p.persistentID) };
    *src.sample_rate.lock() = p.sampleRate;
    *src.channels.lock() = p.channelCount;
    *src.sample_count.lock() = p.sampleCount;
}

unsafe extern "C" fn create_audio_source(
    r: ARADocumentControllerRef,
    host_ref: ARAAudioSourceHostRef,
    p: *const ARAAudioSourceProperties,
) -> ARAAudioSourceRef {
    guard(ptr::null_mut(), || unsafe {
        let src = Arc::new(AudioSource {
            host_ref: Handle(host_ref as *mut c_void),
            name: Mutex::new(String::new()),
            persistent_id: Mutex::new(String::new()),
            sample_rate: Mutex::new(0.0),
            channels: Mutex::new(1),
            sample_count: Mutex::new(0),
            state: AtomicU32::new(SOURCE_EMPTY),
            reported: AtomicU32::new(SOURCE_EMPTY),
            progress: Arc::new(AnalysisProgress::default()),
            reader: Mutex::new(None),
            audio: Mutex::new(None),
            analysis: Mutex::new(None),
            modifications: Mutex::new(Vec::new()),
        });
        source_props(&src, p);
        let handle = Arc::as_ptr(&src) as ARAAudioSourceRef;
        dc(r).shared.sources.lock().push(src);
        handle
    })
}

unsafe extern "C" fn update_audio_source_properties(
    _r: ARADocumentControllerRef,
    s: ARAAudioSourceRef,
    p: *const ARAAudioSourceProperties,
) {
    guard((), || unsafe { source_props(obj::<AudioSource>(s), p) })
}

unsafe extern "C" fn update_audio_source_content(
    r: ARADocumentControllerRef,
    s: ARAAudioSourceRef,
    _range: *const ARAContentTimeRange,
    flags: ARAContentUpdateFlags,
) {
    guard((), || unsafe {
        // Only a change to the samples themselves needs a new read.
        if flags & kARAContentUpdateSignalScopeRemainsUnchanged as i32 != 0 {
            return;
        }
        let shared = &dc(r).shared;
        if let Some(src) = find(&shared.sources.lock(), s as *const c_void) {
            let reading = src.reader.lock().is_some();
            shared.stop_reading(&src);
            src.state.store(SOURCE_EMPTY, Ordering::Release);
            *src.audio.lock() = None;
            if reading {
                shared.start_reading(&src);
            }
        }
    })
}

unsafe extern "C" fn enable_audio_source_samples_access(
    r: ARADocumentControllerRef,
    s: ARAAudioSourceRef,
    enable: ARABool,
) {
    guard((), || unsafe {
        let shared = &dc(r).shared;
        let Some(src) = find(&shared.sources.lock(), s as *const c_void) else {
            return;
        };
        if enable != kARAFalse {
            shared.start_reading(&src);
        } else {
            // Access ends: the reader must be done with the host's reader.
            shared.stop_reading(&src);
        }
    })
}

unsafe extern "C" fn deactivate_audio_source(
    _r: ARADocumentControllerRef,
    _s: ARAAudioSourceRef,
    _deactivate: ARABool,
) {
}

unsafe extern "C" fn destroy_audio_source(r: ARADocumentControllerRef, s: ARAAudioSourceRef) {
    guard((), || unsafe {
        let shared = &dc(r).shared;
        let src = {
            let mut list = shared.sources.lock();
            let i = list
                .iter()
                .position(|a| Arc::as_ptr(a) as *const c_void == s as *const c_void);
            i.map(|i| list.remove(i))
        };
        if let Some(src) = src {
            shared.stop_reading(&src);
        }
    })
}

fn new_modification(
    source: Arc<AudioSource>,
    host_ref: ARAAudioModificationHostRef,
    p: *const ARAAudioModificationProperties,
    doc: Option<VoxDocument>,
) -> Arc<AudioModification> {
    let (name, id) = if p.is_null() {
        (String::new(), String::new())
    } else {
        let p = unsafe { ptr::read_unaligned(p) };
        unsafe { (c_string(p.name), c_string(p.persistentID)) }
    };
    let rate = engine_rate(*source.sample_rate.lock());
    let m = Arc::new(AudioModification {
        host_ref: Handle(host_ref as *mut c_void),
        source: source.clone(),
        persistent_id: Mutex::new(id),
        name: Mutex::new(name),
        engine: Arc::new(Shared::new(CantoEngine::new(rate as f32))),
        doc: Mutex::new(None),
        restored: Mutex::new(None),
        generation: AtomicU64::new(1),
        dirty: AtomicBool::new(false),
    });
    source.modifications.lock().push(Arc::downgrade(&m));
    // A take already read: install at once (with the copied edits, if any).
    let audio = source.audio.lock().clone();
    match audio {
        Some(audio) => m.install(&audio, doc),
        None => *m.restored.lock() = doc,
    }
    m
}

unsafe extern "C" fn create_audio_modification(
    r: ARADocumentControllerRef,
    s: ARAAudioSourceRef,
    host_ref: ARAAudioModificationHostRef,
    p: *const ARAAudioModificationProperties,
) -> ARAAudioModificationRef {
    guard(ptr::null_mut(), || unsafe {
        let shared = &dc(r).shared;
        let Some(src) = find(&shared.sources.lock(), s as *const c_void) else {
            return ptr::null_mut();
        };
        let m = new_modification(src, host_ref, p, None);
        // During a document restore the edits arrive before the objects.
        let id = m.persistent_id.lock().clone();
        if let Some(doc) = shared
            .restoring
            .lock()
            .as_mut()
            .and_then(|docs| docs.remove(&id))
        {
            shared.restore_into(&m, doc);
        }
        let handle = Arc::as_ptr(&m) as ARAAudioModificationRef;
        shared.modifications.lock().push(m);
        handle
    })
}

unsafe extern "C" fn clone_audio_modification(
    r: ARADocumentControllerRef,
    m: ARAAudioModificationRef,
    host_ref: ARAAudioModificationHostRef,
    p: *const ARAAudioModificationProperties,
) -> ARAAudioModificationRef {
    guard(ptr::null_mut(), || unsafe {
        let shared = &dc(r).shared;
        let Some(from) = find(&shared.modifications.lock(), m as *const c_void) else {
            return ptr::null_mut();
        };
        let doc = from
            .doc
            .lock()
            .clone()
            .or_else(|| from.restored.lock().clone());
        let copy = new_modification(from.source.clone(), host_ref, p, doc);
        let handle = Arc::as_ptr(&copy) as ARAAudioModificationRef;
        shared.modifications.lock().push(copy);
        handle
    })
}

unsafe extern "C" fn update_audio_modification_properties(
    _r: ARADocumentControllerRef,
    m: ARAAudioModificationRef,
    p: *const ARAAudioModificationProperties,
) {
    guard((), || unsafe {
        if p.is_null() {
            return;
        }
        let m = obj::<AudioModification>(m);
        let p = ptr::read_unaligned(p);
        *m.name.lock() = c_string(p.name);
        *m.persistent_id.lock() = c_string(p.persistentID);
    })
}

unsafe extern "C" fn deactivate_audio_modification(
    _r: ARADocumentControllerRef,
    _m: ARAAudioModificationRef,
    _deactivate: ARABool,
) {
}

unsafe extern "C" fn destroy_audio_modification(
    r: ARADocumentControllerRef,
    m: ARAAudioModificationRef,
) {
    guard((), || unsafe {
        dc(r)
            .shared
            .modifications
            .lock()
            .retain(|a| Arc::as_ptr(a) as *const c_void != m as *const c_void);
    })
}

unsafe extern "C" fn create_playback_region(
    r: ARADocumentControllerRef,
    m: ARAAudioModificationRef,
    host_ref: ARAPlaybackRegionHostRef,
    p: *const ARAPlaybackRegionProperties,
) -> ARAPlaybackRegionRef {
    guard(ptr::null_mut(), || unsafe {
        let shared = &dc(r).shared;
        let Some(modification) = find(&shared.modifications.lock(), m as *const c_void) else {
            return ptr::null_mut();
        };
        let region = Arc::new(PlaybackRegion {
            _host_ref: Handle(host_ref as *mut c_void),
            modification,
            start_mod: AtomicU64::new(0f64.to_bits()),
            start_play: AtomicU64::new(0f64.to_bits()),
            dur_play: AtomicU64::new(0f64.to_bits()),
            name: Mutex::new(String::new()),
        });
        region.set_props(p);
        let handle = Arc::as_ptr(&region) as ARAPlaybackRegionRef;
        shared.regions.lock().push(region);
        handle
    })
}

unsafe extern "C" fn update_playback_region_properties(
    _r: ARADocumentControllerRef,
    pr: ARAPlaybackRegionRef,
    p: *const ARAPlaybackRegionProperties,
) {
    guard((), || unsafe { obj::<PlaybackRegion>(pr).set_props(p) })
}

unsafe extern "C" fn destroy_playback_region(
    r: ARADocumentControllerRef,
    pr: ARAPlaybackRegionRef,
) {
    guard((), || unsafe {
        dc(r)
            .shared
            .regions
            .lock()
            .retain(|a| Arc::as_ptr(a) as *const c_void != pr as *const c_void);
    })
}

// Content readers: Voxtune doesn't publish analysed content to the host.

unsafe extern "C" fn source_content_available(
    _r: ARADocumentControllerRef,
    _s: ARAAudioSourceRef,
    _t: ARAContentType,
) -> ARABool {
    kARAFalse
}

unsafe extern "C" fn source_content_incomplete(
    _r: ARADocumentControllerRef,
    s: ARAAudioSourceRef,
    _t: ARAContentType,
) -> ARABool {
    let state = unsafe { obj::<AudioSource>(s) }
        .state
        .load(Ordering::Acquire);
    if state == SOURCE_READY {
        kARAFalse
    } else {
        kARATrue
    }
}

unsafe extern "C" fn request_source_analysis(
    _r: ARADocumentControllerRef,
    _s: ARAAudioSourceRef,
    _n: ARASize,
    _t: *const ARAContentType,
) {
}

unsafe extern "C" fn source_content_grade(
    _r: ARADocumentControllerRef,
    _s: ARAAudioSourceRef,
    _t: ARAContentType,
) -> ARAContentGrade {
    kARAContentGradeInitial as ARAContentGrade
}

unsafe extern "C" fn create_source_reader(
    _r: ARADocumentControllerRef,
    _s: ARAAudioSourceRef,
    _t: ARAContentType,
    _range: *const ARAContentTimeRange,
) -> ARAContentReaderRef {
    ptr::null_mut()
}

unsafe extern "C" fn modification_content_available(
    _r: ARADocumentControllerRef,
    _m: ARAAudioModificationRef,
    _t: ARAContentType,
) -> ARABool {
    kARAFalse
}

unsafe extern "C" fn modification_content_grade(
    _r: ARADocumentControllerRef,
    _m: ARAAudioModificationRef,
    _t: ARAContentType,
) -> ARAContentGrade {
    kARAContentGradeInitial as ARAContentGrade
}

unsafe extern "C" fn create_modification_reader(
    _r: ARADocumentControllerRef,
    _m: ARAAudioModificationRef,
    _t: ARAContentType,
    _range: *const ARAContentTimeRange,
) -> ARAContentReaderRef {
    ptr::null_mut()
}

unsafe extern "C" fn region_content_available(
    _r: ARADocumentControllerRef,
    _p: ARAPlaybackRegionRef,
    _t: ARAContentType,
) -> ARABool {
    kARAFalse
}

unsafe extern "C" fn region_content_grade(
    _r: ARADocumentControllerRef,
    _p: ARAPlaybackRegionRef,
    _t: ARAContentType,
) -> ARAContentGrade {
    kARAContentGradeInitial as ARAContentGrade
}

unsafe extern "C" fn create_region_reader(
    _r: ARADocumentControllerRef,
    _p: ARAPlaybackRegionRef,
    _t: ARAContentType,
    _range: *const ARAContentTimeRange,
) -> ARAContentReaderRef {
    ptr::null_mut()
}

unsafe extern "C" fn reader_event_count(
    _r: ARADocumentControllerRef,
    _c: ARAContentReaderRef,
) -> ARAInt32 {
    0
}

unsafe extern "C" fn reader_event(
    _r: ARADocumentControllerRef,
    _c: ARAContentReaderRef,
    _i: ARAInt32,
) -> *const c_void {
    ptr::null()
}

unsafe extern "C" fn destroy_reader(_r: ARADocumentControllerRef, _c: ARAContentReaderRef) {}

unsafe extern "C" fn create_region_sequence(
    _r: ARADocumentControllerRef,
    host_ref: ARARegionSequenceHostRef,
    _p: *const ARARegionSequenceProperties,
) -> ARARegionSequenceRef {
    host_ref as ARARegionSequenceRef
}

unsafe extern "C" fn update_region_sequence(
    _r: ARADocumentControllerRef,
    _s: ARARegionSequenceRef,
    _p: *const ARARegionSequenceProperties,
) {
}

unsafe extern "C" fn destroy_region_sequence(
    _r: ARADocumentControllerRef,
    _s: ARARegionSequenceRef,
) {
}

unsafe extern "C" fn head_and_tail(
    _r: ARADocumentControllerRef,
    _p: ARAPlaybackRegionRef,
    head: *mut ARATimeDuration,
    tail: *mut ARATimeDuration,
) {
    unsafe {
        if !head.is_null() {
            *head = 0.0;
        }
        if !tail.is_null() {
            *tail = 0.0;
        }
    }
}

unsafe extern "C" fn algorithms_count(_r: ARADocumentControllerRef) -> ARAInt32 {
    0
}

unsafe extern "C" fn algorithm_properties(
    _r: ARADocumentControllerRef,
    _i: ARAInt32,
) -> *const ARAProcessingAlgorithmProperties {
    ptr::null()
}

unsafe extern "C" fn algorithm_for_source(
    _r: ARADocumentControllerRef,
    _s: ARAAudioSourceRef,
) -> ARAInt32 {
    0
}

unsafe extern "C" fn request_algorithm(
    _r: ARADocumentControllerRef,
    _s: ARAAudioSourceRef,
    _i: ARAInt32,
) {
}

unsafe extern "C" fn is_licensed(
    _r: ARADocumentControllerRef,
    _dialog: ARABool,
    _n: ARASize,
    _t: *const ARAContentType,
    _flags: ARAPlaybackTransformationFlags,
) -> ARABool {
    kARATrue
}

unsafe extern "C" fn store_source_to_file_chunk(
    _r: ARADocumentControllerRef,
    _w: ARAArchiveWriterHostRef,
    _s: ARAAudioSourceRef,
    _id: *mut ARAPersistentID,
    _open: *mut ARABool,
) -> ARABool {
    kARAFalse
}

/// True while the modification plays the take unchanged.
unsafe extern "C" fn preserves_signal(
    _r: ARADocumentControllerRef,
    m: ARAAudioModificationRef,
) -> ARABool {
    guard(kARAFalse, || unsafe {
        let m = obj::<AudioModification>(m);
        let doc = m.doc.lock();
        let unchanged = doc.as_ref().is_none_or(|d| {
            d.notes.iter().all(|n| {
                (n.pitch - n.detected).abs() < 1e-4
                    && (n.start - n.detected_start).abs() < 1e-5
                    && (n.dur - n.detected_dur).abs() < 1e-5
                    && (n.gain_db - n.detected_gain).abs() < 1e-3
                    && n.formant.abs() < 1e-4
                    && (n.modulation - 1.0).abs() < 1e-4
                    && !n.muted
                    && n.orn_style == ORN_NONE
                    && n.accent <= 0.0
            })
        });
        if unchanged {
            kARATrue
        } else {
            kARAFalse
        }
    })
}

struct SyncDcInterface(ARADocumentControllerInterface);
unsafe impl Sync for SyncDcInterface {}

static DC_INTERFACE: SyncDcInterface = SyncDcInterface(ARADocumentControllerInterface {
    structSize: std::mem::size_of::<ARADocumentControllerInterface>(),
    destroyDocumentController: Some(destroy_document_controller),
    getFactory: Some(get_factory),
    beginEditing: Some(no_op),
    endEditing: Some(no_op),
    notifyModelUpdates: Some(notify_model_updates),
    beginRestoringDocumentFromArchive: Some(begin_restoring_document),
    endRestoringDocumentFromArchive: Some(end_restoring_document),
    storeDocumentToArchive: Some(store_document),
    updateDocumentProperties: Some(update_document_properties),
    createMusicalContext: Some(create_musical_context),
    updateMusicalContextProperties: Some(update_musical_context_properties),
    updateMusicalContextContent: Some(update_musical_context_content),
    destroyMusicalContext: Some(destroy_musical_context),
    createAudioSource: Some(create_audio_source),
    updateAudioSourceProperties: Some(update_audio_source_properties),
    updateAudioSourceContent: Some(update_audio_source_content),
    enableAudioSourceSamplesAccess: Some(enable_audio_source_samples_access),
    deactivateAudioSourceForUndoHistory: Some(deactivate_audio_source),
    destroyAudioSource: Some(destroy_audio_source),
    createAudioModification: Some(create_audio_modification),
    cloneAudioModification: Some(clone_audio_modification),
    updateAudioModificationProperties: Some(update_audio_modification_properties),
    deactivateAudioModificationForUndoHistory: Some(deactivate_audio_modification),
    destroyAudioModification: Some(destroy_audio_modification),
    createPlaybackRegion: Some(create_playback_region),
    updatePlaybackRegionProperties: Some(update_playback_region_properties),
    destroyPlaybackRegion: Some(destroy_playback_region),
    isAudioSourceContentAvailable: Some(source_content_available),
    isAudioSourceContentAnalysisIncomplete: Some(source_content_incomplete),
    requestAudioSourceContentAnalysis: Some(request_source_analysis),
    getAudioSourceContentGrade: Some(source_content_grade),
    createAudioSourceContentReader: Some(create_source_reader),
    isAudioModificationContentAvailable: Some(modification_content_available),
    getAudioModificationContentGrade: Some(modification_content_grade),
    createAudioModificationContentReader: Some(create_modification_reader),
    isPlaybackRegionContentAvailable: Some(region_content_available),
    getPlaybackRegionContentGrade: Some(region_content_grade),
    createPlaybackRegionContentReader: Some(create_region_reader),
    getContentReaderEventCount: Some(reader_event_count),
    getContentReaderDataForEvent: Some(reader_event),
    destroyContentReader: Some(destroy_reader),
    createRegionSequence: Some(create_region_sequence),
    updateRegionSequenceProperties: Some(update_region_sequence),
    destroyRegionSequence: Some(destroy_region_sequence),
    getPlaybackRegionHeadAndTailTime: Some(head_and_tail),
    restoreObjectsFromArchive: Some(restore_objects),
    storeObjectsToArchive: Some(store_objects),
    getProcessingAlgorithmsCount: Some(algorithms_count),
    getProcessingAlgorithmProperties: Some(algorithm_properties),
    getProcessingAlgorithmForAudioSource: Some(algorithm_for_source),
    requestProcessingAlgorithmForAudioSource: Some(request_algorithm),
    isLicensedForCapabilities: Some(is_licensed),
    storeAudioSourceToAudioFileChunk: Some(store_source_to_file_chunk),
    isAudioModificationPreservingAudioSourceSignal: Some(preserves_signal),
});

// =============================================================================
// Plug-in extension (one per bound instance)
// =============================================================================

/// What a bound plug-in instance shares with its audio thread and editor.
pub struct BindingShared {
    pub dc: Arc<DcShared>,
    pub roles: i32,
    /// Regions this instance renders (playback renderer role).
    pub regions: Mutex<Vec<Arc<PlaybackRegion>>>,
    /// The region the host selected for the editor.
    pub focused: Mutex<Option<Arc<PlaybackRegion>>>,
    pub focus_generation: AtomicU64,
}

impl BindingShared {
    pub fn plays(&self) -> bool {
        self.roles & kARAPlaybackRendererRole as i32 != 0
    }

    /// The region the editor shows: the host's selection, else the first
    /// region this instance plays.
    pub fn focus(&self) -> Option<Arc<PlaybackRegion>> {
        self.focused
            .lock()
            .clone()
            .or_else(|| self.regions.lock().first().cloned())
    }

    /// Reading / analysis state of the focused region's take, for the editor.
    pub fn focus_status(&self) -> Option<(String, Option<f32>, bool)> {
        let region = self.focus()?;
        let src = &region.modification.source;
        let state = src.state.load(Ordering::Acquire);
        let name = src.name.lock().clone();
        let p = src.progress.permille.load(Ordering::Relaxed) as f32 / 1000.0;
        Some(match state {
            SOURCE_READING | SOURCE_ANALYZING => (name, Some(p), false),
            SOURCE_FAILED => (name, None, true),
            _ => (name, None, false),
        })
    }
}

#[repr(C)]
pub struct Binding {
    extension: ARAPlugInExtensionInstance,
    pub shared: Arc<BindingShared>,
}

// The extension record only holds our own address and static interface
// tables; the host reads it, we never mutate it after binding.
unsafe impl Send for Binding {}
unsafe impl Sync for Binding {}

unsafe fn binding<'a>(r: *mut c_void) -> &'a Binding {
    unsafe { &*(r as *const Binding) }
}

/// Binds an instance to the controller `dc_ref`. The returned box must live
/// as long as the plug-in instance.
pub unsafe fn bind(dc_ref: *mut c_void, _known: i32, assigned: i32) -> Option<Box<Binding>> {
    if dc_ref.is_null() {
        return None;
    }
    let dc = unsafe { dc(dc_ref as ARADocumentControllerRef) };
    let shared = Arc::new(BindingShared {
        dc: dc.shared.clone(),
        roles: assigned,
        regions: Mutex::new(Vec::new()),
        focused: Mutex::new(None),
        focus_generation: AtomicU64::new(0),
    });
    let mut b = Box::new(Binding {
        extension: ARAPlugInExtensionInstance {
            structSize: std::mem::size_of::<ARAPlugInExtensionInstance>(),
            plugInExtensionRef: ptr::null_mut(),
            plugInExtensionInterface: &EXTENSION_INTERFACE.0,
            playbackRendererRef: ptr::null_mut(),
            playbackRendererInterface: &PLAYBACK_INTERFACE.0,
            editorRendererRef: ptr::null_mut(),
            editorRendererInterface: &EDITOR_RENDERER_INTERFACE.0,
            editorViewRef: ptr::null_mut(),
            editorViewInterface: &EDITOR_VIEW_INTERFACE.0,
        },
        shared,
    });
    let me = &mut *b as *mut Binding as *mut c_void;
    b.extension.plugInExtensionRef = me as _;
    b.extension.playbackRendererRef = me as _;
    b.extension.editorRendererRef = me as _;
    b.extension.editorViewRef = me as _;
    Some(b)
}

impl Binding {
    pub fn extension(&self) -> *const c_void {
        &self.extension as *const ARAPlugInExtensionInstance as *const c_void
    }
}

fn region_by_ref(b: &Binding, r: ARAPlaybackRegionRef) -> Option<Arc<PlaybackRegion>> {
    find(&b.shared.dc.regions.lock(), r as *const c_void)
}

unsafe extern "C" fn add_region(r: ARAPlaybackRendererRef, region: ARAPlaybackRegionRef) {
    guard((), || unsafe {
        let b = binding(r as *mut c_void);
        if let Some(region) = region_by_ref(b, region) {
            let mut list = b.shared.regions.lock();
            if !list.iter().any(|x| Arc::ptr_eq(x, &region)) {
                list.push(region);
            }
        }
    })
}

unsafe extern "C" fn remove_region(r: ARAPlaybackRendererRef, region: ARAPlaybackRegionRef) {
    guard((), || unsafe {
        let b = binding(r as *mut c_void);
        b.shared
            .regions
            .lock()
            .retain(|x| Arc::as_ptr(x) as *const c_void != region as *const c_void);
        let mut focused = b.shared.focused.lock();
        if focused
            .as_ref()
            .is_some_and(|x| Arc::as_ptr(x) as *const c_void == region as *const c_void)
        {
            *focused = None;
            b.shared.focus_generation.fetch_add(1, Ordering::Release);
        }
    })
}

unsafe extern "C" fn extension_set_region(r: ARAPlugInExtensionRef, region: ARAPlaybackRegionRef) {
    unsafe { add_region(r as ARAPlaybackRendererRef, region) }
}

unsafe extern "C" fn extension_remove_region(
    r: ARAPlugInExtensionRef,
    region: ARAPlaybackRegionRef,
) {
    unsafe { remove_region(r as ARAPlaybackRendererRef, region) }
}

unsafe extern "C" fn editor_add_region(_r: ARAEditorRendererRef, _p: ARAPlaybackRegionRef) {}
unsafe extern "C" fn editor_remove_region(_r: ARAEditorRendererRef, _p: ARAPlaybackRegionRef) {}
unsafe extern "C" fn editor_add_sequence(_r: ARAEditorRendererRef, _s: ARARegionSequenceRef) {}
unsafe extern "C" fn editor_remove_sequence(_r: ARAEditorRendererRef, _s: ARARegionSequenceRef) {}

unsafe extern "C" fn notify_selection(r: ARAEditorViewRef, selection: *const ARAViewSelection) {
    guard((), || unsafe {
        if selection.is_null() {
            return;
        }
        let b = binding(r as *mut c_void);
        let s = ptr::read_unaligned(selection);
        if s.playbackRegionRefsCount == 0 || s.playbackRegionRefs.is_null() {
            return;
        }
        let first = *s.playbackRegionRefs;
        if let Some(region) = region_by_ref(b, first) {
            *b.shared.focused.lock() = Some(region);
            b.shared.focus_generation.fetch_add(1, Ordering::Release);
        }
    })
}

unsafe extern "C" fn notify_hide_sequences(
    _r: ARAEditorViewRef,
    _n: ARASize,
    _s: *const ARARegionSequenceRef,
) {
}

struct SyncExtension(ARAPlugInExtensionInterface);
unsafe impl Sync for SyncExtension {}
static EXTENSION_INTERFACE: SyncExtension = SyncExtension(ARAPlugInExtensionInterface {
    structSize: std::mem::size_of::<ARAPlugInExtensionInterface>(),
    setPlaybackRegion: Some(extension_set_region),
    removePlaybackRegion: Some(extension_remove_region),
});

struct SyncPlayback(ARAPlaybackRendererInterface);
unsafe impl Sync for SyncPlayback {}
static PLAYBACK_INTERFACE: SyncPlayback = SyncPlayback(ARAPlaybackRendererInterface {
    structSize: std::mem::size_of::<ARAPlaybackRendererInterface>(),
    addPlaybackRegion: Some(add_region),
    removePlaybackRegion: Some(remove_region),
});

struct SyncEditorRenderer(ARAEditorRendererInterface);
unsafe impl Sync for SyncEditorRenderer {}
static EDITOR_RENDERER_INTERFACE: SyncEditorRenderer =
    SyncEditorRenderer(ARAEditorRendererInterface {
        structSize: std::mem::size_of::<ARAEditorRendererInterface>(),
        addPlaybackRegion: Some(editor_add_region),
        removePlaybackRegion: Some(editor_remove_region),
        addRegionSequence: Some(editor_add_sequence),
        removeRegionSequence: Some(editor_remove_sequence),
    });

struct SyncEditorView(ARAEditorViewInterface);
unsafe impl Sync for SyncEditorView {}
static EDITOR_VIEW_INTERFACE: SyncEditorView = SyncEditorView(ARAEditorViewInterface {
    structSize: std::mem::size_of::<ARAEditorViewInterface>(),
    notifySelection: Some(notify_selection),
    notifyHideRegionSequences: Some(notify_hide_sequences),
});

// =============================================================================
// Rendering (audio thread)
// =============================================================================

/// Renders this instance's regions at host time `pos` into `left` / `right`
/// (cleared first), using `scratch` (2 × frames). Realtime: `try_lock` only,
/// no allocation. A region whose engine isn't at the host rate stays silent.
pub fn render_regions(
    shared: &BindingShared,
    pos: f64,
    playing: bool,
    rate: f32,
    left: &mut [f32],
    mut right: Option<&mut [f32]>,
    scratch: &mut [f32],
) {
    left.fill(0.0);
    if let Some(r) = right.as_deref_mut() {
        r.fill(0.0);
    }
    if !playing {
        return;
    }
    let Some(regions) = shared.regions.try_lock() else {
        return;
    };
    let frames = left.len();
    let block = frames as f64 / rate as f64;
    for region in regions.iter() {
        let start = region.start_in_playback();
        let end = start + region.duration();
        if pos + block <= start || pos >= end {
            continue;
        }
        let engine = &region.modification.engine;
        if (engine.sample_rate() - rate).abs() > 0.5 {
            continue;
        }
        let out = &mut scratch[..frames * 2];
        let take_sec = region.start_in_modification() + (pos - start);
        if !engine.render_following(take_sec, true, out) {
            continue;
        }
        // Past the region's end the take keeps going: keep it out.
        let last = (((end - pos) * rate as f64).ceil().max(0.0) as usize).min(frames);
        for i in 0..last {
            left[i] += out[i * 2];
        }
        if let Some(r) = right.as_deref_mut() {
            for i in 0..last {
                r[i] += out[i * 2 + 1];
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! A fake ARA host drives the document controller through its C
    //! interface, as a DAW would.

    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::{Duration, Instant};

    const SR: f64 = 48_000.0;
    const SECONDS: f64 = 2.0;

    static ARCHIVE: Mutex<Vec<u8>> = Mutex::new(Vec::new());
    static PROGRESS_DONE: AtomicUsize = AtomicUsize::new(0);
    static CONTENT_CHANGED: AtomicUsize = AtomicUsize::new(0);

    /// Two sung-like notes: A3 for the first second, C4 for the second.
    fn sample(i: i64) -> f32 {
        let t = i as f64 / SR;
        let hz = if t < 1.0 { 220.0 } else { 261.63 };
        let env = ((t % 1.0) * 20.0).min(1.0) * ((1.0 - t % 1.0) * 20.0).min(1.0);
        (env * 0.4 * (std::f64::consts::TAU * hz * t).sin()) as f32
    }

    unsafe extern "C" fn create_reader(
        _c: ARAAudioAccessControllerHostRef,
        _s: ARAAudioSourceHostRef,
        _b: ARABool,
    ) -> ARAAudioReaderHostRef {
        8 as ARAAudioReaderHostRef
    }

    unsafe extern "C" fn read_samples(
        _c: ARAAudioAccessControllerHostRef,
        _r: ARAAudioReaderHostRef,
        pos: ARASamplePosition,
        count: ARASampleCount,
        buffers: *const *mut c_void,
    ) -> ARABool {
        for ch in 0..2 {
            let out = unsafe {
                std::slice::from_raw_parts_mut(*buffers.add(ch) as *mut f32, count as usize)
            };
            for (k, s) in out.iter_mut().enumerate() {
                *s = sample(pos + k as i64);
            }
        }
        kARATrue
    }

    unsafe extern "C" fn destroy_reader_cb(
        _c: ARAAudioAccessControllerHostRef,
        _r: ARAAudioReaderHostRef,
    ) {
    }

    unsafe extern "C" fn archive_size(
        _c: ARAArchivingControllerHostRef,
        _r: ARAArchiveReaderHostRef,
    ) -> ARASize {
        ARCHIVE.lock().len()
    }

    unsafe extern "C" fn archive_read(
        _c: ARAArchivingControllerHostRef,
        _r: ARAArchiveReaderHostRef,
        pos: ARASize,
        len: ARASize,
        buf: *mut ARAByte,
    ) -> ARABool {
        let a = ARCHIVE.lock();
        unsafe { ptr::copy_nonoverlapping(a[pos..pos + len].as_ptr(), buf, len) };
        kARATrue
    }

    unsafe extern "C" fn archive_write(
        _c: ARAArchivingControllerHostRef,
        _w: ARAArchiveWriterHostRef,
        pos: ARASize,
        len: ARASize,
        buf: *const ARAByte,
    ) -> ARABool {
        let mut a = ARCHIVE.lock();
        a.resize(pos + len, 0);
        unsafe { ptr::copy_nonoverlapping(buf, a[pos..].as_mut_ptr(), len) };
        kARATrue
    }

    unsafe extern "C" fn progress(
        _c: ARAModelUpdateControllerHostRef,
        _s: ARAAudioSourceHostRef,
        state: ARAAnalysisProgressState,
        _v: f32,
    ) {
        if state == kARAAnalysisProgressCompleted as i32 {
            PROGRESS_DONE.fetch_add(1, Ordering::Relaxed);
        }
    }

    unsafe extern "C" fn modification_changed(
        _c: ARAModelUpdateControllerHostRef,
        _m: ARAAudioModificationHostRef,
        _r: *const ARAContentTimeRange,
        _f: ARAContentUpdateFlags,
    ) {
        CONTENT_CHANGED.fetch_add(1, Ordering::Relaxed);
    }

    static AUDIO: SyncAudio = SyncAudio(ARAAudioAccessControllerInterface {
        structSize: std::mem::size_of::<ARAAudioAccessControllerInterface>(),
        createAudioReaderForSource: Some(create_reader),
        readAudioSamples: Some(read_samples),
        destroyAudioReader: Some(destroy_reader_cb),
    });
    struct SyncAudio(ARAAudioAccessControllerInterface);
    unsafe impl Sync for SyncAudio {}

    static ARCHIVING: SyncArchive = SyncArchive(ARAArchivingControllerInterface {
        structSize: std::mem::size_of::<ARAArchivingControllerInterface>(),
        getArchiveSize: Some(archive_size),
        readBytesFromArchive: Some(archive_read),
        writeBytesToArchive: Some(archive_write),
        notifyDocumentArchivingProgress: None,
        notifyDocumentUnarchivingProgress: None,
        getDocumentArchiveID: None,
    });
    struct SyncArchive(ARAArchivingControllerInterface);
    unsafe impl Sync for SyncArchive {}

    static UPDATES: SyncUpdates = SyncUpdates(ARAModelUpdateControllerInterface {
        structSize: std::mem::size_of::<ARAModelUpdateControllerInterface>(),
        notifyAudioSourceAnalysisProgress: Some(progress),
        notifyAudioSourceContentChanged: None,
        notifyAudioModificationContentChanged: Some(modification_changed),
        notifyPlaybackRegionContentChanged: None,
        notifyDocumentDataChanged: None,
    });
    struct SyncUpdates(ARAModelUpdateControllerInterface);
    unsafe impl Sync for SyncUpdates {}

    fn host() -> ARADocumentControllerHostInstance {
        ARADocumentControllerHostInstance {
            structSize: std::mem::size_of::<ARADocumentControllerHostInstance>(),
            audioAccessControllerHostRef: 1 as _,
            audioAccessControllerInterface: &AUDIO.0,
            archivingControllerHostRef: 2 as _,
            archivingControllerInterface: &ARCHIVING.0,
            contentAccessControllerHostRef: ptr::null_mut(),
            contentAccessControllerInterface: ptr::null(),
            modelUpdateControllerHostRef: 3 as _,
            modelUpdateControllerInterface: &UPDATES.0,
            playbackControllerHostRef: ptr::null_mut(),
            playbackControllerInterface: ptr::null(),
        }
    }

    struct Doc {
        r: ARADocumentControllerRef,
        i: &'static ARADocumentControllerInterface,
    }

    unsafe fn open() -> Doc {
        let f = unsafe { &*(factory() as *const ARAFactory) };
        let host = Box::leak(Box::new(host()));
        let props = ARADocumentProperties {
            structSize: std::mem::size_of::<ARADocumentProperties>(),
            name: c"test".as_ptr(),
        };
        let inst = unsafe { f.createDocumentControllerWithDocument.unwrap()(host, &props) };
        assert!(!inst.is_null());
        let inst = unsafe { ptr::read_unaligned(inst) };
        Doc {
            r: inst.documentControllerRef,
            i: unsafe { &*inst.documentControllerInterface },
        }
    }

    unsafe fn add_take(
        d: &Doc,
    ) -> (
        ARAAudioSourceRef,
        ARAAudioModificationRef,
        ARAPlaybackRegionRef,
    ) {
        unsafe {
            let src_props = ARAAudioSourceProperties {
                structSize: std::mem::size_of::<ARAAudioSourceProperties>(),
                name: c"vocal.wav".as_ptr(),
                persistentID: c"src-1".as_ptr(),
                sampleCount: (SR * SECONDS) as i64,
                sampleRate: SR,
                channelCount: 2,
                merits64BitSamples: kARAFalse,
                channelArrangementDataType: 0,
                channelArrangement: ptr::null(),
            };
            let src = d.i.createAudioSource.unwrap()(d.r, 11 as _, &src_props);
            let mod_props = ARAAudioModificationProperties {
                structSize: std::mem::size_of::<ARAAudioModificationProperties>(),
                name: c"vocal".as_ptr(),
                persistentID: c"mod-1".as_ptr(),
            };
            let m = d.i.createAudioModification.unwrap()(d.r, src, 12 as _, &mod_props);
            let region_props = ARAPlaybackRegionProperties {
                structSize: std::mem::size_of::<ARAPlaybackRegionProperties>(),
                transformationFlags: 0,
                startInModificationTime: 0.0,
                durationInModificationTime: SECONDS,
                startInPlaybackTime: 10.0,
                durationInPlaybackTime: SECONDS,
                musicalContextRef: ptr::null_mut(),
                regionSequenceRef: ptr::null_mut(),
                name: c"Verse".as_ptr(),
                color: ptr::null(),
            };
            let region = d.i.createPlaybackRegion.unwrap()(d.r, m, 13 as _, &region_props);
            assert!(!src.is_null() && !m.is_null() && !region.is_null());
            (src, m, region)
        }
    }

    fn wait_ready(m: &AudioModification) {
        let start = Instant::now();
        while m.source.state.load(Ordering::Acquire) != SOURCE_READY {
            assert!(
                start.elapsed() < Duration::from_secs(60),
                "analysis never finished"
            );
            assert_ne!(m.source.state.load(Ordering::Acquire), SOURCE_FAILED);
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_host_reads_analyses_plays_edits_and_restores_a_take() {
        HOST_RATE.store(SR as u32, Ordering::Relaxed);
        unsafe {
            // Load: the host enables access, we read and analyse.
            let d = open();
            let (src, m_ref, region_ref) = add_take(&d);
            d.i.enableAudioSourceSamplesAccess.unwrap()(d.r, src, kARATrue);
            let m = obj::<AudioModification>(m_ref);
            wait_ready(m);
            d.i.enableAudioSourceSamplesAccess.unwrap()(d.r, src, kARAFalse);
            d.i.notifyModelUpdates.unwrap()(d.r);
            assert!(PROGRESS_DONE.load(Ordering::Relaxed) >= 1);
            let doc = m.doc.lock().clone().expect("edits after load");
            let pitched: Vec<_> = doc.notes.iter().filter(|n| !n.unpitched).collect();
            assert!(pitched.len() >= 2, "{} notes", doc.notes.len());
            assert!(pitched.iter().any(|n| (n.detected - 57.0).abs() < 0.5));
            assert!(pitched.iter().any(|n| (n.detected - 60.0).abs() < 0.5));
            assert_eq!(
                d.i.isAudioModificationPreservingAudioSourceSignal.unwrap()(d.r, m_ref),
                kARATrue
            );

            // Play: a bound instance renders its region on the timeline.
            let binding = bind(d.r as *mut c_void, 7, 7).unwrap();
            let ext = ptr::read_unaligned(binding.extension() as *const ARAPlugInExtensionInstance);
            (*ext.playbackRendererInterface).addPlaybackRegion.unwrap()(
                ext.playbackRendererRef,
                region_ref,
            );
            let mut left = vec![0.0f32; 1024];
            let mut right = vec![0.0f32; 1024];
            let mut scratch = vec![0.0f32; 2048];
            let mut peak = 0.0f32;
            for k in 0..20 {
                let pos = 10.3 + k as f64 * 1024.0 / SR;
                render_regions(
                    &binding.shared,
                    pos,
                    true,
                    SR as f32,
                    &mut left,
                    Some(&mut right),
                    &mut scratch,
                );
                peak = left.iter().fold(peak, |p, s| p.max(s.abs()));
            }
            assert!(peak > 0.05, "region is silent ({peak})");
            render_regions(
                &binding.shared,
                4.0,
                true,
                SR as f32,
                &mut left,
                Some(&mut right),
                &mut scratch,
            );
            assert!(left.iter().all(|&s| s == 0.0), "audible outside the region");

            // Edit: the host hears about it on its next model update.
            let mut edited = doc.clone();
            let first = edited.notes.iter_mut().find(|n| !n.unpitched).unwrap();
            first.pitch += 2.0;
            let raised = first.pitch;
            m.store_edit(&edited);
            d.i.notifyModelUpdates.unwrap()(d.r);
            assert_eq!(CONTENT_CHANGED.load(Ordering::Relaxed), 1);
            assert_eq!(
                d.i.isAudioModificationPreservingAudioSourceSignal.unwrap()(d.r, m_ref),
                kARAFalse
            );

            // Save, then reopen in a new document: the edit comes back from
            // the archive without a new analysis.
            ARCHIVE.lock().clear();
            assert_eq!(d.i.storeDocumentToArchive.unwrap()(d.r, 5 as _), kARATrue);
            assert!(String::from_utf8_lossy(&ARCHIVE.lock()).contains("mod-1"));
            (*ext.playbackRendererInterface)
                .removePlaybackRegion
                .unwrap()(ext.playbackRendererRef, region_ref);
            drop(binding);
            d.i.destroyDocumentController.unwrap()(d.r);

            let d2 = open();
            assert_eq!(
                d2.i.beginRestoringDocumentFromArchive.unwrap()(d2.r, 6 as _),
                kARATrue
            );
            let (src2, m2_ref, _) = add_take(&d2);
            assert_eq!(
                d2.i.endRestoringDocumentFromArchive.unwrap()(d2.r, 6 as _),
                kARATrue
            );
            d2.i.enableAudioSourceSamplesAccess.unwrap()(d2.r, src2, kARATrue);
            let m2 = obj::<AudioModification>(m2_ref);
            wait_ready(m2);
            assert!(m2.source.analysis.lock().is_none(), "analysed again");
            let restored = m2.doc.lock().clone().unwrap();
            let first = restored.notes.iter().find(|n| !n.unpitched).unwrap();
            assert!((first.pitch - raised).abs() < 1e-5);
            assert!(m2
                .engine
                .lock()
                .notes
                .iter()
                .any(|n| (n.pitch - raised).abs() < 1e-5));
            d2.i.destroyDocumentController.unwrap()(d2.r);
        }
    }
}
