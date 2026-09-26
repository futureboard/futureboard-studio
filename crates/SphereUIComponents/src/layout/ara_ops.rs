//! ARA session ownership.
//!
//! Binding an audio clip to an ARA plug-in means four things have to agree: the
//! clip's [`ClipAraBinding`], the ARA document graph, the plug-in instance the
//! engine renders, and the saved archive. This module owns all four and is the
//! only place they are changed together.
//!
//! # Shape
//!
//! One ARA session per **(plug-in, track)**. A session owns one in-process VST3
//! instance, one ARA document, and one playback renderer, and every ARA clip on
//! that track shares them. That is what the engine's per-track renderer model
//! requires — a renderer's output lands in exactly one track's buffer — and it
//! matches how ARA is deployed in DAWs that host the plug-in as a track insert.
//!
//! # Threading
//!
//! [`sphere_ara_host::AraSession`] is the ARA model thread and lives here, on
//! the GPUI main thread. The plug-in calls back on its own threads (sample
//! reads), from its editor (transport requests), or re-entrantly from inside
//! the session's own calls (content reads, and model updates during
//! [`AraState::notify_model_updates`]); those callbacks land on the provider
//! objects below, which own everything they need and hand results to the UI
//! through a bounded queue (transport requests) or per-session counters (model
//! updates) read each frame. No provider touches GPUI state or the audio
//! thread.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sphere_ara_host::{
    AraAudioAccess, AraClipKey, AraGraph, AraGraphChange, AraHostError, AraModelObserver,
    AraModelUpdate, AraMusicalTimeline, AraRendererId, AraResult, AraRoles, AraSampleReader,
    AraSession, AraSessionConfig, AraSourceKey, AraTransportControl, AraTransportRequest,
};

/// Identifies one ARA session: a plug-in hosted on one track.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AraSessionKey {
    pub plugin_id: String,
    pub track_id: String,
}

/// How many pending transport requests the UI will buffer before dropping.
///
/// Bounded on purpose: a plug-in callback must never grow a queue without
/// limit while the UI falls behind. Dropping the oldest keeps the newest
/// request, which is the one the transport should end up following. Model
/// updates do not come through here; see [`ModelSignals`].
const INBOX_CAPACITY: usize = 512;

/// How long a plug-in's document has to have been left alone before a change
/// to it can be taken for the user's: its editor view open, and no host edit
/// and no analysis report on it.
///
/// See [`DocumentWatch`]. Long enough to cover the burst a plug-in sends when
/// its view attaches or an analysis finishes, short enough that a user who
/// opens the editor and starts editing is recognised almost at once.
const USER_EDIT_SETTLE: Duration = Duration::from_secs(2);

/// ARA's `kARAAnalysisProgressStarted`, required first for every analysis.
const ANALYSIS_STARTED: i32 = 0;

/// ARA's `kARAAnalysisProgressCompleted`, required last for every analysis,
/// whether it completed or was cancelled.
const ANALYSIS_COMPLETED: i32 = 2;

/// How long a control-thread ARA change waits for the audio callback to confirm
/// that this track's renderers are out of the graph.
///
/// Long enough to cover a large device block plus scheduling jitter, short
/// enough that a stalled or closed stream does not hang the gesture — the wait
/// simply gives up, and nothing is processing in that case anyway.
const RENDERER_BARRIER_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

/// Gated diagnostic for session lifecycle, sharing the plug-in view debug gate.
///
/// Teardown is a sequence of calls into a plug-in, any one of which can block
/// the main thread; without a line per step a hang says nothing about where it
/// stopped.
fn trace(line: &str) {
    if std::env::var_os("FUTUREBOARD_PLUGIN_VIEW_DEBUG").is_some() {
        eprintln!("[ara-ops] {line}");
    }
}

/// Runs one step of an ARA teardown, announcing it before and timing it after.
///
/// Announced *before* on purpose. Every step here is a call into a plug-in that
/// can block the main thread forever, and when one does, the only evidence of
/// which one is the log — so the last line has to name the call that is stuck,
/// not the last one that finished.
///
/// Not gated behind a debug flag either. This runs when a user removes ARA from
/// a track, not on any hot path, and a flag nobody had set when the app froze is
/// no use to anybody.
fn step<T>(plugin_name: &str, label: &str, work: impl FnOnce() -> T) -> T {
    eprintln!("[ara-close] '{plugin_name}' {label}...");
    let started = std::time::Instant::now();
    let out = work();
    eprintln!(
        "[ara-close] '{plugin_name}' {label} done in {} ms",
        started.elapsed().as_millis()
    );
    out
}

/// Bounded drop-oldest queue shared with plug-in threads.
struct Inbox<T> {
    items: Mutex<std::collections::VecDeque<T>>,
}

impl<T> Default for Inbox<T> {
    fn default() -> Self {
        Self {
            items: Mutex::new(std::collections::VecDeque::with_capacity(INBOX_CAPACITY)),
        }
    }
}

impl<T> Inbox<T> {
    fn push(&self, item: T) {
        if let Ok(mut items) = self.items.lock() {
            if items.len() >= INBOX_CAPACITY {
                items.pop_front();
            }
            items.push_back(item);
        }
    }

    fn drain(&self) -> Vec<T> {
        self.items
            .lock()
            .map(|mut items| items.drain(..).collect())
            .unwrap_or_default()
    }
}

/// Decoded PCM for every ARA audio source, keyed by asset id.
///
/// ARA hands the plug-in random access over the whole source, so the source is
/// decoded once in full rather than streamed: a streaming reader is a
/// forward-biased ring and would underrun on the seeks an analysis pass makes.
/// `load_audio_file` already refuses anything above its in-memory ceiling, so an
/// oversized file fails here instead of thrashing.
#[derive(Default)]
struct AudioLibrary {
    paths: Mutex<HashMap<AraSourceKey, PathBuf>>,
    decoded: Mutex<HashMap<AraSourceKey, Arc<DirectAudio::AudioFileBuffer>>>,
}

impl AudioLibrary {
    fn publish(&self, paths: HashMap<AraSourceKey, PathBuf>) {
        if let Ok(mut slot) = self.paths.lock() {
            *slot = paths;
        }
        // Drop decodes for sources that are no longer referenced, so removing
        // the last ARA clip of a file releases its buffer.
        if let (Ok(paths), Ok(mut decoded)) = (self.paths.lock(), self.decoded.lock()) {
            decoded.retain(|key, _| paths.contains_key(key));
        }
    }

    fn buffer(&self, key: &AraSourceKey) -> AraResult<Arc<DirectAudio::AudioFileBuffer>> {
        if let Ok(decoded) = self.decoded.lock() {
            if let Some(buffer) = decoded.get(key) {
                return Ok(Arc::clone(buffer));
            }
        }
        let path = self
            .paths
            .lock()
            .ok()
            .and_then(|paths| paths.get(key).cloned())
            .ok_or_else(|| {
                AraHostError::host(format!("no media path for ARA source '{}'", key.as_str()))
            })?;
        let buffer = DirectAudio::load_audio_file(&path.to_string_lossy())
            .map(Arc::new)
            .map_err(|error| {
                AraHostError::host(format!("could not decode '{}': {error}", path.display()))
            })?;
        if let Ok(mut decoded) = self.decoded.lock() {
            decoded.insert(key.clone(), Arc::clone(&buffer));
        }
        Ok(buffer)
    }
}

impl AraAudioAccess for AudioLibrary {
    fn open_reader(&self, source: &AraSourceKey) -> AraResult<Box<dyn AraSampleReader>> {
        let buffer = self.buffer(source)?;
        Ok(Box::new(BufferReader { buffer }))
    }
}

/// Planar view over one decoded, interleaved source buffer.
struct BufferReader {
    buffer: Arc<DirectAudio::AudioFileBuffer>,
}

impl AraSampleReader for BufferReader {
    fn channel_count(&self) -> usize {
        self.buffer.channels.max(1)
    }

    fn frame_count(&self) -> i64 {
        self.buffer.frames as i64
    }

    fn read_planar_f32(&mut self, start_frame: i64, out: &mut [&mut [f32]]) -> AraResult<()> {
        let channels = self.channel_count();
        if out.len() != channels {
            return Err(AraHostError::invalid("ARA read channel-count mismatch"));
        }
        let frames = self.buffer.frames as i64;
        for (channel, plane) in out.iter_mut().enumerate() {
            for (offset, sample) in plane.iter_mut().enumerate() {
                let frame = start_frame.saturating_add(offset as i64);
                // ARA permits reads that run past either end of the source; they
                // must come back as silence rather than as an error.
                *sample = if frame < 0 || frame >= frames {
                    0.0
                } else {
                    let index = frame as usize * channels + channel;
                    self.buffer.samples.get(index).copied().unwrap_or(0.0)
                };
            }
        }
        Ok(())
    }
}

/// Queues transport requests from plug-in editors for the UI to apply.
struct TransportBridge {
    inbox: Arc<Inbox<AraTransportRequest>>,
}

impl AraTransportControl for TransportBridge {
    fn request(&self, request: AraTransportRequest) {
        self.inbox.push(request);
    }
}

/// What one session's plug-in has reported about its document, counted
/// rather than queued.
///
/// Not the bounded inbox: that drops its oldest entry when full, and in a large
/// project a burst of analysis progress could push the one report that matters,
/// "the document changed", out of it before the UI drained it. Counters cannot
/// lose it, and they cost no allocation per report. Region and modification
/// content changes are not kept at all, because nothing reads them yet. Plain
/// atomics: ARA has a plug-in report only from inside `notifyModelUpdates` on
/// the model thread, but one that reports from its own threads is served too.
#[derive(Debug, Default)]
struct ModelSignals {
    /// `DocumentDataChanged` reports so far.
    document_changes: AtomicU64,
    /// Analysis progress and source content reports so far: the plug-in
    /// analysing, and the result of an analysis landing. ARA counts an
    /// analysis finishing as a content change of its source.
    analysis_reports: AtomicU64,
    /// Analyses reported started and not yet reported completed.
    analyses_running: AtomicU64,
}

/// One reading of a session's [`ModelSignals`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ModelReport {
    document_changes: u64,
    analysis_reports: u64,
    /// Whether any analysis of the session's sources is still running.
    analysing: bool,
}

impl ModelSignals {
    fn record(&self, update: &AraModelUpdate) {
        match update {
            AraModelUpdate::DocumentDataChanged => {
                self.document_changes.fetch_add(1, Ordering::Relaxed);
            }
            AraModelUpdate::AnalysisProgress { state, .. } => {
                self.analysis_reports.fetch_add(1, Ordering::Relaxed);
                match *state {
                    ANALYSIS_STARTED => {
                        self.analyses_running.fetch_add(1, Ordering::Relaxed);
                    }
                    ANALYSIS_COMPLETED => {
                        // Saturating: a completion whose start was never
                        // reported must not wrap the count.
                        let _ = self.analyses_running.fetch_update(
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                            |running| running.checked_sub(1),
                        );
                    }
                    _ => {}
                }
            }
            AraModelUpdate::SourceContentChanged { .. } => {
                self.analysis_reports.fetch_add(1, Ordering::Relaxed);
            }
            AraModelUpdate::ModificationContentChanged { .. }
            | AraModelUpdate::RegionContentChanged { .. } => {}
        }
    }

    fn report(&self) -> ModelReport {
        ModelReport {
            document_changes: self.document_changes.load(Ordering::Relaxed),
            analysis_reports: self.analysis_reports.load(Ordering::Relaxed),
            analysing: self.analyses_running.load(Ordering::Relaxed) > 0,
        }
    }
}

/// Records one session's plug-in model updates for the UI to read.
///
/// One per session, so what a change means (say, for an archive the plug-in
/// refused) is decided per document.
struct ModelBridge {
    signals: Arc<ModelSignals>,
}

impl AraModelObserver for ModelBridge {
    fn notify(&self, update: AraModelUpdate) {
        self.signals.record(&update);
    }
}

/// One session's reading of its [`ModelReport`]s: which document changes are
/// new, so the project has unsaved work, and whether one of them was the user
/// editing in the plug-in's editor.
///
/// The second matters for an archive the plug-in refused to restore. Saving
/// writes that archive back in place of the live document, which after a
/// failed restore is the plug-in's fresh document and not the user's work.
/// Once the user has edited the live document, it is the newer state and
/// saving has to store it. Guessing that too early replaces the user's saved
/// edits for good, so a document change counts as the user's only when
/// nothing else explains it:
///
/// 1. It was reported by the host's periodic poll, not in answer to one of the
///    host's own edits. The session asks for pending reports right after every
///    edit it makes (graph, timeline, restore), and ARA files what comes back
///    under that edit's undo frame, so those are the plug-in following the
///    host. Everything reported up to the end of a host edit is attributed to
///    it; see [`Self::host_edited`].
/// 2. The plug-in's editor view is open and has been for
///    [`USER_EDIT_SETTLE`]. The user can only edit the document there, and a
///    view that has just attached is building itself, which is not an edit.
/// 3. No analysis of the session's sources is running, and neither a host edit
///    nor an analysis report came within [`USER_EDIT_SETTLE`]. Analysis is
///    requested for every new source, so a fresh document analyses while the
///    user looks at it, and a plug-in may store what it found.
///
/// Each change is judged once, by the poll that first sees it: one that was
/// explained away does not count later. Every change, the user's or not,
/// still marks the project dirty, as it always has.
///
/// Every doubt keeps the archive. An edit the user makes while an analysis
/// runs, or within the settle time of other activity, is not recognised, and a
/// plug-in that never reports an analysis completed keeps the archive for the
/// session's lifetime. That is the cheaper mistake: keeping the archive too
/// long loses only work done in the fresh document after the restore failure
/// was reported, while dropping it too early loses the saved edits for good.
#[derive(Debug, Default)]
struct DocumentWatch {
    /// Document changes already counted towards the project's dirty state.
    counted: u64,
    /// Document changes already judged, or attributed to a host edit.
    judged: u64,
    /// Analysis reports already seen.
    analysis_reports: u64,
    /// The last host edit or new analysis report: activity the user did not
    /// cause.
    last_activity: Option<Instant>,
    /// When the plug-in's editor view was first seen attached, while it stays
    /// attached.
    view_since: Option<Instant>,
}

/// What one [`DocumentWatch::poll`] found.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct DocumentPoll {
    /// The document changed since the last poll.
    changed: bool,
    /// One of those changes was the user editing in the plug-in's editor.
    user_edit: bool,
}

impl DocumentWatch {
    /// The host has just edited the document: everything reported so far was
    /// the plug-in answering that edit.
    ///
    /// Any sync counts, even one that turned out to change nothing; being
    /// cautious costs only a later recognition of the user's edit.
    fn host_edited(&mut self, report: ModelReport, now: Instant) {
        self.judged = report.document_changes;
        self.analysis_reports = report.analysis_reports;
        self.last_activity = Some(now);
    }

    /// Reads what the periodic poll collected, with the editor view's current
    /// state.
    fn poll(&mut self, report: ModelReport, view_attached: bool, now: Instant) -> DocumentPoll {
        self.view_since = if view_attached {
            Some(self.view_since.unwrap_or(now))
        } else {
            None
        };
        if report.analysis_reports != self.analysis_reports {
            self.analysis_reports = report.analysis_reports;
            self.last_activity = Some(now);
        }

        let changed = report.document_changes != self.counted;
        self.counted = report.document_changes;
        let unexplained = report.document_changes != self.judged;
        self.judged = report.document_changes;

        let settled = |since: Instant| now.saturating_duration_since(since) >= USER_EDIT_SETTLE;
        let view_settled = self.view_since.is_some_and(settled);
        let quiet = self.last_activity.is_none_or(settled);
        DocumentPoll {
            changed,
            user_edit: unexplained && view_settled && quiet && !report.analysing,
        }
    }
}

/// What one sync does to bring a session's document up to date; see
/// [`AraState::apply`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SyncPath {
    /// The graph is already in place, so only the musical timeline can have
    /// moved. Regions keep rendering.
    Timeline,
    /// The same regions, some with new properties: the timeline and an
    /// in-place property update. Regions keep rendering, and the renderer
    /// assignments and the editor's selection are left alone.
    Properties,
    /// Renderers out of the engine and rendering off, the graph rebuilt, the
    /// regions re-assigned and the editor told.
    Full,
}

/// Picks the [`SyncPath`] for one sync.
///
/// `change` is asked only when a light path is possible at all. An archive
/// waiting to be restored always takes the full path, because it has to land
/// before the regions are assigned. So does a session whose last full apply
/// did not finish, because the next sync has to redo it. Otherwise only a
/// graph that creates or destroys regions needs the full path.
fn sync_path(
    archive_pending: bool,
    settled: bool,
    change: impl FnOnce() -> AraGraphChange,
) -> SyncPath {
    if archive_pending || !settled {
        return SyncPath::Full;
    }
    match change() {
        AraGraphChange::Unchanged => SyncPath::Timeline,
        AraGraphChange::Properties => SyncPath::Properties,
        AraGraphChange::Structure => SyncPath::Full,
    }
}

/// One ARA plug-in hosted on one track.
struct AraTrackSession {
    session: AraSession,
    /// Keeps the ARA main-factory class instance alive. Declared before
    /// `processor` so it is released before the module that owns it.
    _factory: DirectAudio::AraMainFactory,
    /// The in-process VST3 instance the engine renders. Cloned into the engine's
    /// renderer list; both handles share one C++ processor.
    processor: DirectAudio::Vst3RuntimeProcessor,
    renderer: AraRendererId,
    /// Clips currently assigned to the renderer, in the order of the last full
    /// apply. A property-only sync keeps the same set.
    clips: Vec<AraClipKey>,
    /// Sources this session's clips read from, so the audio library can be
    /// rebuilt without consulting the timeline.
    audio: Arc<AudioLibrary>,
    /// Archive identifier the plug-in writes under, captured at open.
    archive_id: String,
    plugin_name: String,
    /// The saved archive the plug-in refused to restore, written back in
    /// place of the live document until the user edits that document in the
    /// plug-in's editor (see [`DocumentWatch`]).
    ///
    /// Storing the live document instead would overwrite the user's saved
    /// edits with the empty document the failed restore left behind, and the
    /// loss would only show the next time the project was opened.
    unrestored_archive: Option<(String, Vec<u8>)>,
    /// What the plug-in has reported about its document, shared with its
    /// model observer.
    signals: Arc<ModelSignals>,
    /// This session's reading of `signals`.
    watch: DocumentWatch,
    /// Whether the last full apply finished: graph built, regions assigned,
    /// editor told, rendering back on. Only then may a sync that creates and
    /// destroys no region skip all of that.
    settled: bool,
    /// Latency the engine last installed this session's renderer with, so a
    /// sync that changes nothing else still picks up a new plug-in latency.
    installed_latency: Option<u32>,
}

/// Every live ARA session, plus the queues its plug-ins post into.
#[derive(Default)]
pub struct AraState {
    sessions: HashMap<AraSessionKey, AraTrackSession>,
    transport_inbox: Arc<Inbox<AraTransportRequest>>,
    /// Archives loaded from the project, waiting for their session to open.
    ///
    /// A saved project restores clip bindings long before the plug-ins are
    /// instantiated, so the bytes are parked here and handed over the moment the
    /// matching session opens.
    pending_archives: HashMap<AraSessionKey, (String, Vec<u8>)>,
    /// Last error surfaced to the user, if any.
    pub last_error: Option<String>,
}

impl std::fmt::Debug for AraState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AraState")
            .field("sessions", &self.sessions.len())
            .field("pending_archives", &self.pending_archives.len())
            .field("last_error", &self.last_error)
            .finish()
    }
}

impl AraState {
    /// Whether any clip is currently bound.
    pub fn is_active(&self) -> bool {
        !self.sessions.is_empty()
    }

    /// Whether this build can host ARA plug-ins at all.
    pub fn is_supported() -> bool {
        AraSession::is_supported()
    }

    /// Display name of the plug-in bound on this key, if any.
    pub fn plugin_name(&self, key: &AraSessionKey) -> Option<&str> {
        self.sessions
            .get(key)
            .map(|session| session.plugin_name.as_str())
    }

    /// Live sessions, for building the engine's renderer lists and for saving.
    pub fn keys(&self) -> impl Iterator<Item = &AraSessionKey> {
        self.sessions.keys()
    }

    /// The live plug-in instance for one session.
    ///
    /// The editor must attach to this exact instance — the one the engine
    /// renders and the one bound to the ARA document — never to a fresh one.
    pub fn processor(&self, key: &AraSessionKey) -> Option<DirectAudio::Vst3RuntimeProcessor> {
        self.sessions
            .get(key)
            .map(|session| session.processor.clone())
    }

    /// Parks archives restored from a project until their sessions open.
    pub fn load_archives(
        &mut self,
        archives: impl IntoIterator<Item = (AraSessionKey, String, Vec<u8>)>,
    ) {
        self.pending_archives.clear();
        for (key, archive_id, data) in archives {
            self.pending_archives.insert(key, (archive_id, data));
        }
    }

    /// Serialises every live session's document for saving.
    ///
    /// Sessions that have never opened keep the archive they were loaded with,
    /// so saving a project whose ARA plug-ins were never instantiated does not
    /// throw their edits away.
    pub fn store_archives(&mut self) -> Vec<(AraSessionKey, String, Vec<u8>)> {
        let mut stored = Vec::new();
        for (key, session) in self.sessions.iter_mut() {
            if let Some((archive_id, data)) = session.unrestored_archive.as_ref() {
                stored.push((key.clone(), archive_id.clone(), data.clone()));
                continue;
            }
            match session.session.store_archive() {
                Ok(data) => stored.push((key.clone(), session.archive_id.clone(), data)),
                Err(error) => {
                    self.last_error = Some(format!(
                        "{} could not save its ARA state: {error}",
                        session.plugin_name
                    ));
                }
            }
        }
        for (key, (archive_id, data)) in &self.pending_archives {
            if !self.sessions.contains_key(key) {
                stored.push((key.clone(), archive_id.clone(), data.clone()));
            }
        }
        stored
    }

    /// Drains transport requests posted by plug-in editors.
    pub fn take_transport_requests(&self) -> Vec<AraTransportRequest> {
        self.transport_inbox.drain()
    }

    /// Lets every live plug-in deliver its pending model notifications.
    ///
    /// ARA plug-ins may only report analysis progress, content changes and
    /// document-data changes from inside this call, and the host must make it
    /// periodically whenever it is not editing — which, outside the
    /// synchronous edits in this module, it never is. The updates land in each
    /// session's [`ModelSignals`]; read them with [`Self::poll_documents`]
    /// right afterwards. Main thread only.
    pub fn notify_model_updates(&mut self) {
        for (key, session) in self.sessions.iter_mut() {
            if session.session.is_poisoned() {
                continue;
            }
            if let Err(error) = session.session.notify_model_updates() {
                trace(&format!(
                    "model updates for '{}' on {}: {error}",
                    key.plugin_id, key.track_id
                ));
            }
        }
    }

    /// Reads what every plug-in reported since the last poll, and answers
    /// whether any of their documents changed, so the project has unsaved
    /// work even though nothing in the timeline moved.
    ///
    /// Per session, a change the user made in the plug-in's editor also ends
    /// the reign of an archive that plug-in refused to restore: its live
    /// document is the newer state from then on. [`DocumentWatch`] decides
    /// what counts as the user's edit. Call right after
    /// [`Self::notify_model_updates`], so what that call delivered is judged as
    /// the periodic poll's and not as a later host edit's.
    pub fn poll_documents(&mut self, now: Instant) -> bool {
        let mut changed = false;
        for session in self.sessions.values_mut() {
            let poll = session.watch.poll(
                session.signals.report(),
                session.processor.view_is_attached(),
                now,
            );
            changed |= poll.changed;
            if poll.user_edit && session.unrestored_archive.take().is_some() {
                eprintln!(
                    "[ARA] {} was edited in its editor after refusing its saved state; saving \
                     stores its live document from now on",
                    session.plugin_name
                );
            }
        }
        changed
    }

    /// Publishes a new musical timeline (tempo, meter, key) to one session
    /// without touching its graph.
    ///
    /// No renderer suspension and no region churn: ARA lets the host notify a
    /// content change while regions render, as nothing is destroyed. A
    /// session that is not open has nothing to update.
    pub fn update_musical_timeline(
        &mut self,
        key: &AraSessionKey,
        timeline: &AraMusicalTimeline,
    ) -> AraResult<()> {
        let Some(session) = self.sessions.get_mut(key) else {
            return Ok(());
        };
        let outcome = session.session.set_musical_timeline(timeline);
        session
            .watch
            .host_edited(session.signals.report(), Instant::now());
        outcome
    }

    /// Opens a session for `key`, or returns the existing one.
    ///
    /// `engine` instantiates the plug-in in this process; ARA cannot use the
    /// out-of-process host because its callbacks read project state directly.
    fn ensure_session(
        &mut self,
        engine: &DirectAudio::AudioEngine,
        key: &AraSessionKey,
        plugin_name: &str,
        plugin_path: &str,
        class_id: &str,
    ) -> AraResult<&mut AraTrackSession> {
        if self.sessions.contains_key(key) {
            return Ok(self
                .sessions
                .get_mut(key)
                .expect("presence checked immediately above"));
        }

        let processor = engine
            .create_ara_processor(plugin_path, class_id)
            .ok_or_else(|| {
                AraHostError::unsupported(format!("{plugin_name} could not be loaded for ARA"))
            })?;
        let factory = processor.ara_main_factory().ok_or_else(|| {
            AraHostError::unsupported(format!("{plugin_name} exposes no ARA main factory"))
        })?;
        // SAFETY: the guard owns a live reference to the plug-in's ARA
        // main-factory class, and is kept in the session beside the processor
        // whose module provides it.
        let factory_ptr = unsafe { sphere_ara_host::vst3_ara_factory(factory.as_ptr()) }?;

        let audio = Arc::new(AudioLibrary::default());
        let signals = Arc::new(ModelSignals::default());
        let config = AraSessionConfig {
            document_name: Some(format!("Futureboard — {}", key.track_id)),
            audio: Arc::clone(&audio) as Arc<dyn AraAudioAccess>,
            transport: Arc::new(TransportBridge {
                inbox: Arc::clone(&self.transport_inbox),
            }),
            observer: Arc::new(ModelBridge {
                signals: Arc::clone(&signals),
            }),
        };
        // SAFETY: `factory_ptr` came from the guard above, which stays alive in
        // the session for as long as the returned document does.
        let mut session = unsafe { AraSession::open(factory_ptr, config) }?;

        // SAFETY: the component belongs to `processor`, which the session owns
        // and outlives every use of the binding.
        let renderer =
            unsafe { session.bind_renderer(processor.ara_component_unknown(), AraRoles::ALL) }?;

        // Only now may the plug-in be prepared for processing: ARA forbids
        // `setActive()` before the binding, so the instance was created inert.
        if !processor.activate() {
            return Err(AraHostError::Plugin(format!(
                "{plugin_name} could not be prepared for processing after ARA binding"
            )));
        }
        let archive_id = session.factory().document_archive_id.clone();

        self.sessions.insert(
            key.clone(),
            AraTrackSession {
                session,
                _factory: factory,
                processor,
                renderer,
                clips: Vec::new(),
                audio,
                archive_id,
                plugin_name: plugin_name.to_owned(),
                unrestored_archive: None,
                signals,
                watch: DocumentWatch::default(),
                settled: false,
                installed_latency: None,
            },
        );
        Ok(self
            .sessions
            .get_mut(key)
            .expect("inserted immediately above"))
    }

    /// Applies a freshly built graph to one session.
    ///
    /// What that takes depends on the [`AraGraphChange`]. A graph that creates
    /// or destroys regions goes through the full apply: renderers out of the
    /// engine and rendering off, because a playback region may not be
    /// destroyed while a renderer still holds it and ARA lets renderer
    /// assignments change only outside the render state; then regions
    /// re-assigned and the editor told. A graph with the same regions (a clip
    /// moved, trimmed or renamed, say) is updated in place while it keeps
    /// rendering, which ARA allows for property updates inside an edit cycle.
    #[allow(clippy::too_many_arguments)]
    pub fn apply(
        &mut self,
        engine: &DirectAudio::AudioEngine,
        key: &AraSessionKey,
        plugin_name: &str,
        plugin_path: &str,
        class_id: &str,
        timeline: &AraMusicalTimeline,
        graph: &AraGraph,
        media_paths: HashMap<AraSourceKey, PathBuf>,
    ) -> AraResult<()> {
        let pending = self.pending_archives.remove(key);
        if let Err(error) = self.ensure_session(engine, key, plugin_name, plugin_path, class_id) {
            // The archive waits for the next attempt — and is saved back
            // untouched meanwhile — instead of going down with this one.
            if let Some(pending) = pending {
                self.pending_archives.insert(key.clone(), pending);
            }
            return Err(error);
        }

        // Most syncs create and destroy no region: an edit elsewhere in the
        // project, a mixer move, or a clip on this track moved, trimmed or
        // renamed. Taking the renderers out, toggling rendering, re-assigning
        // every region and resetting the editor's selection to all of them
        // would be pure churn there: a barrier wait of up to 250 ms on this
        // thread, the ARA track dropping out during playback, and a plug-in
        // editor whose selection jumps on every edit. What is left is the
        // musical timeline (a content notification) and, when a region's
        // properties changed, an in-place property update. ARA allows both
        // while regions render: its edit cycles exist so the plug-in can
        // synchronise its render threads, and only renderer assignments need
        // the plug-in out of its render state. See [`sync_path`] for when.
        if let Some(session) = self.sessions.get_mut(key) {
            let path = sync_path(pending.is_some(), session.settled, || {
                session.session.graph_change(graph)
            });
            if path != SyncPath::Full {
                session.audio.publish(media_paths);
                let mut outcome = session.session.set_musical_timeline(timeline);
                if outcome.is_ok() && path == SyncPath::Properties {
                    outcome = session.session.apply_graph(graph);
                }
                session
                    .watch
                    .host_edited(session.signals.report(), Instant::now());
                if outcome.is_err() {
                    // Half an edit may have landed; the next sync redoes it in
                    // full. Nothing was suspended, so nothing needs
                    // reinstalling.
                    session.settled = false;
                    return outcome;
                }
                let latency = session.processor.get_latency_samples().max(0) as u32;
                if session.installed_latency != Some(latency) {
                    Self::install_renderers(engine, &mut self.sessions, &key.track_id);
                }
                return Ok(());
            }
        }

        // The edit below creates and destroys playback regions on an instance
        // the engine may be rendering, and each one calls into the plug-in. ARA
        // does not allow the region set to change under a live `process()`, so
        // the track's renderers leave the engine first; `finish_apply` puts them
        // back once the model is whole again.
        Self::suspend_renderers(engine, &key.track_id);
        let outcome = self.apply_model(
            engine,
            key,
            plugin_name,
            timeline,
            graph,
            media_paths,
            pending,
        );
        if outcome.is_err() {
            // A failed edit must not leave the track silent until some later,
            // unrelated sync happens to reinstall it.
            Self::install_renderers(engine, &mut self.sessions, &key.track_id);
        }
        if let Some(session) = self.sessions.get_mut(key) {
            session
                .watch
                .host_edited(session.signals.report(), Instant::now());
        }
        outcome
    }

    /// The model edit itself, with the track's renderers already suspended.
    #[allow(clippy::too_many_arguments)]
    fn apply_model(
        &mut self,
        engine: &DirectAudio::AudioEngine,
        key: &AraSessionKey,
        plugin_name: &str,
        timeline: &AraMusicalTimeline,
        graph: &AraGraph,
        media_paths: HashMap<AraSourceKey, PathBuf>,
        pending: Option<(String, Vec<u8>)>,
    ) -> AraResult<()> {
        let Some(session) = self.sessions.get_mut(key) else {
            return Err(AraHostError::invalid("ARA session disappeared mid-apply"));
        };

        // Unsettled until `finish_apply` completes: an apply that fails part
        // way must be redone in full by the next sync, not skipped.
        session.settled = false;
        session.audio.publish(media_paths);
        session.session.set_rendering(session.renderer, false)?;
        session.session.set_musical_timeline(timeline)?;
        session.session.apply_graph(graph)?;

        // Restore before the regions are assigned and before playback, so the
        // plug-in's stored edits are in place the first time it renders.
        if let Some((archive_id, data)) = pending {
            if let Err(error) = session.session.restore_archive(&archive_id, &data) {
                eprintln!("[ARA] {plugin_name} could not restore its saved state: {error}");
                session.unrestored_archive = Some((archive_id, data));
                self.last_error = Some(format!(
                    "{plugin_name} could not restore its saved ARA state: {error}"
                ));
                // Re-borrow: `self.last_error` above ended the previous borrow.
                return self.finish_apply(engine, key, graph);
            }
        }
        self.finish_apply(engine, key, graph)
    }

    /// Takes a track's ARA renderers out of the engine and waits for the audio
    /// callback to confirm it.
    ///
    /// `set_ara_renderers` only *queues* the change, so without this barrier the
    /// callback can still be inside `process()` on the very instance whose ARA
    /// model is about to be edited or destroyed. A `false` ack means the barrier
    /// was not confirmed — no stream is open, or the callback is stalled — and
    /// in neither case is anything processing, so the caller carries on.
    ///
    /// Control thread only: it blocks, briefly, on the audio callback.
    fn suspend_renderers(engine: &DirectAudio::AudioEngine, track_id: &str) {
        if let Err(error) = engine.set_ara_renderers(track_id.to_string(), Vec::new()) {
            eprintln!("[ARA] could not suspend renderers for track {track_id}: {error}");
            return;
        }
        let _ = engine.wait_for_command_barrier(RENDERER_BARRIER_TIMEOUT);
    }

    fn finish_apply(
        &mut self,
        engine: &DirectAudio::AudioEngine,
        key: &AraSessionKey,
        graph: &AraGraph,
    ) -> AraResult<()> {
        let Some(session) = self.sessions.get_mut(key) else {
            return Err(AraHostError::invalid("ARA session disappeared mid-apply"));
        };
        session.clips = graph
            .regions
            .iter()
            .map(|region| region.key.clone())
            .collect();
        session
            .session
            .set_renderer_regions(session.renderer, &session.clips)?;
        // What the plug-in renders and what its editor shows are separate in
        // ARA 2; without this the docked editor opens on an empty canvas.
        let tracks: Vec<sphere_ara_host::AraTrackKey> = graph
            .sequences
            .iter()
            .map(|sequence| sequence.key.clone())
            .collect();
        if let Err(error) =
            session
                .session
                .notify_editor_selection(session.renderer, &session.clips, &tracks)
        {
            // A plug-in without the editor-view role is not an error worth
            // failing the whole apply over; it just has no view to tell.
            eprintln!("[ARA] editor selection not published: {error}");
        }
        session.session.set_rendering(session.renderer, true)?;
        session.settled = true;

        Self::install_renderers(engine, &mut self.sessions, &key.track_id);
        Ok(())
    }

    /// Rebuilds and installs the engine's renderer list for one track.
    ///
    /// The engine replaces a track's whole list at once, so every session on the
    /// track has to be sent together — installing one at a time would drop the
    /// others. Records the latency each renderer went in with.
    fn install_renderers(
        engine: &DirectAudio::AudioEngine,
        sessions: &mut HashMap<AraSessionKey, AraTrackSession>,
        track_id: &str,
    ) {
        let renderers: Vec<DirectAudio::RuntimeAraRenderer> = sessions
            .iter_mut()
            .filter(|(key, _)| key.track_id == track_id)
            .map(|(key, session)| {
                let latency_samples = session.processor.get_latency_samples().max(0) as u32;
                session.installed_latency = Some(latency_samples);
                DirectAudio::RuntimeAraRenderer {
                    instance_id: format!("ara:{}:{}", key.plugin_id, key.track_id),
                    latency_samples,
                    processor: session.processor.clone(),
                }
            })
            .collect();
        if let Err(error) = engine.set_ara_renderers(track_id.to_string(), renderers) {
            eprintln!("[ARA] could not install renderers for track {track_id}: {error}");
        }
    }

    /// Tears down one session and removes its renderer from the track.
    pub fn close(&mut self, engine: &DirectAudio::AudioEngine, key: &AraSessionKey) {
        let Some(session) = self.sessions.remove(key) else {
            return;
        };
        let plugin_name = session.plugin_name.clone();
        // Teardown order, and every step of it matters. Each is timed and
        // reported, because the failure mode when one is wrong is a hung main
        // thread and the only thing that identifies which call never returned is
        // the absence of the line after it. That is worth four lines per removal
        // whether or not a debug flag is set — this runs once when a user
        // removes ARA from a track, not on any hot path.
        //
        // The shape is: out of the audio graph, then the UI down, then the audio
        // instance down, then the document. Nothing that calls into the plug-in
        // may sit between deactivating the instance and destroying the document.
        //
        // 1. Take the instance out of the audio graph. The removal is only
        //    *queued*, so the barrier is part of this step: the region
        //    assignments dropped in step 4 call into a plug-in the callback
        //    would otherwise still be rendering.
        let confirmed = step(&plugin_name, "1/4 removing renderers", || {
            Self::install_renderers(engine, &mut self.sessions, &key.track_id);
            engine.wait_for_command_barrier(RENDERER_BARRIER_TIMEOUT)
        });
        if !confirmed {
            // Not fatal by itself, but it is the precondition every later step
            // assumes, so it must not pass silently: the audio callback may
            // still be inside this instance while step 3 deactivates it.
            eprintln!(
                "[ara-close] WARNING '{plugin_name}': the audio callback did not confirm the \
                 renderer removal within {RENDERER_BARRIER_TIMEOUT:?}"
            );
        }
        let AraTrackSession {
            session, processor, ..
        } = session;
        // 2. Release the plug-in's editor view, through *both* paths a view can
        //    have been attached by. Last line of defence for a view that has not
        //    come down yet — a project closing out from under a docked editor,
        //    say.
        //
        //    `embed_detach` alone was not that defence. It returns immediately
        //    unless the plug-in is in embed mode, and no ARA editor is: the
        //    docked panel and the popped-out window both attach through the
        //    host-owned view path (`view_attach`), where the host owns the
        //    window and the call drives only `IPlugView`. So the view survived,
        //    step 4 destroyed the document under it, and the app went down.
        //
        //    First, not third. `removed()` is real work inside the plug-in's
        //    editor, and run between deactivating the instance and closing the
        //    document it lands in the one gap where the plug-in has no audio
        //    instance to answer with and the host is about to wait on its
        //    readers — the crash became a hang there. Taking the UI down while
        //    the instance is still whole is also just the ordinary case: it is
        //    what closing an editor window during playback does every day.
        step(&plugin_name, "2/4 releasing the editor view", || {
            processor.view_detach();
            processor.embed_detach();
        });
        // 3. Leave the processing state. A plug-in whose renderer is still
        //    active is entitled to hold its render lock, and step 4 then waits
        //    on it from the main thread — which is the hang, not a crash.
        step(&plugin_name, "3/4 stopping processing", || {
            processor.stop_processing()
        });
        // 4. Destroy the ARA binding and the document controller.
        //
        //    This is the step that can block: closing the document revokes the
        //    host's audio readers, and revoking one waits for whichever of the
        //    plug-in's threads is reading through it to let go.
        let closed = step(&plugin_name, "4/4 closing the ARA document", || {
            session.close()
        });
        if let Err(error) = closed {
            self.last_error = Some(format!("{plugin_name} reported errors on close: {error}"));
        }
        // 5. The processor drops with the last handle, which terminates the
        //    companion instance.
        processor.set_destroy_reason("ara-unbound");
        self.pending_archives.remove(key);
    }

    /// Whether any editor still holds a view on this session's plug-in.
    ///
    /// Asks the plug-in, not any one host. A docked panel and a popped-out
    /// window attach through the same view host, so waiting on only one of them
    /// answers "released" while the other is still drawing from the document
    /// that is about to be destroyed.
    pub fn view_is_attached(&self, key: &AraSessionKey) -> bool {
        self.sessions
            .get(key)
            .is_some_and(|session| session.processor.view_is_attached())
    }

    /// Tears down every session, e.g. when a project closes.
    pub fn close_all(&mut self, engine: &DirectAudio::AudioEngine) {
        for key in self.sessions.keys().cloned().collect::<Vec<_>>() {
            self.close(engine, &key);
        }
        self.pending_archives.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inbox_drops_the_oldest_rather_than_growing() {
        let inbox: Inbox<u32> = Inbox::default();
        for value in 0..(INBOX_CAPACITY as u32 + 10) {
            inbox.push(value);
        }
        let drained = inbox.drain();
        assert_eq!(drained.len(), INBOX_CAPACITY);
        // The newest values survive: the latest transport request is the one
        // to follow, and an unbounded queue is not an option on a callback.
        assert_eq!(*drained.last().unwrap(), INBOX_CAPACITY as u32 + 9);
        assert_eq!(drained[0], 10);
        assert!(inbox.drain().is_empty(), "drain must consume");
    }

    #[test]
    fn parked_archives_survive_a_save_when_their_plugin_never_opened() {
        // A project can be saved without its ARA plug-ins ever being
        // instantiated (missing plug-in, engine not started). Those archives
        // must be written back untouched instead of being dropped.
        let mut state = AraState::default();
        let key = AraSessionKey {
            plugin_id: "vst3:melodyne".to_string(),
            track_id: "track-1".to_string(),
        };
        state.load_archives([(
            key.clone(),
            "com.celemony.ara.v5".to_string(),
            vec![1, 2, 3],
        )]);

        let stored = state.store_archives();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].0, key);
        assert_eq!(stored[0].1, "com.celemony.ara.v5");
        assert_eq!(stored[0].2, vec![1, 2, 3]);
    }

    #[test]
    fn loading_archives_replaces_the_previous_set() {
        let mut state = AraState::default();
        let first = AraSessionKey {
            plugin_id: "a".to_string(),
            track_id: "t".to_string(),
        };
        let second = AraSessionKey {
            plugin_id: "b".to_string(),
            track_id: "t".to_string(),
        };
        state.load_archives([(first, "id-a".to_string(), vec![0])]);
        state.load_archives([(second.clone(), "id-b".to_string(), vec![1])]);

        let stored = state.store_archives();
        assert_eq!(
            stored.len(),
            1,
            "opening a new project must not keep the old archives"
        );
        assert_eq!(stored[0].0, second);
    }

    fn progress(state: i32) -> AraModelUpdate {
        AraModelUpdate::AnalysisProgress {
            source: Some(AraSourceKey::from("asset-1")),
            state,
            value: 0.5,
        }
    }

    const ANALYSIS_UPDATED: i32 = 1;

    /// A plug-in session as `AraState` drives it: the bridge the plug-in
    /// reports into, and the watch the UI poll reads it with.
    struct Document {
        bridge: ModelBridge,
        watch: DocumentWatch,
        start: Instant,
    }

    impl Document {
        fn new() -> Self {
            Self {
                bridge: ModelBridge {
                    signals: Arc::new(ModelSignals::default()),
                },
                watch: DocumentWatch::default(),
                start: Instant::now(),
            }
        }

        fn at(&self, millis: u64) -> Instant {
            self.start + Duration::from_millis(millis)
        }

        /// One host edit at `millis`, during which the plug-in reports
        /// `answers` (the session's own post-edit notification).
        fn host_edit(&mut self, millis: u64, answers: &[AraModelUpdate]) {
            for update in answers {
                self.bridge.notify(update.clone());
            }
            let report = self.bridge.signals.report();
            self.watch.host_edited(report, self.at(millis));
        }

        /// One periodic poll at `millis` that delivers `reports`.
        fn poll(&mut self, millis: u64, view: bool, reports: &[AraModelUpdate]) -> DocumentPoll {
            for update in reports {
                self.bridge.notify(update.clone());
            }
            let report = self.bridge.signals.report();
            self.watch.poll(report, view, self.at(millis))
        }
    }

    const CHANGE: AraModelUpdate = AraModelUpdate::DocumentDataChanged;

    /// The positive case, and the baseline the tests below break one
    /// condition of: editor open and settled, document quiet.
    #[test]
    fn a_change_in_a_quiet_document_with_its_editor_open_is_the_users() {
        let mut doc = Document::new();
        doc.host_edit(0, &[]);
        assert_eq!(doc.poll(100, true, &[]), DocumentPoll::default());
        let poll = doc.poll(2_500, true, &[CHANGE]);
        assert_eq!(
            poll,
            DocumentPoll {
                changed: true,
                user_edit: true,
            }
        );
    }

    #[test]
    fn with_no_editor_open_no_change_is_the_users() {
        let mut doc = Document::new();
        doc.host_edit(0, &[]);
        let poll = doc.poll(10_000, false, &[CHANGE]);
        assert!(poll.changed, "every change still needs saving");
        assert!(!poll.user_edit);
    }

    #[test]
    fn a_change_just_after_the_editor_opens_is_not_the_users() {
        let mut doc = Document::new();
        doc.host_edit(0, &[]);
        // Quiet for long before, but the view only just attached: it is
        // building itself.
        assert!(!doc.poll(10_000, true, &[]).user_edit);
        let poll = doc.poll(11_000, true, &[CHANGE]);
        assert!(poll.changed);
        assert!(!poll.user_edit);
        // Closing and reopening the editor starts the wait again.
        doc.poll(20_000, false, &[]);
        doc.poll(20_100, true, &[]);
        assert!(!doc.poll(21_000, true, &[CHANGE]).user_edit);
        assert!(doc.poll(22_200, true, &[CHANGE]).user_edit);
    }

    #[test]
    fn a_change_the_plugin_reports_in_answer_to_a_host_edit_is_not_the_users() {
        let mut doc = Document::new();
        doc.poll(0, true, &[]);
        // A restore, or a region move Melodyne re-derives scales for: the
        // answer comes back inside the host's own edit.
        doc.host_edit(10_000, &[CHANGE]);
        let poll = doc.poll(15_000, true, &[]);
        assert!(poll.changed, "it still needs saving");
        assert!(!poll.user_edit);
    }

    #[test]
    fn a_change_soon_after_a_host_edit_is_not_the_users() {
        let mut doc = Document::new();
        doc.poll(0, true, &[]);
        doc.host_edit(10_000, &[]);
        assert!(!doc.poll(11_000, true, &[CHANGE]).user_edit);
        assert!(doc.poll(12_100, true, &[CHANGE]).user_edit);
    }

    /// The reviewer's scenario: the saved archive was refused, the user opens
    /// the editor to look, and the fresh document is still analysing.
    #[test]
    fn a_change_while_the_document_is_analysing_is_not_the_users() {
        let mut doc = Document::new();
        doc.host_edit(0, &[]);
        doc.poll(100, true, &[progress(ANALYSIS_STARTED)]);
        // Long after the view settled, with no report for a while: an analysis
        // that is still running explains the change on its own.
        let poll = doc.poll(30_000, true, &[CHANGE]);
        assert!(poll.changed);
        assert!(!poll.user_edit);
        // It finishing is a report too, and the result lands as source
        // content; a change right after that is still the analysis.
        assert!(
            !doc.poll(
                31_000,
                true,
                &[
                    progress(ANALYSIS_UPDATED),
                    progress(ANALYSIS_COMPLETED),
                    AraModelUpdate::SourceContentChanged {
                        source: Some(AraSourceKey::from("asset-1")),
                    },
                    CHANGE,
                ],
            )
            .user_edit
        );
        assert!(!doc.poll(32_000, true, &[CHANGE]).user_edit);
        // Once it has been quiet for the settle time, the user is editing.
        assert!(doc.poll(33_100, true, &[CHANGE]).user_edit);
    }

    #[test]
    fn a_change_explained_away_is_not_judged_again_later() {
        let mut doc = Document::new();
        doc.host_edit(0, &[]);
        doc.poll(100, true, &[]);
        let poll = doc.poll(1_000, true, &[CHANGE]);
        assert!(poll.changed && !poll.user_edit);
        // Nothing new: the old change does not turn into the user's once the
        // editor has settled.
        assert_eq!(doc.poll(5_000, true, &[]), DocumentPoll::default());
    }

    #[test]
    fn a_completion_without_a_start_does_not_wrap_the_running_count() {
        let signals = ModelSignals::default();
        signals.record(&progress(ANALYSIS_COMPLETED));
        assert!(!signals.report().analysing);
        signals.record(&progress(ANALYSIS_STARTED));
        signals.record(&progress(ANALYSIS_STARTED));
        signals.record(&progress(ANALYSIS_UPDATED));
        signals.record(&progress(ANALYSIS_COMPLETED));
        assert!(signals.report().analysing, "one of two is still running");
        signals.record(&progress(ANALYSIS_COMPLETED));
        assert!(!signals.report().analysing);
    }

    /// Finding: progress bursts used to share a 512-entry drop-oldest queue
    /// with the one update that matters.
    #[test]
    fn no_burst_of_analysis_reports_can_lose_a_document_change() {
        let signals = ModelSignals::default();
        signals.record(&CHANGE);
        for _ in 0..(INBOX_CAPACITY * 4) {
            signals.record(&progress(ANALYSIS_UPDATED));
            signals.record(&AraModelUpdate::ModificationContentChanged { clip: None });
            signals.record(&AraModelUpdate::RegionContentChanged { clip: None });
        }
        let report = signals.report();
        assert_eq!(report.document_changes, 1);
        assert_eq!(report.analysis_reports, INBOX_CAPACITY as u64 * 4);
    }

    #[test]
    fn each_session_counts_only_its_own_plugins_reports() {
        let first = ModelBridge {
            signals: Arc::new(ModelSignals::default()),
        };
        let second = ModelBridge {
            signals: Arc::new(ModelSignals::default()),
        };
        second.notify(CHANGE);
        second.notify(CHANGE);
        first.notify(progress(ANALYSIS_STARTED));
        assert_eq!(first.signals.report().document_changes, 0);
        assert!(first.signals.report().analysing);
        assert_eq!(second.signals.report().document_changes, 2);
        assert!(!second.signals.report().analysing);
    }

    /// Analysis is requested right after each host edit that adds a source,
    /// so its start can come back inside the next host edit's own report
    /// rather than from the periodic poll. It still counts as running.
    #[test]
    fn an_analysis_that_starts_inside_a_host_edit_still_explains_a_change() {
        let mut doc = Document::new();
        doc.poll(0, true, &[]);
        doc.host_edit(1_000, &[progress(ANALYSIS_STARTED)]);
        let poll = doc.poll(30_000, true, &[CHANGE]);
        assert!(poll.changed);
        assert!(!poll.user_edit);
        doc.poll(31_000, true, &[progress(ANALYSIS_COMPLETED)]);
        assert!(doc.poll(33_100, true, &[CHANGE]).user_edit);
    }

    /// Review amendment 67: only a graph that creates or destroys regions
    /// takes the renderers out, toggles rendering, re-assigns regions and
    /// resets the editor's selection.
    #[test]
    fn only_a_sync_that_creates_or_destroys_regions_takes_the_full_path() {
        assert_eq!(
            sync_path(false, true, || AraGraphChange::Unchanged),
            SyncPath::Timeline
        );
        // A clip on the track moved, trimmed, renamed or recoloured.
        assert_eq!(
            sync_path(false, true, || AraGraphChange::Properties),
            SyncPath::Properties
        );
        assert_eq!(
            sync_path(false, true, || AraGraphChange::Structure),
            SyncPath::Full
        );
        // An archive to restore, or a full apply that stopped part way: the
        // graph is not even compared.
        let not_asked = || -> AraGraphChange { panic!("the graph must not be compared") };
        assert_eq!(sync_path(true, true, not_asked), SyncPath::Full);
        assert_eq!(sync_path(false, false, not_asked), SyncPath::Full);
        assert_eq!(sync_path(true, false, not_asked), SyncPath::Full);
    }

    #[test]
    fn a_reader_past_the_end_of_the_source_returns_silence() {
        // ARA is allowed to read beyond a source; the contract is silence, not
        // an error, and a plug-in that gets an error there stops analysing.
        let buffer = Arc::new(DirectAudio::AudioFileBuffer {
            sample_rate: 48_000,
            channels: 2,
            frames: 2,
            samples: vec![0.5, -0.5, 0.25, -0.25],
        });
        let mut reader = BufferReader { buffer };
        assert_eq!(reader.channel_count(), 2);
        assert_eq!(reader.frame_count(), 2);

        let mut left = [9.0f32; 4];
        let mut right = [9.0f32; 4];
        let mut planes: Vec<&mut [f32]> = vec![&mut left, &mut right];
        reader.read_planar_f32(-1, &mut planes).unwrap();

        // frame -1 (before), frames 0..1 (real), frame 2 (past the end)
        assert_eq!(left, [0.0, 0.5, 0.25, 0.0]);
        assert_eq!(right, [0.0, -0.5, -0.25, 0.0]);
    }
}
