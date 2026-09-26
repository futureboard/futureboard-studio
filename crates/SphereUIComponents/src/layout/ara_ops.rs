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

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sphere_ara_host::{
    ara_persistent_id, AraAudioAccess, AraClipKey, AraGraph, AraGraphChange, AraHostError,
    AraModelObserver, AraModelUpdate, AraMusicalTimeline, AraRendererId, AraRestoreMap,
    AraRestoreOutcome, AraRestoreReport, AraRestoreRequest, AraRestoreScope, AraResult, AraRoles,
    AraSampleReader, AraSession, AraSessionConfig, AraSourceKey, AraStoredArchive, AraTrackKey,
    AraTransportControl, AraTransportRequest,
};

use super::ara_restore_plan::{self, AraRestorePlan, RestoreTiming};
use crate::project::ProjectAraIdentity;

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

/// How long a plug-in's document has to have been left alone (no host edit
/// and no analysis report on it), or its editor view open, before a change to
/// it can be the user's rather than the plug-in settling.
///
/// See [`DocumentWatch`]. Long enough to cover the burst a plug-in sends when
/// its view attaches or an analysis finishes, short enough that an edit made
/// right after opening the editor counts almost at once.
const USER_EDIT_SETTLE: Duration = Duration::from_secs(2);

/// ARA's `kARAAnalysisProgressStarted`, required first for every analysis.
const ANALYSIS_STARTED: i32 = 0;

/// ARA's `kARAAnalysisProgressCompleted`, required last for every analysis,
/// whether it completed or was cancelled.
const ANALYSIS_COMPLETED: i32 = 2;

/// How long a document has to have sent no analysis report before the host
/// asks the plug-in whether the analyses it reported started are still
/// running. A plug-in that restores a source's analysis may never complete
/// an analysis it reported started before; asking settles that.
const ANALYSIS_CHECK_QUIET: Duration = Duration::from_secs(3);

/// Least time between two such questions to one plug-in.
const ANALYSIS_CHECK_INTERVAL: Duration = Duration::from_secs(2);

/// How long the status bar says that a saved ARA document was kept unchanged.
pub(crate) const ARA_NOTICE: Duration = Duration::from_secs(15);

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
/// lose it. Modification content changes are counted for the undo history
/// (see [`AraHistory`]); region content changes are not kept at all, because
/// nothing reads them yet. ARA has a plug-in report only from inside
/// `notifyModelUpdates` on the model thread, but one that reports from its own
/// threads is served too. None of this is on the audio thread.
#[derive(Debug, Default)]
struct ModelSignals {
    /// `DocumentDataChanged` reports so far.
    document_changes: AtomicU64,
    /// Analysis progress and source content reports so far: the plug-in
    /// analysing, and the result of an analysis landing. ARA counts an
    /// analysis finishing as a content change of its source.
    analysis_reports: AtomicU64,
    /// Analyses of a source that ended so far: a completion report, or the
    /// content of a source whose analysis was running landing. Melodyne
    /// reports the data change a finished analysis makes in the same batch
    /// (see [`DocumentWatch`]).
    analyses_ended: AtomicU64,
    /// `ModificationContentChanged` reports so far: what the plug-in reports
    /// when a clip's edit (its notes, their pitch and timing) changes. The
    /// user's edits in the editor arrive as these even from a plug-in that
    /// never reports `DocumentDataChanged`, which only ARA 2.3 has.
    content_edits: AtomicU64,
    /// Analyses reported started and not yet over, per source.
    ///
    /// Per source, so that one start nothing ever completes cannot pin the
    /// whole document as analysing for the session: a restore can leave such
    /// a start behind (Melodyne did, when the host asked for analysis before
    /// restoring), and while it stood the document never settled.
    /// A source's entry goes when its analysis completes, when its content
    /// lands, when a restore supplies it, or when the plug-in says its
    /// analysis is not incomplete (see [`finished_analyses`]).
    running: Mutex<RunningAnalyses>,
}

/// Analyses a plug-in reported started and not yet over.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct RunningAnalyses {
    /// Starts not yet completed, per source.
    sources: HashMap<AraSourceKey, u32>,
    /// Starts for a source the host could not name.
    unknown: u32,
}

impl RunningAnalyses {
    fn any(&self) -> bool {
        self.unknown > 0 || !self.sources.is_empty()
    }
}

/// One reading of a session's [`ModelSignals`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ModelReport {
    document_changes: u64,
    analysis_reports: u64,
    analyses_ended: u64,
    content_edits: u64,
    /// Whether any analysis of the session's sources is still running.
    analysing: bool,
}

impl ModelSignals {
    fn running(&self) -> std::sync::MutexGuard<'_, RunningAnalyses> {
        // A poisoned lock still holds a usable set; losing the reports would
        // be worse than reading them.
        self.running
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn record(&self, update: &AraModelUpdate) {
        match update {
            AraModelUpdate::DocumentDataChanged => {
                self.document_changes.fetch_add(1, Ordering::Relaxed);
            }
            AraModelUpdate::AnalysisProgress { source, state, .. } => {
                self.analysis_reports.fetch_add(1, Ordering::Relaxed);
                let mut running = self.running();
                match (*state, source) {
                    (ANALYSIS_STARTED, Some(source)) => {
                        *running.sources.entry(source.clone()).or_default() += 1;
                    }
                    (ANALYSIS_STARTED, None) => running.unknown += 1,
                    // Saturating: a completion whose start was never reported
                    // must not wrap the count. It is an analysis ending all
                    // the same.
                    (ANALYSIS_COMPLETED, Some(source)) => {
                        if let Some(count) = running.sources.get_mut(source) {
                            *count -= 1;
                            if *count == 0 {
                                running.sources.remove(source);
                            }
                        }
                        self.analyses_ended.fetch_add(1, Ordering::Relaxed);
                    }
                    (ANALYSIS_COMPLETED, None) => {
                        running.unknown = running.unknown.saturating_sub(1);
                        self.analyses_ended.fetch_add(1, Ordering::Relaxed);
                    }
                    _ => {}
                }
            }
            AraModelUpdate::SourceContentChanged { source } => {
                self.analysis_reports.fetch_add(1, Ordering::Relaxed);
                // The analysis result landed: whatever was running on this
                // source is over, even if its completion never comes.
                if let Some(source) = source {
                    if self.running().sources.remove(source).is_some() {
                        self.analyses_ended.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            AraModelUpdate::ModificationContentChanged { .. } => {
                self.content_edits.fetch_add(1, Ordering::Relaxed);
            }
            AraModelUpdate::RegionContentChanged { .. } => {}
        }
    }

    /// Forgets every start reported for `source`: its analysis is over.
    fn settle(&self, source: &AraSourceKey) {
        self.running().sources.remove(source);
    }

    /// Forgets every start: no analysis of the document is running.
    fn settle_all(&self) {
        *self.running() = RunningAnalyses::default();
    }

    fn report(&self) -> ModelReport {
        ModelReport {
            document_changes: self.document_changes.load(Ordering::Relaxed),
            analysis_reports: self.analysis_reports.load(Ordering::Relaxed),
            analyses_ended: self.analyses_ended.load(Ordering::Relaxed),
            content_edits: self.content_edits.load(Ordering::Relaxed),
            analysing: self.running().any(),
        }
    }
}

/// Whether to ask a plug-in which of the analyses it reported started are
/// really still running: only while some are, once no analysis report has
/// come for [`ANALYSIS_CHECK_QUIET`], and at most once per
/// [`ANALYSIS_CHECK_INTERVAL`].
fn analysis_check_due(
    analysing: bool,
    last_report: Option<Instant>,
    last_check: Option<Instant>,
    now: Instant,
) -> bool {
    let since = |at: Instant, wait: Duration| now.saturating_duration_since(at) >= wait;
    analysing
        && last_report.is_none_or(|at| since(at, ANALYSIS_CHECK_QUIET))
        && last_check.is_none_or(|at| since(at, ANALYSIS_CHECK_INTERVAL))
}

/// What one check found over.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct FinishedAnalyses {
    /// Sources whose analysis is over.
    sources: Vec<AraSourceKey>,
    /// Every analysis is over, including starts the host could not name.
    all: bool,
}

/// Decides which running analyses are over, from the plug-in's own answer.
///
/// `incomplete` asks whether a source's analysis is incomplete right now:
/// `Some(false)` settles it, while `Some(true)`, `None` (the plug-in does not
/// say) or a failed query keep it running. A source that has left the graph
/// has nothing left to analyse. Starts the host could not name settle only
/// when every source of the graph answers `Some(false)`.
fn finished_analyses(
    running: &RunningAnalyses,
    graph_sources: &[AraSourceKey],
    mut incomplete: impl FnMut(&AraSourceKey) -> Option<bool>,
) -> FinishedAnalyses {
    let mut finished = FinishedAnalyses::default();
    let mut keys: Vec<&AraSourceKey> = running.sources.keys().collect();
    keys.sort();
    for key in keys {
        if !graph_sources.contains(key) || incomplete(key) == Some(false) {
            finished.sources.push(key.clone());
        }
    }
    if running.unknown > 0 {
        finished.all = graph_sources
            .iter()
            .all(|key| incomplete(key) == Some(false));
    }
    finished
}

/// The undo history's trace, on with the keyboard trace
/// (`FUTUREBOARD_KEY_DEBUG`): an Undo that finds nothing cannot otherwise say
/// whether no edit was seen, no document was stored, or storing failed.
fn history_trace(line: impl FnOnce() -> String) {
    if crate::components::transport_key::key_debug() {
        eprintln!("[Keyboard] ara undo: {}", line());
    }
}

/// Feeds one session's poll into its undo history, and answers whether a new
/// undo step was recorded.
///
/// The first document is stored once the session has settled, so the first
/// edit has something to go back to. A user edit waits for the document to go
/// quiet, then is stored as a step — unless the host or an analysis was busy
/// with the document in the meantime, in which case the stored document only
/// becomes the new baseline. Any other change the host or an analysis made is
/// stored as the baseline once it goes quiet, so that the check an Undo makes
/// (see [`AraState::step_history`]) never takes it for the user's.
fn track_history(session: &mut AraTrackSession, user_change: bool, now: Instant) -> bool {
    if session.plugin.is_poisoned() {
        return false;
    }
    let analysing = session.signals.report().analysing;
    let history = &mut session.history;
    if user_change {
        history.pending_since = Some(now);
    }
    if session.watch.last_activity != history.activity_seen {
        history.activity_seen = session.watch.last_activity;
        if history.baseline.is_some() {
            history.rebase_since = Some(now);
        }
    }
    let name = &session.plugin_name;
    if history.baseline.is_none() {
        let retry_due = history
            .baseline_failed_at
            .is_none_or(|at| now.saturating_duration_since(at) >= Duration::from_secs(2));
        if !(session.watch.settled && !analysing && retry_due) {
            return false;
        }
        match session.plugin.store_archive() {
            Ok(first) => {
                // The same untouched document stored again: equal bytes are
                // what lets an Undo tell an edit nothing reported from no
                // edit at all.
                history.stable = matches!(
                    session.plugin.store_archive(),
                    Ok(again) if again.bytes == first.bytes
                );
                history_trace(|| {
                    format!(
                        "'{name}': first document stored ({} bytes, {})",
                        first.bytes.len(),
                        if history.stable {
                            "stable"
                        } else {
                            "differs on every store"
                        }
                    )
                });
                history.record(first.bytes, false);
                history.pending_since = None;
                history.rebase_since = None;
            }
            Err(error) => {
                history.baseline_failed_at = Some(now);
                history_trace(|| format!("'{name}': could not store the first document: {error}"));
            }
        }
        return false;
    }
    if analysing {
        return false;
    }
    let quiet = |at: Instant, wait: Duration| now.saturating_duration_since(at) >= wait;
    if let Some(since) = history.pending_since {
        if !quiet(since, ARA_UNDO_SETTLE) {
            return false;
        }
        history.pending_since = None;
        history.rebase_since = None;
        let host_busy = session
            .watch
            .last_activity
            .is_some_and(|at| !quiet(at, ARA_UNDO_SETTLE * 2));
        return match session.plugin.store_archive() {
            Ok(stored) => {
                let step = history.record(stored.bytes, !host_busy);
                history_trace(|| {
                    format!(
                        "'{name}': edit settled: {} (steps {:?})",
                        if step {
                            "undo step recorded"
                        } else if host_busy {
                            "host or analysis busy, taken as the baseline"
                        } else {
                            "document unchanged"
                        },
                        history.depth()
                    )
                });
                step
            }
            Err(error) => {
                history_trace(|| format!("'{name}': could not store an undo step: {error}"));
                false
            }
        };
    }
    if let Some(since) = history.rebase_since {
        if quiet(since, ARA_UNDO_SETTLE * 2) {
            history.rebase_since = None;
            match session.plugin.store_archive() {
                Ok(stored) => {
                    history.record(stored.bytes, false);
                }
                Err(error) => {
                    history_trace(|| format!("'{name}': could not store the document: {error}"))
                }
            }
        }
    }
    false
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
/// new, so the project has unsaved work.
///
/// Every change marks the project dirty, as it always has, except while the
/// document settles after opening, so that opening a project does not by
/// itself leave unsaved changes behind. Until the document has first gone idle
/// (no analysis running, no host edit or analysis report for
/// [`USER_EDIT_SETTLE`]), its changes are the plug-in settling (answering the
/// restore, analysing what it could not restore) and are passed over, unless
/// its editor view is open and settled, where the user could be editing.
///
/// A change that arrives while the view is open but not settled yet could be
/// the user's too. When nothing explains it (no host edit and no analysis
/// report within [`USER_EDIT_SETTLE`], this poll's included), it is held back
/// rather than passed over, and counts once the view or the document has
/// settled. One that comes with analysis news or right after it, or right
/// after a host edit, is the plug-in's own and is passed over, held change or
/// not.
///
/// While the document settles, a change that comes with a source's analysis
/// ending (in the same poll, or within [`USER_EDIT_SETTLE`] of it) is the
/// plug-in's own even when the view is open and settled: Melodyne reports the
/// data change of a finished analysis together with the analysis completing
/// and the source's content changing (seen with Melodyne 5.4.2), and an
/// analysis can run far longer than the view takes to settle. What explains
/// it is that source's analysis ending, not a time window since opening.
///
/// Nothing here decides what a save writes: every restore that is not shown
/// to have landed is kept aside at once and the live document saves (see
/// [`ArchiveFate`]).
#[derive(Debug, Default)]
struct DocumentWatch {
    /// Document changes already counted towards the project's dirty state,
    /// or passed over while the document settled.
    counted: u64,
    /// A change nothing explained arrived while the document settled with its
    /// editor view open but not settled: it counts once it can.
    holding: bool,
    /// Analysis reports already seen.
    analysis_reports: u64,
    /// The last host edit or new analysis report: activity the user did not
    /// cause.
    last_activity: Option<Instant>,
    /// The last new analysis report.
    last_analysis_report: Option<Instant>,
    /// Analysis endings already seen ([`ModelSignals::analyses_ended`]).
    analyses_ended: u64,
    /// The last time a source's analysis was seen ending.
    last_analysis_end: Option<Instant>,
    /// The last time the plug-in was asked whether its analyses still run.
    last_analysis_check: Option<Instant>,
    /// When the plug-in's editor view was first seen attached, while it stays
    /// attached.
    view_since: Option<Instant>,
    /// Whether the document has gone idle at least once since it opened.
    settled: bool,
    /// Modification content changes already seen
    /// ([`ModelSignals::content_edits`]).
    content_edits: u64,
}

impl DocumentWatch {
    /// Everything reported so far is the host's doing (an undo it just
    /// restored): counted as seen, and marked as activity the user did not
    /// cause.
    fn absorb(&mut self, report: ModelReport, now: Instant) {
        self.counted = report.document_changes;
        self.analysis_reports = report.analysis_reports;
        self.content_edits = report.content_edits;
        self.last_activity = Some(now);
    }

    /// Whether a clip's edit changed since the last call. Only for the undo
    /// history: what makes the project dirty is still [`Self::poll`]'s.
    fn take_content_edits(&mut self, report: ModelReport) -> bool {
        let edited = report.content_edits != self.content_edits;
        self.content_edits = report.content_edits;
        edited
    }

    /// The host has just edited the document: activity the user did not
    /// cause, and the analysis reports that came back inside it are seen.
    fn host_edited(&mut self, report: ModelReport, now: Instant) {
        self.analysis_reports = report.analysis_reports;
        self.last_activity = Some(now);
    }

    /// Reads what the periodic poll collected, with the editor view's current
    /// state, and answers whether the document changed since the last poll
    /// in a way that is unsaved work.
    fn poll(&mut self, report: ModelReport, view_attached: bool, now: Instant) -> bool {
        self.view_since = if view_attached {
            Some(self.view_since.unwrap_or(now))
        } else {
            None
        };
        if report.analysis_reports != self.analysis_reports {
            self.analysis_reports = report.analysis_reports;
            self.last_activity = Some(now);
            self.last_analysis_report = Some(now);
        }
        if report.analyses_ended != self.analyses_ended {
            self.analyses_ended = report.analyses_ended;
            self.last_analysis_end = Some(now);
        }

        let settled = |since: Instant| now.saturating_duration_since(since) >= USER_EDIT_SETTLE;
        let view_settled = self.view_since.is_some_and(settled);
        let quiet = self.last_activity.is_none_or(settled);
        if quiet && !report.analysing {
            self.settled = true;
        }

        let new_changes = report.document_changes != self.counted;
        self.counted = report.document_changes;
        if self.settled {
            let changed = new_changes || self.holding;
            self.holding = false;
            return changed;
        }
        if view_settled {
            // Settling with the editor open and settled: a change is the
            // user's unless a source's analysis just ended with it.
            let analysis_ended = self.last_analysis_end.is_some_and(|at| !settled(at));
            let changed = (new_changes && !analysis_ended) || self.holding;
            self.holding = false;
            return changed;
        }
        // Settling. With the editor open, a change nothing explains could be
        // the user's: held until it can count. Everything else is the
        // plug-in's own, passed over.
        if new_changes && view_attached && quiet {
            self.holding = true;
        }
        false
    }
}

/// How long a plug-in's document has to stay quiet after a user edit before
/// the edit is taken as one undo step. A note dragged in the editor reports a
/// run of changes; the step is the whole drag, not every report in it.
const ARA_UNDO_SETTLE: Duration = Duration::from_millis(400);

/// Undo steps kept per ARA document. Each is a whole stored document, so the
/// history is bounded tighter than the project's own.
const ARA_UNDO_DEPTH: usize = 40;

/// Undo and redo for the edits made inside one ARA plug-in's editor.
///
/// ARA gives a host no call to reach a plug-in's own undo, and Melodyne does
/// not answer Ctrl+Z itself in ARA mode: it hands the key to the host, whose
/// job undo is there. So the host keeps the history: after each user edit
/// settles it stores the document, and a step restores the one before or
/// after it. `baseline` is the document as the plug-in holds it now.
#[derive(Debug, Default)]
struct AraHistory {
    baseline: Option<Arc<Vec<u8>>>,
    undo: VecDeque<Arc<Vec<u8>>>,
    redo: Vec<Arc<Vec<u8>>>,
    /// A user edit was seen and is waiting to settle into a step.
    pending_since: Option<Instant>,
    /// The host or an analysis changed the document after `baseline` was
    /// stored: it becomes the baseline again once quiet.
    rebase_since: Option<Instant>,
    /// The session's last host or analysis activity already taken in.
    activity_seen: Option<Instant>,
    /// Whether storing the untouched document twice gave the same bytes.
    stable: bool,
    /// When storing the first document last failed. Retried, but not on every
    /// frame: a store is a whole document serialised by the plug-in.
    baseline_failed_at: Option<Instant>,
}

impl AraHistory {
    /// Takes in a freshly stored document. A user edit that changed it
    /// becomes an undo step; anything else — the first document, or a change
    /// the plug-in or the host made — only moves the baseline, so undo never
    /// takes back an analysis or a clip the arrangement moved. Answers
    /// whether a step was added.
    fn record(&mut self, stored: Vec<u8>, user_edit: bool) -> bool {
        let stored = Arc::new(stored);
        match self.baseline.take() {
            Some(before) if *before == *stored => {
                self.baseline = Some(before);
                false
            }
            Some(before) if user_edit => {
                self.undo.push_back(before);
                while self.undo.len() > ARA_UNDO_DEPTH {
                    self.undo.pop_front();
                }
                self.redo.clear();
                self.baseline = Some(stored);
                true
            }
            _ => {
                self.baseline = Some(stored);
                false
            }
        }
    }

    /// The document an undo (or redo) would restore, if there is one.
    fn peek(&self, undoing: bool) -> Option<Arc<Vec<u8>>> {
        self.baseline.as_ref()?;
        if undoing {
            self.undo.back().cloned()
        } else {
            self.redo.last().cloned()
        }
    }

    /// Commits a step [`Self::peek`] offered, once the plug-in has taken it.
    fn commit(&mut self, undoing: bool) {
        let Some(current) = self.baseline.take() else {
            return;
        };
        let target = if undoing {
            self.undo.pop_back()
        } else {
            self.redo.pop()
        };
        match target {
            Some(target) => {
                if undoing {
                    self.redo.push(current);
                } else {
                    self.undo.push_back(current);
                }
                self.baseline = Some(target);
            }
            None => self.baseline = Some(current),
        }
    }

    fn depth(&self) -> (usize, usize) {
        (self.undo.len(), self.redo.len())
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
/// due to be restored, whole or the part of it kept back for audio that is
/// back now, always takes the full path, because it has to land before the
/// regions are assigned; one waiting for its audio does not, since nothing
/// restores it until a graph change builds that audio, which is a full sync
/// anyway (see [`RestoreTiming`]). A session whose last full apply did not
/// finish takes it too, because the next sync has to redo it. Otherwise only
/// a graph that creates or destroys regions needs the full path.
fn sync_path(
    restore_due: bool,
    settled: bool,
    change: impl FnOnce() -> AraGraphChange,
) -> SyncPath {
    if restore_due || !settled {
        return SyncPath::Full;
    }
    match change() {
        AraGraphChange::Unchanged => SyncPath::Timeline,
        AraGraphChange::Properties => SyncPath::Properties,
        AraGraphChange::Structure => SyncPath::Full,
    }
}

/// One saved ARA document as the project holds it: the plug-in's bytes, the
/// identifier they were written under, and what they hold.
#[derive(Clone, PartialEq)]
pub struct SavedArchive {
    pub archive_id: String,
    pub data: Vec<u8>,
    /// What `data` holds, when it was recorded (see [`ProjectAraIdentity`]).
    pub written_with: Option<ProjectAraIdentity>,
    /// The plug-in stored this document from its live session for the save
    /// being made, so `written_with` describes the audio the project holds
    /// right now. False for everything saved back verbatim (parked, kept,
    /// orphaned, or the last good copy). Never written to a file.
    pub stored_now: bool,
}

impl SavedArchive {
    /// Whether `other` is the same saved document: the same archive
    /// identifier and byte for byte the same data. What orphans are
    /// deduplicated by, exactly rather than by a digest.
    fn same_content(&self, other: &SavedArchive) -> bool {
        self.archive_id == other.archive_id && self.data == other.data
    }

    /// This document as it is kept or parked: no longer this save's store.
    fn verbatim(self) -> Self {
        Self {
            stored_now: false,
            ..self
        }
    }
}

impl std::fmt::Debug for SavedArchive {
    /// The bytes are opaque and large; their length is what a log needs.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SavedArchive")
            .field("archive_id", &self.archive_id)
            .field("bytes", &self.data.len())
            .field("written_with", &self.written_with.is_some())
            .field("stored_now", &self.stored_now)
            .finish()
    }
}

/// An archive restored in part because some of its audio was offline,
/// kept for the rest: one per session (see [`AraState::defer`]).
#[derive(Clone, Debug, PartialEq)]
pub struct DeferredRestore {
    /// The archive, byte for byte, with its record.
    pub archive: SavedArchive,
    /// Archived persistent IDs of the sources still to restore from it,
    /// each with its modifications, once its audio is back.
    pub remaining: Vec<String>,
}

/// Every saved ARA document of a project, keyed by session.
#[derive(Clone, Debug, Default)]
pub struct AraSavedDocuments {
    /// One document per (plug-in, track): what the project restores.
    pub documents: Vec<(AraSessionKey, SavedArchive)>,
    /// Saved documents kept verbatim beside those and never restored on
    /// their own: archives a restore did not show landed (see
    /// [`ArchiveFate`]), the live document of an unconfirmed session whose
    /// plug-in was removed (see [`AraState::close`]), and a kept-back archive
    /// that could not be restored any more (its plug-in removed, a newer one
    /// replacing it).
    pub orphans: Vec<(AraSessionKey, SavedArchive)>,
    /// At most one per session: the archive whose offline part is still to
    /// be restored into that session's document (see [`DeferredRestore`]).
    pub deferred: Vec<(AraSessionKey, DeferredRestore)>,
}

/// Everything [`AraState`] needs to put a project's ARA documents back after a
/// failed switch closed its sessions: taken from the live sessions when the
/// switch began.
#[derive(Clone, Debug, Default)]
pub struct AraParked {
    saved: AraSavedDocuments,
    fingerprints: HashMap<String, String>,
    /// The [`AraState`] it was taken from ([`AraState::instance`]): a
    /// generation says nothing about any other one.
    owner: u64,
    /// [`AraState::generation`] when it was taken: the same generation later
    /// means nothing replaced the sessions it came from.
    generation: u64,
}

/// What a restore means for the archive it restored.
///
/// Whatever the verdict, the live document is what the session saves from
/// then on: new work in the plug-in is never held back behind an archive.
/// What differs is whether the archive is also kept aside, and what the user
/// is told. The archive is the last good document either way, written in the
/// live document's place only if the plug-in later fails to store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ArchiveFate {
    /// The plug-in took the archive: the live document holds it now.
    Restored,
    /// Nothing shows the archive landed and nothing shows it did not (a
    /// plug-in with no note content to judge by, an archive stored before its
    /// analysis finished, a source analysed before the restore), but its
    /// record places every object it holds by its own ID into the object it
    /// was stored from ([`ara_restore_plan::restores_in_place`]). Trusted as
    /// restored.
    Trusted,
    /// Nothing shows either way, and nothing shows where the archive went: a
    /// document with no record (saved before identities were recorded), or
    /// one restored through a map. It is kept once as an orphan beside the
    /// live document. That document is stored with a record on the next
    /// save, so the next open of the project restores it as [`Self::Trusted`]
    /// or [`Self::Restored`] and adds no further copy.
    Unconfirmed,
    /// The restore showed archived state missing from the live document: a
    /// source missed, an object with no place, a refusal, or nothing asked
    /// because nothing had a place. The archive is kept at once as an
    /// orphan, byte for byte with its identity, and the user is told.
    Lost,
}

/// Judges a restore. `report` is `None` when the plug-in was not asked
/// because nothing in the archive had a place ([`AraRestorePlan::NothingMatches`]).
/// `in_place` says the archive was restored by ID with a record whose every
/// object is where it was stored from (see [`ArchiveFate::Trusted`]).
///
/// Only a verdict of matched with no error, or no evidence either way for
/// an archive restored in place, lets the live document stand for the
/// archive: a plug-in reports success for a restore that matched nothing,
/// and trusting its document then would drop the archived state for good.
fn archive_fate(report: Option<&AraRestoreReport>, in_place: bool) -> ArchiveFate {
    let Some(report) = report else {
        return ArchiveFate::Lost;
    };
    let missing = report.error.is_some()
        || report.outcome == AraRestoreOutcome::Missed
        || report
            .sources
            .iter()
            .any(|source| source.outcome == AraRestoreOutcome::Missed)
        || !report.unplaced_sources.is_empty()
        || !report.unplaced_modifications.is_empty();
    if missing {
        ArchiveFate::Lost
    } else if report.restored() {
        ArchiveFate::Restored
    } else if in_place {
        ArchiveFate::Trusted
    } else {
        ArchiveFate::Unconfirmed
    }
}

/// What a save writes for one live session, and why the plug-in's own
/// document could not be stored, if it could not.
///
/// An archive still waiting to be restored (its audio is offline, or a
/// failed apply put it back) is written as it is. Otherwise the plug-in
/// stores its live document, which becomes the last good one; when that
/// fails, the last good document is written instead of none, so a failing
/// plug-in cannot empty the project of its state.
fn document_to_save(
    pending: Option<&SavedArchive>,
    store: impl FnOnce() -> AraResult<SavedArchive>,
    last_good: &mut Option<SavedArchive>,
) -> (Option<SavedArchive>, Option<AraHostError>) {
    if let Some(archive) = pending {
        return (Some(archive.clone().verbatim()), None);
    }
    match store() {
        Ok(stored) => {
            *last_good = Some(stored.clone().verbatim());
            (Some(stored), None)
        }
        Err(error) => (last_good.clone(), Some(error)),
    }
}

/// The plug-in side of one session: its ARA document, and the instance that
/// renders it and hosts its editor.
///
/// Everything [`AraState`] asks of a plug-in goes through here, so the
/// bookkeeping around it (which saved document is restored, parked, kept or
/// written, and when a document may be destroyed) runs the same against a
/// test double that fails where a test chooses. [`LivePlugin`] is the one
/// production implementation. Main thread only, like the session itself.
trait SessionPlugin {
    fn is_poisoned(&self) -> bool;
    fn notify_model_updates(&mut self) -> AraResult<()>;
    fn analysis_incomplete(&mut self, source: &AraSourceKey) -> AraResult<Option<bool>>;
    fn set_musical_timeline(&mut self, timeline: &AraMusicalTimeline) -> AraResult<()>;
    fn graph_change(&self, graph: &AraGraph) -> AraGraphChange;
    fn apply_graph(&mut self, graph: &AraGraph) -> AraResult<()>;
    /// Builds `graph` and restores each of `restores` into it, in order, in
    /// the same edit cycle; one report per restore.
    fn apply_graph_restoring_all(
        &mut self,
        graph: &AraGraph,
        restores: &[AraRestoreRequest<'_>],
    ) -> AraResult<Vec<AraRestoreReport>>;
    /// Turns this session's renderer's render state on or off.
    fn set_rendering(&mut self, enabled: bool) -> AraResult<()>;
    fn set_renderer_regions(&mut self, clips: &[AraClipKey]) -> AraResult<()>;
    fn notify_editor_selection(
        &mut self,
        clips: &[AraClipKey],
        tracks: &[AraTrackKey],
    ) -> AraResult<()>;
    fn store_archive(&mut self) -> AraResult<AraStoredArchive>;
    /// Restores a stored document into the graph as it stands — an undo or
    /// redo step of the plug-in's own edits.
    fn restore_document(&mut self, _archive_id: &str, _bytes: &[u8]) -> AraResult<()> {
        Err(AraHostError::invalid(
            "this plug-in session cannot restore a document",
        ))
    }
    /// The instance the engine renders and editors attach to; a test double
    /// has none.
    fn instance(&self) -> Option<&DirectAudio::Vst3RuntimeProcessor>;
    /// What tells that instance apart from every other one alive
    /// ([`DirectAudio::Vst3RuntimeProcessor::handle_value`]): what an editor
    /// host holding a view on it is matched by. A test double may name one
    /// without having an instance.
    fn instance_handle(&self) -> Option<usize> {
        self.instance()
            .map(DirectAudio::Vst3RuntimeProcessor::handle_value)
    }
    /// Whether any editor still holds a view on the instance, by the
    /// plug-in's own account.
    fn view_is_attached(&self) -> bool;
    fn latency_samples(&self) -> u32;
    /// Destroys the document and lets the instance go: steps 2 to 5 of the
    /// teardown in [`AraState::close`], whose step 1 (out of the engine) has
    /// to have run.
    fn tear_down(self: Box<Self>, plugin_name: &str) -> AraResult<()>;
}

/// A plug-in hosted in this process on an ARA document: the production
/// [`SessionPlugin`].
struct LivePlugin {
    session: AraSession,
    /// Keeps the ARA main-factory class instance alive. Released before
    /// `processor`, whose module provides it.
    factory: DirectAudio::AraMainFactory,
    /// The in-process VST3 instance the engine renders. Cloned into the engine's
    /// renderer list; both handles share one C++ processor.
    processor: DirectAudio::Vst3RuntimeProcessor,
    renderer: AraRendererId,
}

impl SessionPlugin for LivePlugin {
    fn is_poisoned(&self) -> bool {
        self.session.is_poisoned()
    }

    fn notify_model_updates(&mut self) -> AraResult<()> {
        self.session.notify_model_updates()
    }

    fn analysis_incomplete(&mut self, source: &AraSourceKey) -> AraResult<Option<bool>> {
        self.session.analysis_incomplete(source)
    }

    fn set_musical_timeline(&mut self, timeline: &AraMusicalTimeline) -> AraResult<()> {
        self.session.set_musical_timeline(timeline)
    }

    fn graph_change(&self, graph: &AraGraph) -> AraGraphChange {
        self.session.graph_change(graph)
    }

    fn apply_graph(&mut self, graph: &AraGraph) -> AraResult<()> {
        self.session.apply_graph(graph)
    }

    fn apply_graph_restoring_all(
        &mut self,
        graph: &AraGraph,
        restores: &[AraRestoreRequest<'_>],
    ) -> AraResult<Vec<AraRestoreReport>> {
        self.session.apply_graph_restoring_all(graph, restores)
    }

    fn set_rendering(&mut self, enabled: bool) -> AraResult<()> {
        self.session.set_rendering(self.renderer, enabled)
    }

    fn set_renderer_regions(&mut self, clips: &[AraClipKey]) -> AraResult<()> {
        self.session.set_renderer_regions(self.renderer, clips)
    }

    fn notify_editor_selection(
        &mut self,
        clips: &[AraClipKey],
        tracks: &[AraTrackKey],
    ) -> AraResult<()> {
        self.session
            .notify_editor_selection(self.renderer, clips, tracks)
    }

    fn store_archive(&mut self) -> AraResult<AraStoredArchive> {
        self.session.store_archive()
    }

    fn restore_document(&mut self, archive_id: &str, bytes: &[u8]) -> AraResult<()> {
        self.session.restore_archive(archive_id, bytes)
    }

    fn instance(&self) -> Option<&DirectAudio::Vst3RuntimeProcessor> {
        Some(&self.processor)
    }

    fn view_is_attached(&self) -> bool {
        self.processor.view_is_attached()
    }

    fn latency_samples(&self) -> u32 {
        self.processor.get_latency_samples().max(0) as u32
    }

    fn tear_down(self: Box<Self>, plugin_name: &str) -> AraResult<()> {
        let LivePlugin {
            session,
            factory,
            processor,
            ..
        } = *self;
        // 2. Release the plug-in's editor view, through *both* paths a view can
        //    have been attached by. Last line of defence for a view that has not
        //    come down yet: every caller waits for the editors to let go first
        //    (see `StudioLayout::unbind_track_from_ara` and
        //    `StudioLayout::close_all_ara_sessions`), and one that gave up
        //    waiting says so in the log.
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
        step(plugin_name, "2/4 releasing the editor view", || {
            processor.view_detach();
            processor.embed_detach();
        });
        // 3. Leave the processing state. A plug-in whose renderer is still
        //    active is entitled to hold its render lock, and step 4 then waits
        //    on it from the main thread — which is the hang, not a crash.
        step(plugin_name, "3/4 stopping processing", || {
            processor.stop_processing()
        });
        // 4. Destroy the ARA binding and the document controller.
        //
        //    This is the step that can block: closing the document revokes the
        //    host's audio readers, and revoking one waits for whichever of the
        //    plug-in's threads is reading through it to let go.
        let closed = step(plugin_name, "4/4 closing the ARA document", || {
            session.close()
        });
        // 5. The factory goes before the module that provides it, and the
        //    processor drops with the last handle, which terminates the
        //    companion instance.
        drop(factory);
        processor.set_destroy_reason("ara-unbound");
        closed
    }
}

/// One ARA plug-in hosted on one track.
struct AraTrackSession {
    plugin: Box<dyn SessionPlugin>,
    /// Clips currently assigned to the renderer, in the order of the last full
    /// apply. A property-only sync keeps the same set.
    clips: Vec<AraClipKey>,
    /// Sources this session's clips read from, so the audio library can be
    /// rebuilt without consulting the timeline.
    audio: Arc<AudioLibrary>,
    /// Archive identifier the plug-in writes under, captured at open.
    archive_id: String,
    plugin_name: String,
    /// The last restore into this session was not confirmed either way
    /// ([`ArchiveFate::Trusted`] or [`ArchiveFate::Unconfirmed`]). Its live
    /// document is what saves, so on unbind it is kept as an orphan rather
    /// than dropped with the plug-in (see [`AraState::close`]).
    unconfirmed: bool,
    /// The last document this session is known to hold, or the archive
    /// last restored into it: that archive, then each document the plug-in
    /// stored. Written when the plug-in fails to store, rather than nothing.
    last_good: Option<SavedArchive>,
    /// The sources of the last graph applied, for asking the plug-in about
    /// their analysis.
    sources: Vec<AraSourceKey>,
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
    /// Undo and redo of the edits made in the plug-in's own editor.
    history: AraHistory,
}

impl AraTrackSession {
    fn new(
        plugin: Box<dyn SessionPlugin>,
        archive_id: String,
        plugin_name: String,
        audio: Arc<AudioLibrary>,
        signals: Arc<ModelSignals>,
    ) -> Self {
        Self {
            plugin,
            clips: Vec::new(),
            audio,
            archive_id,
            plugin_name,
            unconfirmed: false,
            last_good: None,
            sources: Vec::new(),
            signals,
            watch: DocumentWatch::default(),
            settled: false,
            installed_latency: None,
            history: AraHistory::default(),
        }
    }
}

/// A session taken out of the project whose document is not destroyed yet,
/// because an editor may still hold a view on it; see
/// [`AraState::finish_retired`].
struct RetiredSession {
    plugin_name: String,
    plugin: Box<dyn SessionPlugin>,
    /// When it left the project.
    since: Instant,
}

/// A parked document taken for one sync, with where and when it lands.
struct ParkedRestore {
    archive: SavedArchive,
    plan: AraRestorePlan,
    timing: RestoreTiming,
}

impl ParkedRestore {
    /// Whether this sync restores it, whole or in part.
    fn due(&self) -> bool {
        !matches!(self.timing, RestoreTiming::AwaitMedia { .. })
    }

    /// Whether the plug-in is asked at all. A whole restore of which nothing
    /// has a place is not ([`AraRestorePlan::NothingMatches`]); a partial one
    /// always is, since its first part brings the document data.
    fn asked(&self) -> bool {
        match &self.timing {
            RestoreTiming::Now => self.plan != AraRestorePlan::NothingMatches,
            RestoreTiming::Partly { .. } => true,
            RestoreTiming::AwaitMedia { .. } => false,
        }
    }

    /// Which part of the archive is restored now: the whole of it, or all
    /// but what is offline. A legacy archive goes whole either way: nothing
    /// says what it holds, and what is offline is not in the graph to take
    /// it.
    fn scope_now(&self) -> Option<Vec<String>> {
        match (&self.timing, self.archive.written_with.as_ref()) {
            (RestoreTiming::Partly { deferred }, Some(identity)) => {
                Some(ara_restore_plan::sources_except(identity, deferred))
            }
            _ => None,
        }
    }
}

/// The part of a kept-back archive whose audio is in the graph again.
struct ReturningPart {
    /// Archived persistent IDs of the sources restored now.
    due: Vec<String>,
    /// Where the ones that moved land.
    map: AraRestoreMap,
}

/// Where the next [`AraState`] takes its [`AraState::instance`] from.
static NEXT_ARA_STATE: AtomicU64 = AtomicU64::new(1);

/// Every live ARA session, plus the queues its plug-ins post into.
pub struct AraState {
    sessions: HashMap<AraSessionKey, AraTrackSession>,
    /// Sessions of a project that was replaced or closed, waiting for their
    /// editor views to let go before their documents are destroyed.
    retired: Vec<RetiredSession>,
    transport_inbox: Arc<Inbox<AraTransportRequest>>,
    /// Archives loaded from the project, waiting for their session to open.
    ///
    /// A saved project restores clip bindings long before the plug-ins are
    /// instantiated, so the bytes are parked here and handed over the moment the
    /// matching session opens. An apply that fails before its restore puts the
    /// archive back, so the next full sync tries again, and one whose audio is
    /// offline stays until a sync builds that audio (see [`RestoreTiming`]).
    pending_archives: HashMap<AraSessionKey, SavedArchive>,
    /// Sessions whose parked document waits for its audio, or part of it,
    /// whose user has been told so.
    awaiting_media: HashSet<AraSessionKey>,
    /// Per session, the archive restored in part because some of its audio
    /// was offline, kept for the rest (see [`Self::defer`]).
    deferred: HashMap<AraSessionKey, DeferredRestore>,
    /// Saved documents kept verbatim beside the live documents, and saved
    /// with the project: archives a restore did not show landed, and live
    /// documents of unconfirmed sessions whose plug-in was removed. One copy
    /// of each: see [`Self::keep_orphan`].
    orphans: Vec<(AraSessionKey, SavedArchive)>,
    /// Content fingerprint per asset id, from the loaded project's asset
    /// records: what a restore plan compares a moved source's audio by.
    asset_fingerprints: HashMap<String, String>,
    /// Bumped whenever the parked documents are replaced or every session is
    /// closed; see [`AraParked`].
    generation: u64,
    /// Tells this state apart from every other one in the process, so a
    /// parked snapshot is only ever compared with the state it came from.
    instance: u64,
    /// What the user has to be told about saved documents, for the status
    /// bar.
    notices: Vec<String>,
    /// Last error surfaced to the user, if any.
    pub last_error: Option<String>,
}

impl Default for AraState {
    fn default() -> Self {
        Self {
            sessions: HashMap::new(),
            retired: Vec::new(),
            transport_inbox: Arc::default(),
            pending_archives: HashMap::new(),
            awaiting_media: HashSet::new(),
            deferred: HashMap::new(),
            orphans: Vec::new(),
            asset_fingerprints: HashMap::new(),
            generation: 0,
            instance: NEXT_ARA_STATE.fetch_add(1, Ordering::Relaxed),
            notices: Vec::new(),
            last_error: None,
        }
    }
}

impl std::fmt::Debug for AraState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AraState")
            .field("sessions", &self.sessions.len())
            .field("retired", &self.retired.len())
            .field("pending_archives", &self.pending_archives.len())
            .field("deferred", &self.deferred.len())
            .field("orphans", &self.orphans.len())
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
            .and_then(|session| session.plugin.instance().cloned())
    }

    /// The handle of that instance ([`SessionPlugin::instance_handle`]), for
    /// telling whether a view an editor holds is on it: the same key can name
    /// a new instance once a project is reopened in place of another.
    pub fn instance_handle(&self, key: &AraSessionKey) -> Option<usize> {
        self.sessions
            .get(key)
            .and_then(|session| session.plugin.instance_handle())
    }

    /// Parks a project's saved documents until their sessions open, replacing
    /// whatever was parked before. `fingerprints` maps asset ids to the
    /// content fingerprints of the project's asset records.
    pub fn load_archives(
        &mut self,
        saved: AraSavedDocuments,
        fingerprints: HashMap<String, String>,
    ) {
        self.generation += 1;
        self.pending_archives.clear();
        self.awaiting_media.clear();
        self.deferred.clear();
        self.orphans.clear();
        for (key, orphan) in saved.orphans {
            self.keep_orphan(&key, orphan);
        }
        for (key, held) in saved.deferred {
            let held = DeferredRestore {
                archive: held.archive.verbatim(),
                remaining: held.remaining,
            };
            // One per session, and one with nothing left to restore is no
            // record at all; neither archive is the project's to drop.
            if held.remaining.is_empty() {
                self.keep_orphan(&key, held.archive);
            } else if let Some(displaced) = self.deferred.insert(key.clone(), held) {
                self.keep_orphan(&key, displaced.archive);
            }
        }
        for (key, archive) in saved.documents {
            // One document per session. A second one is not the project's
            // to drop: it is kept, like any document nothing can restore.
            if let Some(displaced) = self
                .pending_archives
                .insert(key.clone(), archive.verbatim())
            {
                self.keep_orphan(&key, displaced);
            }
        }
        self.asset_fingerprints = fingerprints;
    }

    /// Keeps a saved document nothing will restore, beside the live
    /// documents, once: the same document already kept for the same session
    /// is not kept again, so repeated opens, restores and saves never pile up
    /// copies of it. Answers whether it was not kept before.
    fn keep_orphan(&mut self, key: &AraSessionKey, archive: SavedArchive) -> bool {
        let archive = archive.verbatim();
        if let Some((_, kept)) = self
            .orphans
            .iter_mut()
            .find(|(held, kept)| held == key && kept.same_content(&archive))
        {
            // The same bytes: keep what is known about them.
            if kept.written_with.is_none() {
                kept.written_with = archive.written_with;
            }
            return false;
        }
        self.orphans.push((key.clone(), archive));
        true
    }

    /// Serialises every saved document for saving: each live session's, every
    /// parked one, every kept-back one and every orphan.
    ///
    /// Sessions that have never opened keep the archive they were loaded with,
    /// so saving a project whose ARA plug-ins were never instantiated does not
    /// throw their edits away. A live session writes an archive still waiting
    /// for its restore as it is, and its live document otherwise; see
    /// [`document_to_save`]. That is only ever a document whose graph held
    /// no audio at all, so no work can be held back behind it.
    ///
    /// An archive restored in part is written once, beside the live
    /// document, for as long as some of it is still to be restored
    /// ([`DeferredRestore`]); the live document is what the session saves
    /// throughout. Once all of it has been restored, the record goes with
    /// the first save that stores the live document; if the plug-in cannot
    /// store, the archive is kept as an orphan instead.
    pub fn store_archives(&mut self) -> AraSavedDocuments {
        let mut documents = Vec::new();
        let mut restored_parts: Vec<(AraSessionKey, bool)> = Vec::new();
        for (key, session) in self.sessions.iter_mut() {
            let pending = self.pending_archives.get(key);
            let archive_id = session.archive_id.clone();
            let plugin = &mut session.plugin;
            let mut store = || {
                plugin.store_archive().map(|stored| SavedArchive {
                    archive_id: archive_id.clone(),
                    data: stored.bytes,
                    written_with: Some(ara_restore_plan::project_identity(&stored.identity)),
                    stored_now: true,
                })
            };
            let (saved, error) = document_to_save(pending, &mut store, &mut session.last_good);
            let stored_live = pending.is_none() && error.is_none();
            if let Some(error) = error {
                let kept = if saved.is_some() {
                    "; its last saved state was written instead"
                } else {
                    ""
                };
                eprintln!(
                    "[ARA] {} on {} could not store its document: {error}{kept}",
                    session.plugin_name, key.track_id
                );
                self.last_error = Some(format!(
                    "{} could not save its ARA state: {error}{kept}",
                    session.plugin_name
                ));
            }
            if let Some(saved) = saved {
                documents.push((key.clone(), saved));
            }
            if self
                .deferred
                .get(key)
                .is_some_and(|held| held.remaining.is_empty())
                && pending.is_none()
            {
                restored_parts.push((key.clone(), stored_live));
            }
        }
        for (key, stored_live) in restored_parts {
            let Some(held) = self.deferred.remove(&key) else {
                continue;
            };
            if !stored_live {
                self.keep_orphan(&key, held.archive);
            }
        }
        for (key, archive) in &self.pending_archives {
            if !self.sessions.contains_key(key) {
                documents.push((key.clone(), archive.clone()));
            }
        }
        let mut deferred: Vec<(AraSessionKey, DeferredRestore)> = self
            .deferred
            .iter()
            .map(|(key, held)| (key.clone(), held.clone()))
            .collect();
        deferred.sort_by(|a, b| a.0.cmp(&b.0));
        AraSavedDocuments {
            documents,
            orphans: self.orphans.clone(),
            deferred,
        }
    }

    /// How many times the parked documents were replaced or every session
    /// closed; see [`AraParked`].
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Everything needed to put this project's ARA documents back if the
    /// sessions are replaced by a switch that then fails: each live document
    /// stored now, and everything parked.
    pub fn park_for_rollback(&mut self) -> AraParked {
        AraParked {
            saved: self.store_archives(),
            fingerprints: self.asset_fingerprints.clone(),
            owner: self.instance,
            generation: self.generation,
        }
    }

    /// Whether `parked` still describes the documents held now: it was taken
    /// from this very state, and nothing has replaced them since, so there is
    /// nothing to put back. A snapshot from another state (a workspace
    /// mounted afresh for a rollback, say) is never held, whatever its
    /// generation.
    pub fn holds(&self, parked: &AraParked) -> bool {
        self.instance == parked.owner && self.generation == parked.generation
    }

    /// Puts back what [`Self::park_for_rollback`] took, after a switch that
    /// failed. Answers false, and changes nothing, when nothing replaced
    /// those documents since: the live sessions they came from are still
    /// open. Otherwise every session now open (the other project's) is
    /// retired, as in [`Self::retire_all`], and the documents are parked for
    /// their sessions to reopen.
    pub fn roll_back(
        &mut self,
        parked: AraParked,
        engine: Option<&DirectAudio::AudioEngine>,
        now: Instant,
    ) -> bool {
        if self.holds(&parked) {
            return false;
        }
        self.retire_all(engine, now);
        self.load_archives(parked.saved, parked.fingerprints);
        true
    }

    /// What the user has to be told about saved documents since the last
    /// call.
    pub fn take_notices(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notices)
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
            if session.plugin.is_poisoned() {
                continue;
            }
            if let Err(error) = session.plugin.notify_model_updates() {
                trace(&format!(
                    "model updates for '{}' on {}: {error}",
                    key.plugin_id, key.track_id
                ));
            }
        }
    }

    /// Reads what every plug-in reported since the last poll, and answers
    /// whether any of their documents changed, so the project has unsaved
    /// work even though nothing in the timeline moved. [`DocumentWatch`]
    /// decides which changes count. Call right after
    /// [`Self::notify_model_updates`], so what that call delivered is judged
    /// as the periodic poll's and not as a later host edit's.
    ///
    /// Also settles analyses a plug-in reported started and, by its own
    /// account, has finished (see [`finished_analyses`]), so that one it never
    /// completes cannot keep the document from ever settling.
    pub fn poll_documents(&mut self, now: Instant) -> bool {
        let mut changed = false;
        for (key, session) in self.sessions.iter_mut() {
            let report = session.signals.report();
            let user_change = session
                .watch
                .poll(report, session.plugin.view_is_attached(), now);
            changed |= user_change;
            // A clip's edit changing is an edit too, from a plug-in that never
            // reports its document data changing — once the document has
            // settled, so opening the project is not one.
            let edited = session.watch.take_content_edits(report) && session.watch.settled;
            // A recorded step is unsaved work, whatever else reported it.
            changed |= track_history(session, user_change || edited, now);

            let due = analysis_check_due(
                session.signals.report().analysing,
                session.watch.last_analysis_report,
                session.watch.last_analysis_check,
                now,
            );
            if due && !session.plugin.is_poisoned() {
                session.watch.last_analysis_check = Some(now);
                let running = session.signals.running().clone();
                let plugin = &mut session.plugin;
                let finished = finished_analyses(&running, &session.sources, |source| {
                    plugin.analysis_incomplete(source).ok().flatten()
                });
                if finished.all {
                    session.signals.settle_all();
                } else {
                    for source in &finished.sources {
                        session.signals.settle(source);
                    }
                }
                if finished.all || !finished.sources.is_empty() {
                    trace(&format!(
                        "'{}' on {}: analyses the plug-in reports finished settled: {:?}{}",
                        session.plugin_name,
                        key.track_id,
                        finished
                            .sources
                            .iter()
                            .map(AraSourceKey::as_str)
                            .collect::<Vec<_>>(),
                        if finished.all { " (all)" } else { "" }
                    ));
                }
            }
        }
        changed
    }

    /// Undoes (or redoes) the last edit made in one session's plug-in editor.
    ///
    /// `Ok(false)` when there is nothing to step to. An edit still settling
    /// is taken into the history first, so an Undo pressed straight after an
    /// edit undoes that edit rather than the one before it.
    pub fn step_history(
        &mut self,
        key: &AraSessionKey,
        undoing: bool,
        now: Instant,
    ) -> Result<bool, String> {
        let Some(session) = self.sessions.get_mut(key) else {
            return Ok(false);
        };
        if session.plugin.is_poisoned() {
            return Err(format!("{} is not responding", session.plugin_name));
        }
        // An edit the plug-in reported and that has not settled yet is the
        // user's. So is whatever differs from the baseline now, when storing
        // an untouched document gives the same bytes and neither the host nor
        // an analysis has changed it since: an edit nothing reported.
        let reported = session.history.pending_since.take().is_some();
        let unreported = session.history.stable && session.history.rebase_since.is_none();
        if session.history.baseline.is_some() && (reported || unreported) {
            match session.plugin.store_archive() {
                Ok(stored) => {
                    if session.history.record(stored.bytes, true) {
                        history_trace(|| {
                            format!(
                                "'{}': {} edit taken as a step on Undo",
                                session.plugin_name,
                                if reported {
                                    "a settling"
                                } else {
                                    "an unreported"
                                }
                            )
                        });
                    }
                }
                Err(error) => history_trace(|| {
                    format!(
                        "'{}': could not store the document on Undo: {error}",
                        session.plugin_name
                    )
                }),
            }
        }
        let Some(target) = session.history.peek(undoing) else {
            history_trace(|| {
                format!(
                    "'{}': nothing to {}: first document stored={} settled={} analysing={} \
                     stable={} steps={:?}",
                    session.plugin_name,
                    if undoing { "undo" } else { "redo" },
                    session.history.baseline.is_some(),
                    session.watch.settled,
                    session.signals.report().analysing,
                    session.history.stable,
                    session.history.depth()
                )
            });
            return Ok(false);
        };
        session
            .plugin
            .restore_document(&session.archive_id, &target)
            .map_err(|error| {
                format!(
                    "{} could not {}: {error}",
                    session.plugin_name,
                    if undoing { "undo" } else { "redo" }
                )
            })?;
        session.history.commit(undoing);
        // What the plug-in reports about the restore is the host's doing,
        // not a new edit: counted as seen, so it neither becomes a step nor
        // throws away the redo that was just made possible.
        session.watch.absorb(session.signals.report(), now);
        session.history.pending_since = None;
        history_trace(|| {
            format!(
                "'{}': {} restored (steps {:?})",
                session.plugin_name,
                if undoing { "undo" } else { "redo" },
                session.history.depth()
            )
        });
        Ok(true)
    }

    /// Undo and redo steps one session's plug-in editor has.
    pub fn history_depth(&self, key: &AraSessionKey) -> (usize, usize) {
        self.sessions
            .get(key)
            .map(|session| session.history.depth())
            .unwrap_or_default()
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
        let outcome = session.plugin.set_musical_timeline(timeline);
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

        let plugin = LivePlugin {
            session,
            factory,
            processor,
            renderer,
        };
        self.sessions.insert(
            key.clone(),
            AraTrackSession::new(
                Box::new(plugin),
                archive_id,
                plugin_name.to_owned(),
                audio,
                signals,
            ),
        );
        Ok(self
            .sessions
            .get_mut(key)
            .expect("inserted immediately above"))
    }

    /// Applies a freshly built graph to one session, opening the session
    /// first when it is not open yet.
    ///
    /// What that takes depends on the [`AraGraphChange`]. A graph that creates
    /// or destroys regions goes through the full apply: renderers out of the
    /// engine and rendering off, because a playback region may not be
    /// destroyed while a renderer still holds it and ARA lets renderer
    /// assignments change only outside the render state; then regions
    /// re-assigned and the editor told. A graph with the same regions (a clip
    /// moved, trimmed or renamed, say) is updated in place while it keeps
    /// rendering, which ARA allows for property updates inside an edit cycle.
    ///
    /// `offline` lists the clips left out of `graph` because their media
    /// could not be probed, with their sources (see
    /// [`super::ara_graph::AraProjectView::offline`]).
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
        offline: &[(AraClipKey, AraSourceKey)],
    ) -> AraResult<()> {
        // A session that cannot open leaves its parked document where it is:
        // it waits for the next attempt, and is saved back untouched
        // meanwhile.
        self.ensure_session(engine, key, plugin_name, plugin_path, class_id)?;
        self.sync_open(
            Some(engine),
            key,
            plugin_name,
            timeline,
            graph,
            media_paths,
            offline,
        )
    }

    /// Brings an open session's document in line with `graph`: everything
    /// [`Self::apply`] does once the session exists. `engine` is `None` when
    /// there is no audio engine to take renderers out of or put back in.
    #[allow(clippy::too_many_arguments)]
    fn sync_open(
        &mut self,
        engine: Option<&DirectAudio::AudioEngine>,
        key: &AraSessionKey,
        plugin_name: &str,
        timeline: &AraMusicalTimeline,
        graph: &AraGraph,
        media_paths: HashMap<AraSourceKey, PathBuf>,
        offline: &[(AraClipKey, AraSourceKey)],
    ) -> AraResult<()> {
        let parked = self.pending_archives.remove(key).map(|archive| {
            let plan = self.plan_restore(&archive, graph);
            let timing =
                ara_restore_plan::restore_timing(archive.written_with.as_ref(), graph, offline);
            ParkedRestore {
                archive,
                plan,
                timing,
            }
        });
        let returning = self.returning_part(key, graph);
        let restore_due = parked.as_ref().is_some_and(ParkedRestore::due) || returning.is_some();
        let Some(session) = self.sessions.get_mut(key) else {
            self.repark(key, parked.map(|parked| parked.archive));
            return Err(AraHostError::invalid("ARA session disappeared mid-apply"));
        };

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
        let path = sync_path(restore_due, session.settled, || {
            session.plugin.graph_change(graph)
        });
        if path != SyncPath::Full {
            session.audio.publish(media_paths);
            session.sources = graph.sources.iter().map(|desc| desc.key.clone()).collect();
            let mut outcome = session.plugin.set_musical_timeline(timeline);
            if outcome.is_ok() && path == SyncPath::Properties {
                outcome = session.plugin.apply_graph(graph);
            }
            session
                .watch
                .host_edited(session.signals.report(), Instant::now());
            let reinstall = match outcome {
                Ok(()) => session.installed_latency != Some(session.plugin.latency_samples()),
                Err(_) => {
                    // Half an edit may have landed; the next sync redoes it in
                    // full. Nothing was suspended, so nothing needs
                    // reinstalling.
                    session.settled = false;
                    false
                }
            };
            // A parked document can only be waiting for its audio here (one
            // due now takes the full path): it stays parked.
            self.repark(key, parked.map(|parked| parked.archive));
            if reinstall {
                Self::install_renderers(engine, &mut self.sessions, &key.track_id);
            }
            return outcome;
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
            parked,
            returning,
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

    /// Where a parked document lands in `graph` ([`ara_restore_plan::plan_restore`]).
    fn plan_restore(&self, archive: &SavedArchive, graph: &AraGraph) -> AraRestorePlan {
        let fingerprints = &self.asset_fingerprints;
        ara_restore_plan::plan_restore(archive.written_with.as_ref(), graph, &|source| {
            fingerprints.get(source.as_str()).cloned()
        })
    }

    /// The part of the session's kept-back archive whose audio `graph` holds
    /// again, if any ([`ara_restore_plan::returning_sources`]).
    fn returning_part(&self, key: &AraSessionKey, graph: &AraGraph) -> Option<ReturningPart> {
        let held = self.deferred.get(key)?;
        let fingerprints = &self.asset_fingerprints;
        let (due, map) = ara_restore_plan::returning_sources(
            held.archive.written_with.as_ref(),
            &held.remaining,
            graph,
            &|source| fingerprints.get(source.as_str()).cloned(),
        );
        (!due.is_empty()).then_some(ReturningPart { due, map })
    }

    /// Puts an archive taken for an apply back where the next full sync picks
    /// it up, because the apply failed before the plug-in could restore it,
    /// or its audio is not there yet.
    fn repark(&mut self, key: &AraSessionKey, archive: Option<SavedArchive>) {
        let Some(archive) = archive else {
            return;
        };
        if let Some(displaced) = self.pending_archives.insert(key.clone(), archive) {
            // Nothing parks a second archive while one is out, but neither is
            // dropped if something ever does.
            self.keep_orphan(key, displaced);
        }
    }

    /// The model edit itself, with the track's renderers already suspended.
    ///
    /// A parked archive due now is restored inside the edit cycle that builds
    /// the graph, the order ARA documents for opening a saved document; see
    /// [`AraSession::apply_graph_restoring`]. Where it lands was planned
    /// ([`ara_restore_plan::plan_restore`]), and the plug-in's verdict
    /// decides whether the live document takes its place ([`archive_fate`]).
    /// When some of its audio is offline, only the rest is restored now and
    /// the archive is kept for the offline part ([`Self::defer`]); the part
    /// of a kept-back archive whose audio is back goes into the same cycle,
    /// before the parked document, which brings the plug-in's document data
    /// and so comes last (see [`AraSession::apply_graph_restoring_all`]).
    /// One whose graph holds no audio at all is not handed over and stays
    /// parked ([`Self::await_media`]). Anything that fails before the
    /// restore puts the archive back for the next full sync, and leaves the
    /// kept-back one as it was.
    #[allow(clippy::too_many_arguments)]
    fn apply_model(
        &mut self,
        engine: Option<&DirectAudio::AudioEngine>,
        key: &AraSessionKey,
        plugin_name: &str,
        timeline: &AraMusicalTimeline,
        graph: &AraGraph,
        media_paths: HashMap<AraSourceKey, PathBuf>,
        parked: Option<ParkedRestore>,
        returning: Option<ReturningPart>,
    ) -> AraResult<()> {
        // The kept-back archive restored in part here; a copy, so that a
        // failure below leaves the record exactly as it was.
        let kept_back = returning
            .as_ref()
            .and_then(|_| self.deferred.get(key))
            .map(|held| held.archive.clone());
        let Some(session) = self.sessions.get_mut(key) else {
            self.repark(key, parked.map(|parked| parked.archive));
            return Err(AraHostError::invalid("ARA session disappeared mid-apply"));
        };

        // Unsettled until `finish_apply` completes: an apply that fails part
        // way must be redone in full by the next sync, not skipped.
        session.settled = false;
        session.audio.publish(media_paths);
        session.sources = graph.sources.iter().map(|desc| desc.key.clone()).collect();
        let prepared = session
            .plugin
            .set_rendering(false)
            .and_then(|()| session.plugin.set_musical_timeline(timeline));
        if let Err(error) = prepared {
            self.repark(key, parked.map(|parked| parked.archive));
            return Err(error);
        }

        let kept_back_identity = kept_back
            .as_ref()
            .and_then(|archive| archive.written_with.as_ref())
            .map(ara_restore_plan::host_identity);
        let asked = parked.as_ref().filter(|parked| parked.asked());
        let parked_identity = asked
            .and_then(|parked| parked.archive.written_with.as_ref())
            .map(ara_restore_plan::host_identity);
        let parked_scope = asked.and_then(ParkedRestore::scope_now);
        let mut requests: Vec<AraRestoreRequest<'_>> = Vec::with_capacity(2);
        if let (Some(part), Some(archive)) = (returning.as_ref(), kept_back.as_ref()) {
            requests.push(AraRestoreRequest {
                archive_id: &archive.archive_id,
                bytes: &archive.data,
                identity: kept_back_identity.as_ref(),
                map: (!part.map.sources.is_empty()).then_some(&part.map),
                // The document data came with the first part; restoring it
                // again would overwrite what was done since.
                scope: AraRestoreScope::Sources {
                    archived: &part.due,
                    document_data: false,
                },
            });
        }
        let parked_index = asked.map(|parked| {
            requests.push(AraRestoreRequest {
                archive_id: &parked.archive.archive_id,
                bytes: &parked.archive.data,
                identity: parked_identity.as_ref(),
                map: match &parked.plan {
                    AraRestorePlan::Mapped(map) => Some(map),
                    _ => None,
                },
                scope: match &parked_scope {
                    Some(sources) => AraRestoreScope::Sources {
                        archived: sources,
                        document_data: true,
                    },
                    None => AraRestoreScope::Whole,
                },
            });
            requests.len() - 1
        });
        let mut reports = match session.plugin.apply_graph_restoring_all(graph, &requests) {
            Ok(reports) => reports,
            Err(error) => {
                // The graph was not applied and no restore counted.
                self.repark(key, parked.map(|parked| parked.archive));
                return Err(error);
            }
        };
        drop(requests);
        let parked_report = parked_index.and_then(|index| reports.get(index).cloned());
        if let (Some(part), Some(archive)) = (returning, kept_back) {
            let report = if reports.is_empty() {
                AraRestoreReport {
                    error: Some(AraHostError::invalid("the plug-in reported nothing")),
                    outcome: AraRestoreOutcome::Missed,
                    sources: Vec::new(),
                    unplaced_sources: part.due.clone(),
                    unplaced_modifications: Vec::new(),
                    harmony_resynced: false,
                }
            } else {
                reports.remove(0)
            };
            self.conclude_returning(key, plugin_name, graph, &archive, &part, &report);
        }
        if let Some(parked) = parked {
            match parked.timing.clone() {
                RestoreTiming::AwaitMedia { offline } => {
                    self.await_media(key, plugin_name, graph, parked.archive, offline);
                }
                RestoreTiming::Now => self.conclude_restore(
                    key,
                    plugin_name,
                    graph,
                    parked.archive,
                    &parked.plan,
                    None,
                    parked_report.as_ref(),
                ),
                RestoreTiming::Partly { deferred } => {
                    let part = parked_scope.clone();
                    self.conclude_restore(
                        key,
                        plugin_name,
                        graph,
                        parked.archive.clone(),
                        &parked.plan,
                        part.as_deref(),
                        parked_report.as_ref(),
                    );
                    self.defer(key, plugin_name, graph, parked.archive, deferred);
                }
            }
        }
        self.finish_apply(engine, key, graph)
    }

    /// Keeps a document parked whose graph holds no audio yet (see
    /// [`RestoreTiming::AwaitMedia`]). The next full sync that builds audio
    /// (the media relinked, a drive mounted) tries again; until then the
    /// document is saved back as it is, and on unbind it is kept as an
    /// orphan. Said once per wait, in the log and, when media is offline, to
    /// the user. A document with no record of what it holds is not promised
    /// a restore: whether it matches is only known once it is tried.
    fn await_media(
        &mut self,
        key: &AraSessionKey,
        plugin_name: &str,
        graph: &AraGraph,
        archive: SavedArchive,
        offline: bool,
    ) {
        if self.awaiting_media.insert(key.clone()) {
            let track = graph.name.as_deref().unwrap_or(key.track_id.as_str());
            eprintln!(
                "[ARA] restore of {plugin_name} on '{track}' ({}, {}): archive '{}' ({} bytes) \
                 waits for its audio ({}); kept parked unchanged",
                key.plugin_id,
                key.track_id,
                archive.archive_id,
                archive.data.len(),
                if offline {
                    "its media is offline"
                } else {
                    "the track has no audio"
                },
            );
            if offline {
                let then = if archive.written_with.is_some() {
                    "will be restored"
                } else {
                    "will be tried again"
                };
                self.notices.push(format!(
                    "{plugin_name} on '{track}': audio is offline; saved edits are kept and \
                     {then} when the audio is found."
                ));
            }
        }
        self.repark(key, Some(archive));
    }

    /// Keeps `archive` for the sources `sources` names (archived persistent
    /// IDs), whose audio was offline when the rest of it was restored: the
    /// session's one [`DeferredRestore`]. Each later full sync that builds
    /// some of that audio restores that part, and only that part, into the
    /// live document ([`Self::conclude_returning`]); meanwhile the archive is
    /// saved once beside the live document, which is what the session saves.
    ///
    /// One record per session, replaced rather than added to: the same
    /// archive again (restored in part once more) keeps its record, with the
    /// sources of both; another one replaces it, and what the old one still
    /// held back is kept as an orphan, byte for byte, so nothing is dropped.
    /// Said once, in the log and to the user.
    fn defer(
        &mut self,
        key: &AraSessionKey,
        plugin_name: &str,
        graph: &AraGraph,
        archive: SavedArchive,
        sources: Vec<String>,
    ) {
        let archive = archive.verbatim();
        let track = graph.name.as_deref().unwrap_or(key.track_id.as_str());
        eprintln!(
            "[ARA] restore of {plugin_name} on '{track}' ({}, {}): archive '{}' ({} bytes) \
             restored except {sources:?}, whose audio is offline; kept for them",
            key.plugin_id,
            key.track_id,
            archive.archive_id,
            archive.data.len(),
        );
        if self.awaiting_media.insert(key.clone()) {
            let then = if archive.written_with.is_some() {
                "will be restored"
            } else {
                "will be tried again"
            };
            self.notices.push(format!(
                "{plugin_name} on '{track}': some of its audio is offline; the saved edits for it \
                 are kept and {then} when the audio is found."
            ));
        }
        match self.deferred.remove(key) {
            Some(mut held) if held.archive.same_content(&archive) => {
                for source in sources {
                    if !held.remaining.contains(&source) {
                        held.remaining.push(source);
                    }
                }
                if held.archive.written_with.is_none() {
                    held.archive.written_with = archive.written_with;
                }
                self.deferred.insert(key.clone(), held);
            }
            displaced => {
                if let Some(displaced) = displaced {
                    if !displaced.remaining.is_empty() {
                        eprintln!(
                            "[ARA] {plugin_name} on '{track}': the archive kept for {:?} is \
                             replaced by a newer one and kept as an orphan",
                            displaced.remaining
                        );
                        self.keep_orphan(key, displaced.archive);
                    }
                }
                self.deferred.insert(
                    key.clone(),
                    DeferredRestore {
                        archive,
                        remaining: sources,
                    },
                );
            }
        }
    }

    /// Acts on the verdict for the part of a kept-back archive whose audio
    /// came back ([`archive_fate`], judged on that part alone). Those sources
    /// are done with whatever the verdict: restored or trusted, they are in
    /// the live document; otherwise the archive is kept once as an orphan
    /// (and a loss is said to the user, unless a legacy archive simply held
    /// nothing for that audio), since trying the same part again would give
    /// the same answer. The record goes once nothing is left in it and the
    /// live document has been stored ([`Self::store_archives`]).
    fn conclude_returning(
        &mut self,
        key: &AraSessionKey,
        plugin_name: &str,
        graph: &AraGraph,
        archive: &SavedArchive,
        part: &ReturningPart,
        report: &AraRestoreReport,
    ) {
        let in_place = part.map.sources.is_empty()
            && archive.written_with.as_ref().is_some_and(|identity| {
                ara_restore_plan::restores_in_place(identity, graph, Some(&part.due))
            });
        let fate = archive_fate(Some(report), in_place);
        let track = graph.name.as_deref().unwrap_or(key.track_id.as_str());
        eprintln!(
            "[ARA] restore of {plugin_name} on '{track}' ({}, {}): the part of archive '{}' ({} \
             bytes) kept for {:?}, back now: outcome {:?}, error {:?}, per source {:?}, unplaced \
             sources {:?}, unplaced modifications {:?} -> {}",
            key.plugin_id,
            key.track_id,
            archive.archive_id,
            archive.data.len(),
            part.due,
            report.outcome,
            report.error,
            report
                .sources
                .iter()
                .map(|source| (
                    source.persistent_id.as_str(),
                    source.outcome,
                    source.grade_before,
                    source.grade_after
                ))
                .collect::<Vec<_>>(),
            report.unplaced_sources,
            report.unplaced_modifications,
            match fate {
                ArchiveFate::Restored => "restored",
                ArchiveFate::Trusted =>
                    "trusted as restored (unconfirmed, every recorded object in place)",
                ArchiveFate::Unconfirmed =>
                    "kept as an orphan (unconfirmed, nothing recorded in place)",
                ArchiveFate::Lost => "kept as an orphan (not matched)",
            }
        );
        if let Some(session) = self.sessions.get_mut(key) {
            for source in &report.sources {
                if source.outcome == AraRestoreOutcome::Matched
                    || source.analysis_incomplete == Some(false)
                {
                    session.signals.settle(&source.key);
                }
            }
            if matches!(fate, ArchiveFate::Trusted | ArchiveFate::Unconfirmed) {
                session.unconfirmed = true;
            }
        }
        match fate {
            ArchiveFate::Restored | ArchiveFate::Trusted => {}
            ArchiveFate::Unconfirmed => {
                self.keep_orphan(key, archive.clone());
            }
            ArchiveFate::Lost => {
                // A legacy archive is only taken to hold that audio (see
                // `ara_restore_plan::offline_sources`): a clean miss says it
                // never did, which is nothing to warn about. It is kept all
                // the same.
                let known_loss = archive.written_with.is_some() || report.error.is_some();
                if self.keep_orphan(key, archive.clone()) && known_loss {
                    self.notices.push(format!(
                        "{plugin_name} on '{track}': saved edits for audio that is back could \
                         not be matched to it and are kept in the project unchanged."
                    ));
                }
            }
        }
        if let Some(held) = self.deferred.get_mut(key) {
            held.remaining.retain(|source| !part.due.contains(source));
        }
    }

    /// Acts on a restore's verdict ([`archive_fate`]). The live document
    /// saves from now on whatever the verdict, and the archive is the last
    /// good document until the plug-in first stores. An archive that is not
    /// shown or trusted to have landed is also kept once as an orphan, and a
    /// lost one is said to the user. One line goes to the log either way,
    /// naming the archive, the IDs on both sides and the verdict, because a
    /// restore that lands nothing is otherwise silent. `part` names the
    /// archived sources a partial restore brought (the rest is kept back and
    /// not judged here).
    #[allow(clippy::too_many_arguments)]
    fn conclude_restore(
        &mut self,
        key: &AraSessionKey,
        plugin_name: &str,
        graph: &AraGraph,
        archive: SavedArchive,
        plan: &AraRestorePlan,
        part: Option<&[String]>,
        report: Option<&AraRestoreReport>,
    ) {
        self.awaiting_media.remove(key);
        let in_place = *plan == AraRestorePlan::AsSaved
            && archive
                .written_with
                .as_ref()
                .is_some_and(|identity| ara_restore_plan::restores_in_place(identity, graph, part));
        let fate = archive_fate(report, in_place);
        let track = graph.name.as_deref().unwrap_or(key.track_id.as_str());
        eprintln!(
            "[ARA] restore of {plugin_name} on '{track}' ({}, {}): archive '{}' ({} bytes), \
             plan {}, archived sources {}, current sources {:?}, {} -> {}",
            key.plugin_id,
            key.track_id,
            archive.archive_id,
            archive.data.len(),
            plan.label(),
            archive
                .written_with
                .as_ref()
                .map(|identity| format!(
                    "{:?}",
                    identity
                        .sources
                        .iter()
                        .map(|source| source.persistent_id.as_str())
                        .collect::<Vec<_>>()
                ))
                .unwrap_or_else(|| "unrecorded".to_string()),
            graph
                .sources
                .iter()
                .map(|desc| ara_persistent_id(desc.key.as_str()).into_owned())
                .collect::<Vec<_>>(),
            match report {
                None => "not asked: nothing in the archive has a place".to_string(),
                Some(report) => format!(
                    "outcome {:?}, error {:?}, per source {:?}, unplaced sources {:?}, unplaced \
                     modifications {:?}",
                    report.outcome,
                    report.error,
                    report
                        .sources
                        .iter()
                        .map(|source| (
                            source.persistent_id.as_str(),
                            source.archived_id.as_deref(),
                            source.outcome
                        ))
                        .collect::<Vec<_>>(),
                    report.unplaced_sources,
                    report.unplaced_modifications,
                ),
            },
            match fate {
                ArchiveFate::Restored => "restored",
                ArchiveFate::Trusted =>
                    "trusted as restored (unconfirmed, every recorded object in place)",
                ArchiveFate::Unconfirmed =>
                    "kept as an orphan (unconfirmed, nothing recorded in place); the live \
                     document saves",
                ArchiveFate::Lost => "kept as an orphan (not matched); the live document saves",
            }
        );

        let Some(session) = self.sessions.get_mut(key) else {
            self.repark(key, Some(archive));
            return;
        };
        // A source the restore filled, or that the plug-in says is not
        // analysing, has no analysis running whatever it reported before.
        for source in report.iter().flat_map(|report| report.sources.iter()) {
            if source.outcome == AraRestoreOutcome::Matched
                || source.analysis_incomplete == Some(false)
            {
                session.signals.settle(&source.key);
            }
        }
        // Whatever the verdict, the live document saves from now on; the
        // archive is written in its place only if the plug-in cannot store.
        let archive = archive.verbatim();
        session.last_good = Some(archive.clone());
        session.unconfirmed |= matches!(fate, ArchiveFate::Trusted | ArchiveFate::Unconfirmed);
        match fate {
            ArchiveFate::Restored | ArchiveFate::Trusted => {}
            ArchiveFate::Unconfirmed => {
                self.keep_orphan(key, archive);
            }
            ArchiveFate::Lost => {
                self.notices.push(format!(
                    "{plugin_name} on '{track}': saved edits could not be matched to this \
                     project's audio and are kept in the project unchanged; new edits are saved \
                     beside them."
                ));
                self.keep_orphan(key, archive);
            }
        }
    }

    /// Takes a track's ARA renderers out of the engine and waits for the audio
    /// callback to confirm it.
    ///
    /// `set_ara_renderers` only *queues* the change, so without this barrier the
    /// callback can still be inside `process()` on the very instance whose ARA
    /// model is about to be edited or destroyed. A `false` ack means the barrier
    /// was not confirmed — no stream is open, or the callback is stalled — and
    /// in neither case is anything processing, so the caller carries on.
    /// Without an engine nothing renders at all.
    ///
    /// Control thread only: it blocks, briefly, on the audio callback.
    fn suspend_renderers(engine: Option<&DirectAudio::AudioEngine>, track_id: &str) {
        let Some(engine) = engine else {
            return;
        };
        if let Err(error) = engine.set_ara_renderers(track_id.to_string(), Vec::new()) {
            eprintln!("[ARA] could not suspend renderers for track {track_id}: {error}");
            return;
        }
        let _ = engine.wait_for_command_barrier(RENDERER_BARRIER_TIMEOUT);
    }

    fn finish_apply(
        &mut self,
        engine: Option<&DirectAudio::AudioEngine>,
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
        session.plugin.set_renderer_regions(&session.clips)?;
        // What the plug-in renders and what its editor shows are separate in
        // ARA 2; without this the docked editor opens on an empty canvas.
        let tracks: Vec<AraTrackKey> = graph
            .sequences
            .iter()
            .map(|sequence| sequence.key.clone())
            .collect();
        if let Err(error) = session
            .plugin
            .notify_editor_selection(&session.clips, &tracks)
        {
            // A plug-in without the editor-view role is not an error worth
            // failing the whole apply over; it just has no view to tell.
            eprintln!("[ARA] editor selection not published: {error}");
        }
        session.plugin.set_rendering(true)?;
        session.settled = true;

        Self::install_renderers(engine, &mut self.sessions, &key.track_id);
        Ok(())
    }

    /// Rebuilds and installs the engine's renderer list for one track.
    ///
    /// The engine replaces a track's whole list at once, so every session on the
    /// track has to be sent together — installing one at a time would drop the
    /// others. Records the latency each renderer went in with. Without an
    /// engine there is nothing to install into.
    fn install_renderers(
        engine: Option<&DirectAudio::AudioEngine>,
        sessions: &mut HashMap<AraSessionKey, AraTrackSession>,
        track_id: &str,
    ) {
        let Some(engine) = engine else {
            return;
        };
        let renderers: Vec<DirectAudio::RuntimeAraRenderer> = sessions
            .iter_mut()
            .filter(|(key, _)| key.track_id == track_id)
            .filter_map(|(key, session)| {
                let processor = session.plugin.instance()?.clone();
                let latency_samples = session.plugin.latency_samples();
                session.installed_latency = Some(latency_samples);
                Some(DirectAudio::RuntimeAraRenderer {
                    instance_id: format!("ara:{}:{}", key.plugin_id, key.track_id),
                    latency_samples,
                    processor,
                })
            })
            .collect();
        if let Err(error) = engine.set_ara_renderers(track_id.to_string(), renderers) {
            eprintln!("[ARA] could not install renderers for track {track_id}: {error}");
        }
    }

    /// Takes one session out of the project, leaving its document alive.
    ///
    /// Saved state that never reached its live document stays: a document
    /// still parked (waiting for its audio, or put back by a failed apply),
    /// and an archive kept back for audio that is still offline, are kept as
    /// orphans. Archives kept aside by a restore were orphans already.
    ///
    /// Step 1 of the teardown runs here: the instance leaves the audio graph,
    /// confirmed by the callback. The removal is only *queued*, so the
    /// barrier is part of this step: the region assignments dropped when the
    /// document closes call into a plug-in the callback would otherwise still
    /// be rendering. `engine` is `None` when the audio engine is already
    /// gone, and with it every renderer.
    fn take_out(
        &mut self,
        engine: Option<&DirectAudio::AudioEngine>,
        key: &AraSessionKey,
    ) -> Option<AraTrackSession> {
        let session = self.sessions.remove(key)?;
        self.awaiting_media.remove(key);
        if let Some(pending) = self.pending_archives.remove(key) {
            self.keep_orphan(key, pending);
        }
        if let Some(held) = self.deferred.remove(key) {
            if !held.remaining.is_empty() {
                self.keep_orphan(key, held.archive);
            }
        }
        let plugin_name = session.plugin_name.as_str();
        let confirmed = match engine {
            Some(engine) => step(plugin_name, "1/4 removing renderers", || {
                Self::install_renderers(Some(engine), &mut self.sessions, &key.track_id);
                engine.wait_for_command_barrier(RENDERER_BARRIER_TIMEOUT)
            }),
            None => {
                eprintln!("[ara-close] '{plugin_name}' 1/4 no audio engine: nothing renders");
                true
            }
        };
        if !confirmed {
            // Not fatal by itself, but it is the precondition every later step
            // assumes, so it must not pass silently: the audio callback may
            // still be inside this instance while step 3 deactivates it.
            eprintln!(
                "[ara-close] WARNING '{plugin_name}': the audio callback did not confirm the \
                 renderer removal within {RENDERER_BARRIER_TIMEOUT:?}"
            );
        }
        Some(session)
    }

    /// Tears down one session and removes its renderer from the track.
    ///
    /// The live document goes with the session, as removing the plug-in means,
    /// except when the restore into it was not confirmed either way: nothing
    /// shows that document holds everything it was restored from, and it
    /// holds whatever was done in it since, so it is kept as an orphan (see
    /// [`Self::keep_unconfirmed_live_document`]). See [`Self::take_out`] for
    /// the saved state that stays.
    ///
    /// Teardown order, and every step of it matters. Each is timed and
    /// reported, because the failure mode when one is wrong is a hung main
    /// thread and the only thing that identifies which call never returned is
    /// the absence of the line after it. That is worth four lines per removal
    /// whether or not a debug flag is set — this runs once when a user
    /// removes ARA from a track, not on any hot path. The shape is: out of the
    /// audio graph ([`Self::take_out`]), then the UI down, then the audio
    /// instance down, then the document ([`SessionPlugin::tear_down`]).
    /// Nothing that calls into the plug-in may sit between deactivating the
    /// instance and destroying the document.
    ///
    /// The caller waits for every editor view on the session to let go first
    /// (see `StudioLayout::unbind_track_from_ara`); the view release inside
    /// the teardown is the last line of defence, not the plan.
    pub fn close(&mut self, engine: Option<&DirectAudio::AudioEngine>, key: &AraSessionKey) {
        self.keep_unconfirmed_live_document(key);
        let Some(session) = self.take_out(engine, key) else {
            return;
        };
        let plugin_name = session.plugin_name;
        if let Err(error) = session.plugin.tear_down(&plugin_name) {
            self.last_error = Some(format!("{plugin_name} reported errors on close: {error}"));
        }
    }

    /// Keeps the live document of a session whose restore was not confirmed
    /// ([`AraTrackSession::unconfirmed`]) as an orphan, before the session
    /// goes: the plug-in stores it now, or, when it cannot, the last good
    /// document is kept instead. Only for an unbind; a project that is
    /// closed or replaced saved (or discarded) its documents already.
    fn keep_unconfirmed_live_document(&mut self, key: &AraSessionKey) {
        let Some(session) = self.sessions.get_mut(key) else {
            return;
        };
        if !session.unconfirmed {
            return;
        }
        let stored = if session.plugin.is_poisoned() {
            Err(AraHostError::Poisoned)
        } else {
            session.plugin.store_archive()
        };
        let kept = match stored {
            Ok(stored) => Some(SavedArchive {
                archive_id: session.archive_id.clone(),
                data: stored.bytes,
                written_with: Some(ara_restore_plan::project_identity(&stored.identity)),
                stored_now: false,
            }),
            Err(error) => {
                eprintln!(
                    "[ARA] {} on {} could not store its unconfirmed document before the unbind: \
                     {error}; its last saved state is kept instead",
                    session.plugin_name, key.track_id
                );
                session.last_good.clone()
            }
        };
        if let Some(kept) = kept {
            self.keep_orphan(key, kept);
        }
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
            .is_some_and(|session| session.plugin.view_is_attached())
    }

    /// Takes every session out of the project and forgets every saved
    /// document (parked, kept back or orphaned), e.g. when the project is
    /// replaced or closed: they belong to
    /// that project, and the next one must neither save them nor get them
    /// back.
    ///
    /// Synchronous for everything the next project can see: renderers leave
    /// the engine now (step 1, [`Self::take_out`]), and no session, parked
    /// document or orphan is left, so the next project can open sessions of
    /// its own under the same keys at once. The documents themselves are not
    /// destroyed here, because an editor view may still hold one: a docked
    /// panel lets go on a deferred tick, a popped-out window when GPUI
    /// removes it, and destroying a document under a live view takes the app
    /// down. They wait in [`Self::finish_retired`] instead.
    pub fn retire_all(&mut self, engine: Option<&DirectAudio::AudioEngine>, now: Instant) {
        for key in self.sessions.keys().cloned().collect::<Vec<_>>() {
            if let Some(session) = self.take_out(engine, &key) {
                self.retired.push(RetiredSession {
                    plugin_name: session.plugin_name,
                    plugin: session.plugin,
                    since: now,
                });
            }
        }
        self.pending_archives.clear();
        self.awaiting_media.clear();
        self.deferred.clear();
        self.orphans.clear();
        self.asset_fingerprints.clear();
        self.notices.clear();
        self.generation += 1;
    }

    /// Whether a retired session's document is still waiting to be
    /// destroyed.
    pub fn has_retired(&self) -> bool {
        !self.retired.is_empty()
    }

    /// Destroys the document of every retired session no editor view holds
    /// any more (steps 2 to 5 of [`Self::close`]), and answers whether any is
    /// left waiting.
    ///
    /// `released` answers for the view hosts the caller owns (the docked
    /// panel, say) whether they have let go of an instance, named by its
    /// handle ([`SessionPlugin::instance_handle`]); the plug-in is asked as
    /// well. A session still held `patience` after it was retired is
    /// destroyed anyway, and the log says so: a plug-in that never lets go
    /// must not keep its document alive for the rest of the run.
    pub fn finish_retired(
        &mut self,
        now: Instant,
        patience: Duration,
        released: impl Fn(usize) -> bool,
    ) -> bool {
        let mut index = 0;
        while index < self.retired.len() {
            let retired = &self.retired[index];
            let free = !retired.plugin.view_is_attached()
                && retired.plugin.instance_handle().is_none_or(&released);
            let overdue = now.saturating_duration_since(retired.since) >= patience;
            if !free && !overdue {
                index += 1;
                continue;
            }
            let retired = self.retired.remove(index);
            if !free {
                // Said out loud, and not behind a debug flag: closing the
                // document while a view is still on it is the state that hangs
                // or crashes the app. A freeze report with this line above it
                // and no `[ara-close] ... done` line after it names the
                // culprit exactly.
                eprintln!(
                    "[ara-close] WARNING: '{}' still holds its editor view after {patience:?}; \
                     closing the document anyway",
                    retired.plugin_name
                );
            }
            if let Err(error) = retired.plugin.tear_down(&retired.plugin_name) {
                self.last_error = Some(format!(
                    "{} reported errors on close: {error}",
                    retired.plugin_name
                ));
            }
        }
        !self.retired.is_empty()
    }

    /// Tears down every session at once and forgets every saved document:
    /// [`Self::retire_all`] without the wait, for when no editor view can be
    /// open (no GPUI window, tests, a scratch driver).
    pub fn close_all(&mut self, engine: Option<&DirectAudio::AudioEngine>) {
        let now = Instant::now();
        self.retire_all(engine, now);
        self.finish_retired(now, Duration::ZERO, |_| true);
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

    fn session_key(plugin_id: &str, track_id: &str) -> AraSessionKey {
        AraSessionKey {
            plugin_id: plugin_id.to_string(),
            track_id: track_id.to_string(),
        }
    }

    fn saved(archive_id: &str, data: &[u8]) -> SavedArchive {
        SavedArchive {
            archive_id: archive_id.to_string(),
            data: data.to_vec(),
            written_with: None,
            stored_now: false,
        }
    }

    fn documents(entries: Vec<(AraSessionKey, SavedArchive)>) -> AraSavedDocuments {
        AraSavedDocuments {
            documents: entries,
            ..AraSavedDocuments::default()
        }
    }

    // ── A plug-in double ────────────────────────────────────────────────────
    //
    // `AraState` drives it through the same `SessionPlugin` seam as a real
    // plug-in, so the bookkeeping under test is the production code: only
    // the plug-in's answers are scripted.

    /// One restore the double was asked for, in one build.
    #[derive(Clone, Debug, PartialEq)]
    struct Asked {
        archive_id: String,
        data: Vec<u8>,
        mapped: bool,
        /// `None` for the whole archive; the archived sources and whether the
        /// document data comes too, for part of it.
        part: Option<(Vec<String>, bool)>,
    }

    /// How the double answers, and what it was asked.
    struct Script {
        /// `set_rendering` and `set_musical_timeline` fail.
        prepare_fails: bool,
        /// `apply_graph_restoring_all` fails, as a graph that does not
        /// validate does.
        build_fails: bool,
        /// What restores report, one each, in order; `report` once these
        /// run out.
        reports: std::collections::VecDeque<AraRestoreReport>,
        /// What a restore reports.
        report: AraRestoreReport,
        /// What the graph looks like against the document.
        change: AraGraphChange,
        /// What `store_archive` returns; `None` fails.
        stored: Option<Vec<u8>>,
        /// The record `store_archive` returns with it.
        stored_identity: sphere_ara_host::AraArchiveIdentity,
        /// Whether an editor holds a view on the instance, by the plug-in's
        /// own account.
        view_attached: bool,
        /// The handle the instance is known by, if the double names one.
        handle: Option<usize>,
        /// Each graph built: every restore asked for with it, in order.
        builds: Vec<Vec<Asked>>,
        torn_down: bool,
    }

    impl Script {
        /// Each build's last restore (the one that brings the document
        /// data), as (archive id, through a map), or `None` for a build with
        /// no restore.
        fn restored(&self) -> Vec<Option<(String, bool)>> {
            self.builds
                .iter()
                .map(|asked| {
                    asked
                        .last()
                        .map(|asked| (asked.archive_id.clone(), asked.mapped))
                })
                .collect()
        }
    }

    impl Default for Script {
        fn default() -> Self {
            Self {
                prepare_fails: false,
                build_fails: false,
                reports: std::collections::VecDeque::new(),
                report: restore_report(AraRestoreOutcome::Matched, &[AraRestoreOutcome::Matched]),
                change: AraGraphChange::Structure,
                stored: Some(b"live".to_vec()),
                stored_identity: sphere_ara_host::AraArchiveIdentity::default(),
                view_attached: false,
                handle: None,
                builds: Vec::new(),
                torn_down: false,
            }
        }
    }

    struct FakePlugin(std::rc::Rc<std::cell::RefCell<Script>>);

    impl SessionPlugin for FakePlugin {
        fn is_poisoned(&self) -> bool {
            false
        }
        fn notify_model_updates(&mut self) -> AraResult<()> {
            Ok(())
        }
        fn analysis_incomplete(&mut self, _source: &AraSourceKey) -> AraResult<Option<bool>> {
            Ok(None)
        }
        fn set_musical_timeline(&mut self, _timeline: &AraMusicalTimeline) -> AraResult<()> {
            if self.0.borrow().prepare_fails {
                return Err(AraHostError::invalid("timeline refused"));
            }
            Ok(())
        }
        fn graph_change(&self, _graph: &AraGraph) -> AraGraphChange {
            self.0.borrow().change
        }
        fn apply_graph(&mut self, _graph: &AraGraph) -> AraResult<()> {
            Ok(())
        }
        fn apply_graph_restoring_all(
            &mut self,
            _graph: &AraGraph,
            restores: &[AraRestoreRequest<'_>],
        ) -> AraResult<Vec<AraRestoreReport>> {
            let mut script = self.0.borrow_mut();
            if script.build_fails {
                return Err(AraHostError::invalid(
                    "region clip-1 references unknown source",
                ));
            }
            script.builds.push(
                restores
                    .iter()
                    .map(|request| Asked {
                        archive_id: request.archive_id.to_owned(),
                        data: request.bytes.to_vec(),
                        mapped: request.map.is_some(),
                        part: match request.scope {
                            AraRestoreScope::Whole => None,
                            AraRestoreScope::Sources {
                                archived,
                                document_data,
                            } => Some((archived.to_vec(), document_data)),
                        },
                    })
                    .collect(),
            );
            Ok(restores
                .iter()
                .map(|_| {
                    let next = script.reports.pop_front();
                    next.unwrap_or_else(|| script.report.clone())
                })
                .collect())
        }
        fn set_rendering(&mut self, _enabled: bool) -> AraResult<()> {
            if self.0.borrow().prepare_fails {
                return Err(AraHostError::invalid("rendering refused"));
            }
            Ok(())
        }
        fn set_renderer_regions(&mut self, _clips: &[AraClipKey]) -> AraResult<()> {
            Ok(())
        }
        fn notify_editor_selection(
            &mut self,
            _clips: &[AraClipKey],
            _tracks: &[AraTrackKey],
        ) -> AraResult<()> {
            Ok(())
        }
        fn store_archive(&mut self) -> AraResult<AraStoredArchive> {
            let script = self.0.borrow();
            match script.stored.clone() {
                Some(bytes) => Ok(AraStoredArchive {
                    bytes,
                    identity: script.stored_identity.clone(),
                }),
                None => Err(AraHostError::Poisoned),
            }
        }
        fn instance(&self) -> Option<&DirectAudio::Vst3RuntimeProcessor> {
            None
        }
        fn instance_handle(&self) -> Option<usize> {
            self.0.borrow().handle
        }
        fn view_is_attached(&self) -> bool {
            self.0.borrow().view_attached
        }
        fn latency_samples(&self) -> u32 {
            0
        }
        fn tear_down(self: Box<Self>, _plugin_name: &str) -> AraResult<()> {
            self.0.borrow_mut().torn_down = true;
            Ok(())
        }
    }

    const ARCHIVE_ID: &str = "com.celemony.ara.chunk.13";

    /// Opens a session on `key` whose plug-in is the double.
    fn open_fake(
        state: &mut AraState,
        key: &AraSessionKey,
    ) -> std::rc::Rc<std::cell::RefCell<Script>> {
        let script = std::rc::Rc::new(std::cell::RefCell::new(Script::default()));
        state.sessions.insert(
            key.clone(),
            AraTrackSession::new(
                Box::new(FakePlugin(std::rc::Rc::clone(&script))),
                ARCHIVE_ID.to_string(),
                "Melodyne".to_string(),
                Arc::new(AudioLibrary::default()),
                Arc::new(ModelSignals::default()),
            ),
        );
        script
    }

    fn source_desc(key: &str) -> sphere_ara_host::AraAudioSourceDesc {
        sphere_ara_host::AraAudioSourceDesc {
            key: AraSourceKey::from(key),
            name: key.to_owned(),
            sample_rate: 44_100.0,
            frame_count: 1_205_470,
            channel_count: 2,
        }
    }

    fn region_desc(clip: &str, source: &str) -> sphere_ara_host::AraPlaybackRegionDesc {
        sphere_ara_host::AraPlaybackRegionDesc {
            key: AraClipKey::from(clip),
            source: AraSourceKey::from(source),
            track: AraTrackKey::from("track-1"),
            name: clip.to_owned(),
            start_in_modification: 0.0,
            duration_in_modification: 1.0,
            start_in_playback: 0.0,
            duration_in_playback: 1.0,
            transform: sphere_ara_host::AraPlaybackTransform::NONE,
            color: None,
        }
    }

    /// The track's graph: one clip per (clip, source) pair.
    fn track_graph(clips: &[(&str, &str)]) -> AraGraph {
        let mut sources: Vec<sphere_ara_host::AraAudioSourceDesc> = Vec::new();
        for (_, source) in clips {
            if !sources.iter().any(|desc| desc.key.as_str() == *source) {
                sources.push(source_desc(source));
            }
        }
        AraGraph {
            name: Some("Audio 1".to_owned()),
            sources,
            sequences: Vec::new(),
            regions: clips
                .iter()
                .map(|(clip, source)| region_desc(clip, source))
                .collect(),
        }
    }

    /// One sync of an open session, exactly as `apply` runs it once the
    /// session exists, with no audio engine.
    fn sync(
        state: &mut AraState,
        key: &AraSessionKey,
        graph: &AraGraph,
        offline: &[(AraClipKey, AraSourceKey)],
    ) -> AraResult<()> {
        state.sync_open(
            None,
            key,
            "Melodyne",
            &AraMusicalTimeline::default(),
            graph,
            HashMap::new(),
            offline,
        )
    }

    fn evidence_key() -> AraSessionKey {
        session_key("vst3:fd5c205bb907b3ca", "track-1")
    }

    /// The evidence project's document: legacy, no record of what it holds.
    fn evidence_archive() -> SavedArchive {
        saved(ARCHIVE_ID, b"GNBKVAi\0 evidence")
    }

    const SOURCE: &str = "Assets/Audio/1.wav";
    const OTHER: &str = "Assets/Audio/2.wav";

    /// The record a store returns for a document over `track_graph(clips)`.
    fn host_record(clips: &[(&str, &str)]) -> sphere_ara_host::AraArchiveIdentity {
        let graph = track_graph(clips);
        sphere_ara_host::AraArchiveIdentity {
            sources: graph
                .sources
                .iter()
                .map(|desc| sphere_ara_host::AraArchivedSource {
                    persistent_id: ara_persistent_id(desc.key.as_str()).into_owned(),
                    key: desc.key.clone(),
                    sample_rate: desc.sample_rate,
                    frame_count: desc.frame_count,
                    channel_count: desc.channel_count,
                })
                .collect(),
            modifications: graph
                .regions
                .iter()
                .map(|region| sphere_ara_host::AraArchivedModification {
                    persistent_id: ara_persistent_id(region.key.as_str()).into_owned(),
                    clip: region.key.clone(),
                    source_persistent_id: ara_persistent_id(region.source.as_str()).into_owned(),
                })
                .collect(),
            keys: None,
        }
    }

    /// The evidence document, saved with a record of `clips`.
    fn recorded_archive(clips: &[(&str, &str)]) -> SavedArchive {
        SavedArchive {
            written_with: Some(ara_restore_plan::project_identity(&host_record(clips))),
            ..evidence_archive()
        }
    }

    fn unconfirmed_report() -> AraRestoreReport {
        let mut report = restore_report(
            AraRestoreOutcome::Inconclusive,
            &[AraRestoreOutcome::Inconclusive],
        );
        // Stored before its analysis finished: nothing analysed either side.
        report.sources[0].grade_after = Some(0);
        report
    }

    /// Finding (review): the re-park after a failed apply was only ever
    /// called by hand. Here the plug-in refuses the timeline before the
    /// build: the document goes back where the next full sync finds it, is
    /// saved as it was meanwhile, and the next sync restores it.
    #[test]
    fn a_failure_before_the_build_puts_the_document_back_for_the_next_sync() {
        let mut state = AraState::default();
        let key = evidence_key();
        state.load_archives(
            documents(vec![(key.clone(), evidence_archive())]),
            HashMap::new(),
        );
        let script = open_fake(&mut state, &key);
        script.borrow_mut().prepare_fails = true;
        let graph = track_graph(&[("clip-1", "Assets/Audio/1.wav")]);

        assert!(sync(&mut state, &key, &graph, &[]).is_err());
        assert_eq!(
            state.pending_archives.get(&key),
            Some(&evidence_archive()),
            "put back"
        );
        assert!(script.borrow().builds.is_empty(), "nothing was built");
        // Saved as it was, not the live document.
        assert_eq!(
            state.store_archives().documents,
            vec![(key.clone(), evidence_archive())]
        );

        script.borrow_mut().prepare_fails = false;
        sync(&mut state, &key, &graph, &[]).unwrap();
        assert_eq!(
            script.borrow().restored(),
            vec![Some((ARCHIVE_ID.to_string(), false))],
            "the retry restores it inside the build"
        );
        assert!(state.pending_archives.is_empty());
    }

    /// The build itself fails (the bridge refuses the graph): the graph was
    /// not applied and the restore did not count, so the document goes back.
    #[test]
    fn a_failed_build_puts_the_document_back_for_the_next_sync() {
        let mut state = AraState::default();
        let key = evidence_key();
        state.load_archives(
            documents(vec![(key.clone(), evidence_archive())]),
            HashMap::new(),
        );
        let script = open_fake(&mut state, &key);
        script.borrow_mut().build_fails = true;
        let graph = track_graph(&[("clip-1", "Assets/Audio/1.wav")]);

        assert!(sync(&mut state, &key, &graph, &[]).is_err());
        assert_eq!(state.pending_archives.get(&key), Some(&evidence_archive()));
        assert!(
            !state.sessions[&key].settled,
            "the next sync redoes it in full"
        );
        assert_eq!(
            state.store_archives().documents,
            vec![(key.clone(), evidence_archive())]
        );

        script.borrow_mut().build_fails = false;
        sync(&mut state, &key, &graph, &[]).unwrap();
        assert_eq!(script.borrow().builds.len(), 1);
        assert!(state.pending_archives.is_empty());
        assert_eq!(
            state.sessions[&key].last_good,
            Some(evidence_archive()),
            "matched: the live document holds it now"
        );
    }

    /// Finding (review, verified with Melodyne): the evidence project opened
    /// with its media offline had its document restored into a graph with no
    /// sources, judged unconfirmed without a word, never retried, and dropped
    /// on unbind. Now it is not handed over, stays parked, is saved as it
    /// was, is retried when the audio comes back, and is never dropped.
    #[test]
    fn a_document_whose_audio_is_offline_waits_for_it() {
        let mut state = AraState::default();
        let key = evidence_key();
        state.load_archives(
            documents(vec![(key.clone(), evidence_archive())]),
            HashMap::new(),
        );
        let script = open_fake(&mut state, &key);
        let offline_graph = track_graph(&[]);
        let offline = vec![(
            AraClipKey::from("clip-1"),
            AraSourceKey::from("Assets/Audio/1.wav"),
        )];

        sync(&mut state, &key, &offline_graph, &offline).unwrap();
        assert_eq!(
            script.borrow().restored(),
            vec![None],
            "built, not restored"
        );
        assert_eq!(state.pending_archives.get(&key), Some(&evidence_archive()));
        let notices = state.take_notices();
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("audio is offline"), "{notices:?}");
        // Saved as it was; the live document holds no audio, nothing beside.
        let stored = state.store_archives();
        assert_eq!(stored.documents, vec![(key.clone(), evidence_archive())]);
        assert!(stored.orphans.is_empty());

        // Later syncs while it stays offline: no restore, no second notice,
        // and no full rebuild just because a document is parked.
        script.borrow_mut().change = AraGraphChange::Unchanged;
        sync(&mut state, &key, &offline_graph, &offline).unwrap();
        assert_eq!(script.borrow().builds.len(), 1, "the light path");
        assert!(state.take_notices().is_empty());
        assert!(state.pending_archives.contains_key(&key));

        // The media is found: the graph gains the source and the restore runs.
        script.borrow_mut().change = AraGraphChange::Structure;
        let online = track_graph(&[("clip-1", "Assets/Audio/1.wav")]);
        sync(&mut state, &key, &online, &[]).unwrap();
        assert_eq!(
            script.borrow().restored().last(),
            Some(&Some((ARCHIVE_ID.to_string(), false)))
        );
        assert!(state.pending_archives.is_empty());
    }

    /// Unbinding the plug-in while its document waits for its audio keeps
    /// the document, as an orphan.
    #[test]
    fn a_document_waiting_for_its_audio_survives_the_unbind() {
        let mut state = AraState::default();
        let key = evidence_key();
        state.load_archives(
            documents(vec![(key.clone(), evidence_archive())]),
            HashMap::new(),
        );
        let script = open_fake(&mut state, &key);
        let offline = vec![(
            AraClipKey::from("clip-1"),
            AraSourceKey::from("Assets/Audio/1.wav"),
        )];
        sync(&mut state, &key, &track_graph(&[]), &offline).unwrap();

        state.close(None, &key);
        assert!(script.borrow().torn_down);
        let stored = state.store_archives();
        assert!(stored.documents.is_empty());
        assert_eq!(stored.orphans, vec![(key, evidence_archive())]);
    }

    /// A recorded document whose audio is all offline while the track plays
    /// other audio. It used to wait whole, with the live document (which may
    /// hold work on that other audio) saved beside it as a new orphan on
    /// every save. Now the live document is what saves: the first part of
    /// the restore holds no source, only the document data (seen to work
    /// with Melodyne 5.4.2, probe p7), and the archive is kept back once.
    #[test]
    fn recorded_audio_all_offline_beside_other_audio_keeps_the_archive_back_once() {
        let mut state = AraState::default();
        let key = evidence_key();
        let waiting = recorded_archive(&[("clip-1", SOURCE)]);
        state.load_archives(
            documents(vec![(key.clone(), waiting.clone())]),
            HashMap::new(),
        );
        let script = open_fake(&mut state, &key);
        script.borrow_mut().report = restore_report(AraRestoreOutcome::Matched, &[]);
        script.borrow_mut().stored_identity = host_record(&[("clip-9", "Assets/Audio/new.wav")]);
        let graph = track_graph(&[("clip-9", "Assets/Audio/new.wav")]);
        let offline = vec![(AraClipKey::from("clip-1"), AraSourceKey::from(SOURCE))];
        sync(&mut state, &key, &graph, &offline).unwrap();
        assert_eq!(
            script.borrow().builds,
            vec![vec![Asked {
                archive_id: ARCHIVE_ID.to_string(),
                data: waiting.data.clone(),
                mapped: false,
                part: Some((Vec::new(), true)),
            }]],
            "the document data now, and nothing else"
        );
        assert!(state.pending_archives.is_empty());

        for _ in 0..3 {
            let stored = state.store_archives();
            assert_eq!(stored.documents.len(), 1);
            assert_eq!(
                stored.documents[0].1.data,
                b"live".to_vec(),
                "the live document"
            );
            assert!(stored.orphans.is_empty(), "no copy beside it");
            assert_eq!(
                stored.deferred,
                vec![(
                    key.clone(),
                    DeferredRestore {
                        archive: waiting.clone(),
                        remaining: vec![SOURCE.to_string()],
                    }
                )]
            );
        }
    }

    /// Task rule: once a restore is judged lost, the kept document becomes an
    /// orphan at once and the live document saves from then on, so new work
    /// in the plug-in is saved and the old state is never lost.
    #[test]
    fn a_lost_restore_is_orphaned_at_once_and_the_live_document_saves() {
        let mut state = AraState::default();
        let key = evidence_key();
        state.load_archives(
            documents(vec![(key.clone(), evidence_archive())]),
            HashMap::new(),
        );
        let script = open_fake(&mut state, &key);
        // The evidence verdict: Melodyne refused, and re-analyses afresh.
        let mut missed = restore_report(AraRestoreOutcome::Missed, &[AraRestoreOutcome::Missed]);
        missed.error = Some(AraHostError::Plugin(
            "peer failure: plug-in rejected object restoration".to_string(),
        ));
        script.borrow_mut().report = missed;
        let graph = track_graph(&[("clip-1", "Assets/Audio/1.wav")]);
        sync(&mut state, &key, &graph, &[]).unwrap();

        let notices = state.take_notices();
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("could not be matched"), "{notices:?}");
        let stored = state.store_archives();
        assert_eq!(stored.documents.len(), 1);
        assert_eq!(stored.documents[0].1.data, b"live".to_vec());
        assert_eq!(stored.orphans, vec![(key.clone(), evidence_archive())]);
        // Every later save stores the live document again, and still one
        // copy of the orphan.
        script.borrow_mut().stored = Some(b"live, edited".to_vec());
        let stored = state.store_archives();
        assert_eq!(stored.documents[0].1.data, b"live, edited".to_vec());
        assert_eq!(stored.orphans.len(), 1);
    }

    /// The same document lost again (a later reopen in the same run, a
    /// retry) and one loaded twice from a file are kept once.
    #[test]
    fn orphans_are_kept_once_by_content() {
        let mut state = AraState::default();
        let key = evidence_key();
        let other = session_key("vst3:fd5c205bb907b3ca", "track-2");
        state.load_archives(
            AraSavedDocuments {
                documents: vec![(key.clone(), evidence_archive())],
                orphans: vec![
                    (key.clone(), evidence_archive()),
                    (key.clone(), evidence_archive()),
                    (key.clone(), saved(ARCHIVE_ID, b"other bytes")),
                    // The same bytes for another track are that track's.
                    (other.clone(), evidence_archive()),
                ],
                ..AraSavedDocuments::default()
            },
            HashMap::new(),
        );
        assert_eq!(state.orphans.len(), 3);

        let script = open_fake(&mut state, &key);
        script.borrow_mut().report =
            restore_report(AraRestoreOutcome::Missed, &[AraRestoreOutcome::Missed]);
        sync(
            &mut state,
            &key,
            &track_graph(&[("clip-1", "Assets/Audio/1.wav")]),
            &[],
        )
        .unwrap();
        assert_eq!(state.orphans.len(), 3, "already kept");
        assert_eq!(state.store_archives().orphans.len(), 3);
    }

    /// Finding (review): an unconfirmed restore saved the old archive in place
    /// of the live document until an edit in the editor was recognised, so
    /// new work in the plug-in was lost whenever it was not. Now a document
    /// with no record (the user's legacy project) restored unconfirmed saves
    /// its live document at once, with a record, and the archive is kept once
    /// beside it. Nothing is known to be missing, so the user is not told.
    /// Unbinding keeps the live document too.
    #[test]
    fn an_unconfirmed_legacy_restore_saves_the_live_document_and_keeps_the_archive_once() {
        let mut state = AraState::default();
        let key = evidence_key();
        state.load_archives(
            documents(vec![(key.clone(), evidence_archive())]),
            HashMap::new(),
        );
        let script = open_fake(&mut state, &key);
        script.borrow_mut().report = unconfirmed_report();
        script.borrow_mut().stored_identity = host_record(&[("clip-1", SOURCE)]);
        sync(&mut state, &key, &track_graph(&[("clip-1", SOURCE)]), &[]).unwrap();

        assert!(state.take_notices().is_empty());
        let stored = state.store_archives();
        assert_eq!(stored.documents.len(), 1);
        assert_eq!(
            stored.documents[0].1.data,
            b"live".to_vec(),
            "new work is never held back"
        );
        assert!(
            stored.documents[0].1.written_with.is_some(),
            "saved with a record from now on"
        );
        assert_eq!(stored.orphans, vec![(key.clone(), evidence_archive())]);
        assert_eq!(state.store_archives().orphans.len(), 1, "kept once");

        // Unbinding does not drop the live document of an unconfirmed session.
        script.borrow_mut().stored = Some(b"live, edited".to_vec());
        state.close(None, &key);
        assert!(script.borrow().torn_down);
        let stored = state.store_archives();
        assert!(stored.documents.is_empty());
        let kept: Vec<&[u8]> = stored
            .orphans
            .iter()
            .map(|(_, archive)| archive.data.as_slice())
            .collect();
        assert_eq!(kept, vec![&b"GNBKVAi\0 evidence"[..], &b"live, edited"[..]]);
        assert!(stored.orphans[1].1.written_with.is_some());
    }

    /// Task rule: an unconfirmed restore whose record places every object by
    /// its own ID where it was stored from is trusted. Its live document is
    /// the saved one from then on, with no copy beside it; until the plug-in
    /// first stores, the archive is the last good document. The same verdict
    /// for a record that is not all in place keeps the archive beside it.
    #[test]
    fn an_unconfirmed_restore_in_place_is_trusted() {
        let mut state = AraState::default();
        let key = evidence_key();
        let graph = track_graph(&[("clip-1", SOURCE)]);
        let archive = recorded_archive(&[("clip-1", SOURCE)]);
        state.load_archives(
            documents(vec![(key.clone(), archive.clone())]),
            HashMap::new(),
        );
        let script = open_fake(&mut state, &key);
        script.borrow_mut().report = unconfirmed_report();
        script.borrow_mut().stored = None;
        sync(&mut state, &key, &graph, &[]).unwrap();
        assert_eq!(
            script.borrow().restored(),
            vec![Some((ARCHIVE_ID.to_string(), false))]
        );
        assert!(state.take_notices().is_empty());
        assert!(state.orphans.is_empty(), "no copy of a trusted document");

        // The plug-in cannot store yet: the archive is written, not nothing.
        let stored = state.store_archives();
        assert_eq!(stored.documents, vec![(key.clone(), archive.clone())]);
        assert!(stored.orphans.is_empty());
        // Once it stores, its live document is the document.
        script.borrow_mut().stored = Some(b"live".to_vec());
        let stored = state.store_archives();
        assert_eq!(stored.documents[0].1.data, b"live".to_vec());
        assert!(stored.orphans.is_empty());
        // Unbinding keeps it: its restore was not confirmed.
        state.close(None, &key);
        assert_eq!(state.store_archives().orphans.len(), 1);

        // A record that holds a clip no longer on its source is not in
        // place: the same verdict keeps the archive beside the live document.
        let mut state = AraState::default();
        let moved = recorded_archive(&[("clip-1", SOURCE), ("clip-2", SOURCE)]);
        state.load_archives(
            documents(vec![(key.clone(), moved.clone())]),
            HashMap::new(),
        );
        let script = open_fake(&mut state, &key);
        script.borrow_mut().report = unconfirmed_report();
        sync(
            &mut state,
            &key,
            &track_graph(&[("clip-1", SOURCE), ("clip-2", OTHER)]),
            &[],
        )
        .unwrap();
        let stored = state.store_archives();
        assert_eq!(stored.documents[0].1.data, b"live".to_vec());
        assert_eq!(stored.orphans, vec![(key, moved)]);
    }

    /// Task rule: orphans must not grow per open and save. A legacy document
    /// restored unconfirmed is kept once; the next save gives the document a
    /// record, and every later open restores that in place (trusted) and adds
    /// nothing.
    #[test]
    fn an_unconfirmed_legacy_document_is_kept_once_over_repeated_open_and_save_cycles() {
        let key = evidence_key();
        let graph = track_graph(&[("clip-1", SOURCE)]);
        let mut saved = documents(vec![(key.clone(), evidence_archive())]);
        let mut state = AraState::default();
        for cycle in 0..4 {
            // Reopen: the last project's sessions go, the file's documents
            // are parked, and the session opens and restores.
            state.close_all(None);
            state.load_archives(saved.clone(), HashMap::new());
            let script = open_fake(&mut state, &key);
            let live = format!("live {cycle}").into_bytes();
            script.borrow_mut().report = unconfirmed_report();
            script.borrow_mut().stored = Some(live.clone());
            script.borrow_mut().stored_identity = host_record(&[("clip-1", SOURCE)]);
            sync(&mut state, &key, &graph, &[]).unwrap();
            assert!(state.take_notices().is_empty());
            for save in 0..2 {
                saved = state.store_archives();
                assert_eq!(saved.documents.len(), 1);
                assert_eq!(saved.documents[0].1.data, live, "cycle {cycle} save {save}");
                assert!(saved.documents[0].1.written_with.is_some());
                assert_eq!(
                    saved.orphans,
                    vec![(key.clone(), evidence_archive())],
                    "cycle {cycle} save {save}"
                );
            }
        }
    }

    /// A report for one restore that judged `sources`, each as
    /// (key, outcome, grade before, grade after), as the host builds it.
    fn judged(sources: &[(&str, AraRestoreOutcome, i32, i32)]) -> AraRestoreReport {
        let outcomes: Vec<AraRestoreOutcome> =
            sources.iter().map(|(_, outcome, _, _)| *outcome).collect();
        let all = |wanted| outcomes.iter().all(|outcome| *outcome == wanted);
        let outcome = if all(AraRestoreOutcome::Matched) {
            AraRestoreOutcome::Matched
        } else if all(AraRestoreOutcome::Missed) {
            AraRestoreOutcome::Missed
        } else {
            AraRestoreOutcome::Inconclusive
        };
        AraRestoreReport {
            error: None,
            outcome,
            sources: sources
                .iter()
                .map(
                    |(key, outcome, before, after)| sphere_ara_host::AraSourceRestore {
                        key: AraSourceKey::from(*key),
                        persistent_id: (*key).to_string(),
                        archived_id: Some((*key).to_string()),
                        outcome: *outcome,
                        grade_before: Some(*before),
                        grade_after: Some(*after),
                        analysis_incomplete: Some(false),
                    },
                )
                .collect(),
            unplaced_sources: Vec::new(),
            unplaced_modifications: Vec::new(),
            harmony_resynced: false,
        }
    }

    fn asked(archive: &SavedArchive, part: Option<(&[&str], bool)>) -> Asked {
        Asked {
            archive_id: archive.archive_id.clone(),
            data: archive.data.clone(),
            mapped: false,
            part: part.map(|(sources, document_data)| {
                (
                    sources.iter().map(|source| (*source).to_string()).collect(),
                    document_data,
                )
            }),
        }
    }

    /// Finding (review, verified with Melodyne): a document waiting whole for
    /// part of its audio overwrote, when that audio came back in the same
    /// session, the work done meanwhile on the audio that was there, and
    /// saved a new copy of the live document beside it on every save. Now
    /// the audio that is there is restored at once (with the document data)
    /// and the live document is what saves from then on; the archive is kept
    /// back, once, for the offline part, and that part alone (no document
    /// data) is restored into the live document when its audio comes back.
    /// The verdicts are the ones Melodyne 5.4.2 gave for exactly this
    /// (probe p1/p2: A Matched 0->1, then B Matched 0->1, A untouched).
    #[test]
    fn part_of_the_audio_offline_restores_the_rest_now_and_that_part_when_it_returns() {
        use AraRestoreOutcome::Matched;
        for recorded in [false, true] {
            let mut state = AraState::default();
            let key = evidence_key();
            let archive = if recorded {
                recorded_archive(&[("clip-1", SOURCE), ("clip-2", OTHER)])
            } else {
                evidence_archive()
            };
            state.load_archives(
                documents(vec![(key.clone(), archive.clone())]),
                HashMap::new(),
            );
            let script = open_fake(&mut state, &key);
            script.borrow_mut().report = judged(&[(SOURCE, Matched, 0, 1)]);
            script.borrow_mut().stored_identity = host_record(&[("clip-1", SOURCE)]);
            let offline = vec![(AraClipKey::from("clip-2"), AraSourceKey::from(OTHER))];
            sync(
                &mut state,
                &key,
                &track_graph(&[("clip-1", SOURCE)]),
                &offline,
            )
            .unwrap();

            // A recorded document goes through a filter naming what is
            // there; a legacy one whole, since what is offline is not in the
            // graph to take anything.
            let first = if recorded {
                asked(&archive, Some((&[SOURCE], true)))
            } else {
                asked(&archive, None)
            };
            assert_eq!(script.borrow().builds, vec![vec![first]]);
            assert!(state.pending_archives.is_empty());
            let notices = state.take_notices();
            assert_eq!(notices.len(), 1, "{notices:?}");
            assert!(
                notices[0].contains("some of its audio is offline"),
                "{notices:?}"
            );
            assert_eq!(
                notices[0].contains("will be restored"),
                recorded,
                "only a recorded document is promised a restore: {notices:?}"
            );
            let held = DeferredRestore {
                archive: archive.clone(),
                remaining: vec![OTHER.to_string()],
            };
            let stored = state.store_archives();
            assert_eq!(stored.documents.len(), 1);
            assert_eq!(
                stored.documents[0].1.data,
                b"live".to_vec(),
                "the live document"
            );
            assert!(
                stored.orphans.is_empty(),
                "no copy beside it: {:?}",
                stored.orphans
            );
            assert_eq!(stored.deferred, vec![(key.clone(), held.clone())]);

            // Work on the audio that is there is saved as the document; the
            // user edits, and a later save stores that.
            script.borrow_mut().stored = Some(b"live, edited while B was away".to_vec());
            let stored = state.store_archives();
            assert_eq!(
                stored.documents[0].1.data,
                b"live, edited while B was away".to_vec()
            );
            assert_eq!(stored.deferred, vec![(key.clone(), held.clone())]);

            // The media is found: only its part, without the document data,
            // into the live document. Nothing else is asked for, so what was
            // done to the rest meanwhile is not touched.
            script.borrow_mut().report = judged(&[(OTHER, Matched, 0, 1)]);
            let whole = track_graph(&[("clip-1", SOURCE), ("clip-2", OTHER)]);
            sync(&mut state, &key, &whole, &[]).unwrap();
            assert_eq!(
                script.borrow().builds[1],
                vec![asked(&archive, Some((&[OTHER], false)))]
            );
            assert!(state.take_notices().is_empty());
            assert!(state.orphans.is_empty());
            script.borrow_mut().stored = Some(b"live, whole".to_vec());
            let stored = state.store_archives();
            assert_eq!(stored.documents[0].1.data, b"live, whole".to_vec());
            assert!(
                stored.deferred.is_empty(),
                "restored: the record goes with the save"
            );
            assert!(stored.orphans.is_empty());
            // The next sync asks for nothing.
            script.borrow_mut().change = AraGraphChange::Unchanged;
            sync(&mut state, &key, &whole, &[]).unwrap();
            assert_eq!(script.borrow().builds.len(), 2);
        }
    }

    /// Task rule 3 (the review's measured growth): orphans stay bounded over
    /// reopen and save cycles while part of the audio stays offline, with a
    /// live document whose bytes differ on every store (Melodyne's do after
    /// a fresh analysis). Nothing is added per cycle: the live document is
    /// the document, and the kept-back archive is the same bytes each time.
    /// When the audio comes back at a reopen, both restores run in the one
    /// build: the kept-back part first, the saved live document (which
    /// brings the document data) last, as ARA asks and as Melodyne 5.4.2
    /// took it (probe p6: both Matched, the result byte-identical to the
    /// in-session path).
    #[test]
    fn reopening_and_saving_while_part_of_the_audio_stays_offline_adds_nothing() {
        use AraRestoreOutcome::{Matched, Missed};
        for recorded in [false, true] {
            let key = evidence_key();
            let archive = if recorded {
                recorded_archive(&[("clip-1", SOURCE), ("clip-2", OTHER)])
            } else {
                evidence_archive()
            };
            let offline = vec![(AraClipKey::from("clip-2"), AraSourceKey::from(OTHER))];
            let partial = track_graph(&[("clip-1", SOURCE)]);
            let mut saved = documents(vec![(key.clone(), archive.clone())]);
            let mut state = AraState::default();
            // The legacy evidence archive is refused by Melodyne (as in every
            // real run), which keeps it as an orphan once.
            let orphans_expected = usize::from(!recorded);
            for cycle in 0..4 {
                state.close_all(None);
                state.load_archives(saved.clone(), HashMap::new());
                let script = open_fake(&mut state, &key);
                let first = if cycle == 0 && !recorded {
                    let mut refused = judged(&[(SOURCE, Missed, 0, 0)]);
                    refused.error = Some(AraHostError::Plugin(
                        "peer failure: plug-in rejected object restoration".to_string(),
                    ));
                    refused
                } else {
                    judged(&[(SOURCE, Matched, 0, 1)])
                };
                script.borrow_mut().report = first;
                script.borrow_mut().stored = Some(format!("live {cycle}").into_bytes());
                script.borrow_mut().stored_identity = host_record(&[("clip-1", SOURCE)]);
                sync(&mut state, &key, &partial, &offline).unwrap();
                for save in 0..2 {
                    saved = state.store_archives();
                    let at = format!("recorded {recorded} cycle {cycle} save {save}");
                    assert_eq!(saved.documents.len(), 1, "{at}");
                    assert_eq!(
                        saved.documents[0].1.data,
                        format!("live {cycle}").into_bytes(),
                        "{at}"
                    );
                    assert_eq!(saved.orphans.len(), orphans_expected, "{at}");
                    assert_eq!(saved.deferred.len(), 1, "{at}");
                    assert_eq!(saved.deferred[0].1.archive.data, archive.data, "{at}");
                    assert_eq!(
                        saved.deferred[0].1.remaining,
                        vec![OTHER.to_string()],
                        "{at}"
                    );
                }
            }

            // Reopened with the media back.
            state.close_all(None);
            state.load_archives(saved.clone(), HashMap::new());
            let script = open_fake(&mut state, &key);
            let back = if recorded {
                judged(&[(OTHER, Matched, 0, 1)])
            } else {
                // The legacy evidence archive holds no state under that ID.
                let mut refused = judged(&[(OTHER, Missed, 0, 0)]);
                refused.error = Some(AraHostError::Plugin(
                    "peer failure: plug-in rejected object restoration".to_string(),
                ));
                refused
            };
            script.borrow_mut().reports = vec![back, judged(&[(SOURCE, Matched, 0, 1)])].into();
            let whole = track_graph(&[("clip-1", SOURCE), ("clip-2", OTHER)]);
            sync(&mut state, &key, &whole, &[]).unwrap();
            let live = saved.documents[0].1.clone();
            assert_eq!(
                script.borrow().builds,
                vec![vec![
                    asked(&archive, Some((&[OTHER], false))),
                    asked(&live, None),
                ]],
                "one build: the kept-back part first, the document data last"
            );
            assert!(
                state.take_notices().is_empty(),
                "a miss of an archive already kept is not said twice"
            );
            let saved = state.store_archives();
            assert!(saved.deferred.is_empty());
            assert_eq!(saved.orphans.len(), orphans_expected);
        }
    }

    /// Task rule 5: a kept-back part that comes back into a source Melodyne
    /// had analysed before its restore (a build that brought the audio back
    /// and failed afterwards, say) is reported as Melodyne 5.4.2 reports it:
    /// Inconclusive, grade 1->1 (probe p8; the restore replaces the analysed
    /// state). A recorded part in place is trusted: restored, no copy, and
    /// the live document kept on unbind. A legacy one is kept once beside
    /// the live document. Either way the part is done with.
    #[test]
    fn a_part_restored_into_audio_already_analysed_is_judged_as_melodyne_reports_it() {
        use AraRestoreOutcome::{Inconclusive, Matched};
        for recorded in [false, true] {
            let mut state = AraState::default();
            let key = evidence_key();
            let archive = if recorded {
                recorded_archive(&[("clip-1", SOURCE), ("clip-2", OTHER)])
            } else {
                evidence_archive()
            };
            state.load_archives(
                AraSavedDocuments {
                    documents: vec![(key.clone(), saved(ARCHIVE_ID, b"live before"))],
                    deferred: vec![(
                        key.clone(),
                        DeferredRestore {
                            archive: archive.clone(),
                            remaining: vec![OTHER.to_string()],
                        },
                    )],
                    ..AraSavedDocuments::default()
                },
                HashMap::new(),
            );
            let script = open_fake(&mut state, &key);
            script.borrow_mut().reports = vec![
                judged(&[(OTHER, Inconclusive, 1, 1)]),
                judged(&[(SOURCE, Matched, 0, 1)]),
            ]
            .into();
            let whole = track_graph(&[("clip-1", SOURCE), ("clip-2", OTHER)]);
            sync(&mut state, &key, &whole, &[]).unwrap();
            assert_eq!(script.borrow().builds[0].len(), 2);
            assert!(
                state.take_notices().is_empty(),
                "nothing is known to be missing"
            );
            let stored = state.store_archives();
            assert!(stored.deferred.is_empty(), "the part is done with");
            if recorded {
                assert!(stored.orphans.is_empty(), "trusted in place");
            } else {
                assert_eq!(stored.orphans, vec![(key.clone(), archive.clone())]);
                assert_eq!(state.store_archives().orphans.len(), 1, "kept once");
            }
            assert!(
                state.sessions[&key].unconfirmed,
                "its live document outlives an unbind"
            );
        }
    }

    /// One kept-back archive per session: a newer document restored in part
    /// replaces it, and what the old one still held back is kept as an
    /// orphan, once, rather than dropped or piled up.
    #[test]
    fn a_newer_partial_restore_replaces_the_kept_back_archive_and_keeps_it_once() {
        const THIRD: &str = "Assets/Audio/3.wav";
        let key = evidence_key();
        let old = recorded_archive(&[("clip-1", SOURCE), ("clip-2", OTHER)]);
        let mut newer = recorded_archive(&[("clip-1", SOURCE), ("clip-3", THIRD)]);
        newer.data = b"GNBKVAi\0 newer".to_vec();
        for _ in 0..2 {
            let mut state = AraState::default();
            state.load_archives(
                AraSavedDocuments {
                    documents: vec![(key.clone(), newer.clone())],
                    orphans: vec![(key.clone(), old.clone())],
                    deferred: vec![(
                        key.clone(),
                        DeferredRestore {
                            archive: old.clone(),
                            remaining: vec![OTHER.to_string()],
                        },
                    )],
                },
                HashMap::new(),
            );
            let script = open_fake(&mut state, &key);
            script.borrow_mut().report = judged(&[(SOURCE, AraRestoreOutcome::Matched, 0, 1)]);
            let offline = vec![
                (AraClipKey::from("clip-2"), AraSourceKey::from(OTHER)),
                (AraClipKey::from("clip-3"), AraSourceKey::from(THIRD)),
            ];
            sync(
                &mut state,
                &key,
                &track_graph(&[("clip-1", SOURCE)]),
                &offline,
            )
            .unwrap();
            let stored = state.store_archives();
            assert_eq!(
                stored.deferred,
                vec![(
                    key.clone(),
                    DeferredRestore {
                        archive: newer.clone(),
                        remaining: vec![THIRD.to_string()],
                    }
                )]
            );
            assert_eq!(
                stored.orphans,
                vec![(key.clone(), old.clone())],
                "the old one kept, once (it was already an orphan here)"
            );
        }
    }

    /// Unbinding the plug-in while part of its archive is kept back keeps
    /// that archive, as an orphan: the part never reached a live document.
    #[test]
    fn a_kept_back_archive_survives_the_unbind() {
        let mut state = AraState::default();
        let key = evidence_key();
        let archive = recorded_archive(&[("clip-1", SOURCE), ("clip-2", OTHER)]);
        state.load_archives(
            documents(vec![(key.clone(), archive.clone())]),
            HashMap::new(),
        );
        let script = open_fake(&mut state, &key);
        let offline = vec![(AraClipKey::from("clip-2"), AraSourceKey::from(OTHER))];
        sync(
            &mut state,
            &key,
            &track_graph(&[("clip-1", SOURCE)]),
            &offline,
        )
        .unwrap();
        state.close(None, &key);
        assert!(script.borrow().torn_down);
        let stored = state.store_archives();
        assert!(stored.documents.is_empty() && stored.deferred.is_empty());
        assert_eq!(stored.orphans, vec![(key, archive)]);
    }

    /// A part restored in full, and then a plug-in that cannot store: the
    /// live document holding it could not be saved, so the archive is kept
    /// as an orphan rather than dropped with the record.
    #[test]
    fn a_restored_part_is_kept_when_the_live_document_cannot_be_stored() {
        let mut state = AraState::default();
        let key = evidence_key();
        let archive = recorded_archive(&[("clip-1", SOURCE), ("clip-2", OTHER)]);
        state.load_archives(
            AraSavedDocuments {
                documents: vec![(key.clone(), saved(ARCHIVE_ID, b"live before"))],
                deferred: vec![(
                    key.clone(),
                    DeferredRestore {
                        archive: archive.clone(),
                        remaining: vec![OTHER.to_string()],
                    },
                )],
                ..AraSavedDocuments::default()
            },
            HashMap::new(),
        );
        let script = open_fake(&mut state, &key);
        script.borrow_mut().reports = vec![
            judged(&[(OTHER, AraRestoreOutcome::Matched, 0, 1)]),
            judged(&[(SOURCE, AraRestoreOutcome::Matched, 0, 1)]),
        ]
        .into();
        script.borrow_mut().stored = None;
        let whole = track_graph(&[("clip-1", SOURCE), ("clip-2", OTHER)]);
        sync(&mut state, &key, &whole, &[]).unwrap();
        let stored = state.store_archives();
        assert!(stored.deferred.is_empty());
        assert_eq!(stored.orphans, vec![(key, archive)]);
        assert!(state.last_error.is_some());
    }

    /// Kept-back archives of sessions that never opened are saved back as
    /// they were loaded; two for one session keep the second as an orphan,
    /// and one with nothing left to restore is kept, not dropped.
    #[test]
    fn kept_back_archives_load_one_per_session_and_save_back_untouched() {
        let mut state = AraState::default();
        let key = evidence_key();
        let other = session_key("vst3:fd5c205bb907b3ca", "track-2");
        let held = |data: &[u8], remaining: &[&str]| DeferredRestore {
            archive: saved(ARCHIVE_ID, data),
            remaining: remaining.iter().map(|id| (*id).to_string()).collect(),
        };
        state.load_archives(
            AraSavedDocuments {
                deferred: vec![
                    (key.clone(), held(b"first", &[OTHER])),
                    (key.clone(), held(b"second", &[OTHER])),
                    (other.clone(), held(b"done", &[])),
                ],
                ..AraSavedDocuments::default()
            },
            HashMap::new(),
        );
        let stored = state.store_archives();
        assert_eq!(
            stored.deferred,
            vec![(key.clone(), held(b"second", &[OTHER]))]
        );
        let mut orphans = stored.orphans.clone();
        orphans.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            orphans,
            vec![
                (key.clone(), saved(ARCHIVE_ID, b"first")),
                (other, saved(ARCHIVE_ID, b"done")),
            ]
        );
        // Saving again changes nothing.
        let again = state.store_archives();
        assert_eq!(again.deferred, stored.deferred);
        assert_eq!(again.orphans.len(), 2);
    }

    /// Finding (review): after a lost restore nothing was the last good
    /// document, so a plug-in that then failed to store saved no document for
    /// the session at all. The archive is the last good one until the first
    /// store.
    #[test]
    fn a_lost_archive_is_written_when_the_plugin_cannot_store() {
        let mut state = AraState::default();
        let key = evidence_key();
        state.load_archives(
            documents(vec![(key.clone(), evidence_archive())]),
            HashMap::new(),
        );
        let script = open_fake(&mut state, &key);
        script.borrow_mut().report =
            restore_report(AraRestoreOutcome::Missed, &[AraRestoreOutcome::Missed]);
        script.borrow_mut().stored = None;
        sync(&mut state, &key, &track_graph(&[("clip-1", SOURCE)]), &[]).unwrap();

        let stored = state.store_archives();
        assert_eq!(stored.documents, vec![(key.clone(), evidence_archive())]);
        assert_eq!(stored.orphans, vec![(key, evidence_archive())]);
        assert!(state.last_error.is_some());
    }

    #[test]
    fn parked_archives_survive_a_save_when_their_plugin_never_opened() {
        // A project can be saved without its ARA plug-ins ever being
        // instantiated (missing plug-in, engine not started). Those archives
        // must be written back untouched instead of being dropped.
        let mut state = AraState::default();
        let key = session_key("vst3:melodyne", "track-1");
        let mut archive = saved("com.celemony.ara.v5", &[1, 2, 3]);
        archive.written_with = Some(ProjectAraIdentity::default());
        state.load_archives(
            documents(vec![(key.clone(), archive.clone())]),
            HashMap::new(),
        );

        let stored = state.store_archives();
        assert_eq!(stored.documents, vec![(key, archive)]);
        assert!(stored.orphans.is_empty());
    }

    #[test]
    fn loading_archives_replaces_the_previous_set() {
        let mut state = AraState::default();
        let first = session_key("a", "t");
        let second = session_key("b", "t");
        state.load_archives(
            AraSavedDocuments {
                documents: vec![(first.clone(), saved("id-a", &[0]))],
                orphans: vec![(first.clone(), saved("id-a", &[9]))],
                deferred: vec![(
                    first,
                    DeferredRestore {
                        archive: saved("id-a", &[8]),
                        remaining: vec!["b.wav".to_string()],
                    },
                )],
            },
            HashMap::new(),
        );
        state.load_archives(
            documents(vec![(second.clone(), saved("id-b", &[1]))]),
            HashMap::new(),
        );

        let stored = state.store_archives();
        assert_eq!(
            stored.documents.len(),
            1,
            "opening a new project must not keep the old archives"
        );
        assert_eq!(stored.documents[0].0, second);
        assert!(stored.orphans.is_empty(), "nor the old orphans");
        assert!(stored.deferred.is_empty(), "nor what the old one kept back");
    }

    /// Orphans are saved back byte for byte, identity included.
    #[test]
    fn orphans_are_saved_back_as_they_were_loaded() {
        let mut state = AraState::default();
        let key = session_key("vst3:melodyne", "track-1");
        let mut orphan = saved("com.celemony.ara.chunk.13", &[7, 7, 7]);
        orphan.written_with = Some(ProjectAraIdentity {
            key_descriptor: Some(3),
            ..ProjectAraIdentity::default()
        });
        state.load_archives(
            AraSavedDocuments {
                documents: Vec::new(),
                orphans: vec![(key.clone(), orphan.clone())],
                ..AraSavedDocuments::default()
            },
            HashMap::new(),
        );
        assert_eq!(state.store_archives().orphans, vec![(key, orphan)]);
    }

    /// Two documents for one session cannot both be restored; neither is
    /// dropped.
    #[test]
    fn a_second_document_for_one_session_is_kept_as_an_orphan() {
        let mut state = AraState::default();
        let key = session_key("vst3:melodyne", "track-1");
        state.load_archives(
            documents(vec![
                (key.clone(), saved("id", &[1])),
                (key.clone(), saved("id", &[2])),
            ]),
            HashMap::new(),
        );
        let stored = state.store_archives();
        assert_eq!(stored.documents, vec![(key.clone(), saved("id", &[2]))]);
        assert_eq!(stored.orphans, vec![(key, saved("id", &[1]))]);
    }

    /// Closing every session forgets the project's documents, parked and
    /// orphaned alike, and says the documents held were replaced.
    #[test]
    fn closing_everything_forgets_the_projects_documents() {
        let mut state = AraState::default();
        let key = session_key("vst3:melodyne", "track-1");
        state.load_archives(
            AraSavedDocuments {
                documents: vec![(key.clone(), saved("id", &[1]))],
                orphans: vec![(key.clone(), saved("id", &[2]))],
                deferred: vec![(
                    key,
                    DeferredRestore {
                        archive: saved("id", &[3]),
                        remaining: vec!["b.wav".to_string()],
                    },
                )],
            },
            HashMap::from([("a".to_string(), "1-00000001".to_string())]),
        );
        let before = state.generation();
        state.close_all(None);
        assert!(state.generation() > before);
        let stored = state.store_archives();
        assert!(stored.documents.is_empty() && stored.orphans.is_empty());
        assert!(stored.deferred.is_empty());
        assert!(state.asset_fingerprints.is_empty());
    }

    /// Finding (review): a project closed or replaced under an open editor
    /// had its documents destroyed at once. Retiring takes every session out
    /// of the project at once, so the next one can open its own under the
    /// same keys, but destroys a document only when no view holds it any
    /// more, or once the wait has run out.
    #[test]
    fn a_retired_document_is_destroyed_only_once_its_editor_lets_go() {
        let mut state = AraState::default();
        let viewed = evidence_key();
        let hidden = session_key("vst3:fd5c205bb907b3ca", "track-2");
        let viewed_script = open_fake(&mut state, &viewed);
        let hidden_script = open_fake(&mut state, &hidden);
        viewed_script.borrow_mut().view_attached = true;
        let start = Instant::now();
        let patience = Duration::from_secs(2);

        state.retire_all(None, start);
        assert!(!state.is_active(), "out of the project at once");
        assert!(state.processor(&viewed).is_none());
        // The next project opens a session under the same key right away.
        let next = open_fake(&mut state, &viewed);

        assert!(state.finish_retired(start, patience, |_| true));
        assert!(hidden_script.borrow().torn_down, "no view: destroyed");
        assert!(!viewed_script.borrow().torn_down, "its view is still up");
        viewed_script.borrow_mut().view_attached = false;
        assert!(!state.finish_retired(start + Duration::from_millis(16), patience, |_| true));
        assert!(viewed_script.borrow().torn_down);
        assert!(!next.borrow().torn_down, "the new session is not touched");
        assert!(state.is_active());

        // A view that never lets go does not keep its document forever.
        let stuck = open_fake(&mut state, &hidden);
        stuck.borrow_mut().view_attached = true;
        state.retire_all(None, start);
        assert!(state.finish_retired(start + Duration::from_secs(1), patience, |_| true));
        assert!(!stuck.borrow().torn_down);
        assert!(!state.finish_retired(start + patience, patience, |_| true));
        assert!(stuck.borrow().torn_down);
    }

    /// Finding (review): the docked panel's half of that wait (`released`,
    /// asked by instance handle) was never exercised. A retired document the
    /// docked panel still holds a view on is not destroyed, even with the
    /// project reopened under the same key; once the panel lets go of that
    /// instance, it is.
    #[test]
    fn a_retired_document_waits_for_the_docked_editor_holding_its_instance() {
        const RETIRED: usize = 0x9_8acd_8000;
        const NEXT: usize = 0x9_8ace_0000;
        let mut state = AraState::default();
        let key = evidence_key();
        let retired = open_fake(&mut state, &key);
        retired.borrow_mut().handle = Some(RETIRED);
        let start = Instant::now();
        let patience = Duration::from_secs(2);

        state.retire_all(None, start);
        // The project reopened in place: a new instance under the same key.
        let next = open_fake(&mut state, &key);
        next.borrow_mut().handle = Some(NEXT);
        assert_eq!(state.instance_handle(&key), Some(NEXT));

        // The docked panel's answer: it holds a view on one instance.
        let docked = std::cell::Cell::new(Some(RETIRED));
        let released = |handle: usize| docked.get() != Some(handle);
        assert!(state.finish_retired(start, patience, released));
        assert!(
            !retired.borrow().torn_down,
            "the docked panel still holds its view"
        );
        // The panel re-targets to the new instance: the retired one is free,
        // and the new one is not touched.
        docked.set(Some(NEXT));
        assert!(!state.finish_retired(start + Duration::from_millis(16), patience, released));
        assert!(retired.borrow().torn_down);
        assert!(!next.borrow().torn_down);

        // A panel that never lets go does not keep the document forever.
        let stuck = open_fake(&mut state, &session_key("vst3:fd5c205bb907b3ca", "track-2"));
        stuck.borrow_mut().handle = Some(RETIRED);
        state.retire_all(None, start);
        docked.set(Some(RETIRED));
        assert!(state.finish_retired(start + Duration::from_secs(1), patience, released));
        assert!(!stuck.borrow().torn_down);
        assert!(!state.finish_retired(start + patience, patience, released));
        assert!(stuck.borrow().torn_down);
    }

    /// A switch that fails puts the previous project's documents back only
    /// when something replaced them; otherwise its live sessions stay. The
    /// live session's document is what was stored when the switch began.
    #[test]
    fn a_rollback_puts_back_only_documents_that_were_replaced() {
        let mut state = AraState::default();
        let key = evidence_key();
        let parked_key = session_key("vst3:fd5c205bb907b3ca", "track-2");
        state.load_archives(
            documents(vec![(parked_key.clone(), saved("parked", &[1]))]),
            HashMap::from([("Assets/Audio/1.wav".to_string(), "1-00000001".to_string())]),
        );
        let live = open_fake(&mut state, &key);
        live.borrow_mut().stored = Some(b"live at the switch".to_vec());

        let parked = state.park_for_rollback();
        assert!(state.holds(&parked), "nothing replaced them yet");
        assert!(!state.roll_back(parked.clone(), None, Instant::now()));
        assert!(
            state.is_active(),
            "a switch that failed early keeps them live"
        );
        assert!(!live.borrow().torn_down);

        // The switch replaces the project: its sessions retire, the other
        // project's documents are parked and one of its sessions opens.
        let now = Instant::now();
        state.retire_all(None, now);
        state.finish_retired(now, Duration::ZERO, |_| true);
        assert!(live.borrow().torn_down);
        let other_key = session_key("vst3:other", "track-1");
        state.load_archives(
            documents(vec![(other_key.clone(), saved("new", &[9]))]),
            HashMap::new(),
        );
        let other = open_fake(&mut state, &other_key);
        assert!(!state.holds(&parked));

        // It fails: the real rollback puts the first project's back.
        assert!(state.roll_back(parked, None, now));
        assert!(!state.is_active());
        state.finish_retired(now, Duration::ZERO, |_| true);
        assert!(other.borrow().torn_down);
        let mut restored = state.store_archives().documents;
        restored.sort_by(|a, b| a.0.cmp(&b.0));
        let mut expected = vec![
            (key, saved(ARCHIVE_ID, b"live at the switch")),
            (parked_key, saved("parked", &[1])),
        ];
        expected.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(restored.len(), 2);
        for ((got_key, got), (want_key, want)) in restored.iter().zip(&expected) {
            assert_eq!(got_key, want_key);
            assert_eq!(got.archive_id, want.archive_id);
            assert_eq!(got.data, want.data);
            assert!(!got.stored_now, "put back verbatim");
        }
        assert_eq!(
            state
                .asset_fingerprints
                .get("Assets/Audio/1.wav")
                .map(String::as_str),
            Some("1-00000001")
        );
    }

    /// Finding (review): a snapshot was judged by generation alone, so one
    /// taken from a state that never loaded or retired a project (generation
    /// 0) counted as held by a workspace mounted afresh for the rollback
    /// (generation 0 as well), and its documents were dropped instead of put
    /// back. A snapshot is only ever held by the state it came from.
    #[test]
    fn a_rollback_snapshot_is_only_ever_held_by_the_state_it_came_from() {
        let key = evidence_key();
        let mut old = AraState::default();
        let live = open_fake(&mut old, &key);
        live.borrow_mut().stored = Some(b"live at the switch".to_vec());
        let parked = old.park_for_rollback();
        assert_eq!(old.generation(), 0);
        assert!(old.holds(&parked));

        let mut fresh = AraState::default();
        assert_eq!(fresh.generation(), old.generation());
        assert!(
            !fresh.holds(&parked),
            "another state, whatever its generation"
        );
        assert!(fresh.roll_back(parked, None, Instant::now()));
        let restored = fresh.store_archives().documents;
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].0, key);
        assert_eq!(restored[0].1.data, b"live at the switch".to_vec());
    }

    /// A kept-back part whose audio came back and did not land (a miss, a
    /// refusal) is done with too: the archive is kept once beside the live
    /// document, and the user is told once. A legacy archive that simply
    /// held nothing for that audio (seen with Melodyne 5.4.2 on the repaired
    /// project: Missed, grade 0->0, no error) is kept without a warning.
    #[test]
    fn a_part_that_does_not_land_is_kept_once_and_said_once() {
        for recorded in [true, false] {
            a_part_that_does_not_land(recorded);
        }
    }

    fn a_part_that_does_not_land(recorded: bool) {
        let mut state = AraState::default();
        let key = evidence_key();
        let archive = if recorded {
            recorded_archive(&[("clip-1", SOURCE), ("clip-2", OTHER)])
        } else {
            evidence_archive()
        };
        state.load_archives(
            AraSavedDocuments {
                documents: vec![(key.clone(), saved(ARCHIVE_ID, b"live before"))],
                deferred: vec![(
                    key.clone(),
                    DeferredRestore {
                        archive: archive.clone(),
                        remaining: vec![OTHER.to_string()],
                    },
                )],
                ..AraSavedDocuments::default()
            },
            HashMap::new(),
        );
        let script = open_fake(&mut state, &key);
        let mut missed = restore_report(AraRestoreOutcome::Missed, &[AraRestoreOutcome::Missed]);
        missed.sources[0].grade_after = Some(0);
        script.borrow_mut().reports = vec![missed].into();
        let whole = track_graph(&[("clip-1", SOURCE), ("clip-2", OTHER)]);
        sync(&mut state, &key, &whole, &[]).unwrap();
        let notices = state.take_notices();
        if recorded {
            assert_eq!(notices.len(), 1, "{notices:?}");
            assert!(notices[0].contains("could not be matched"), "{notices:?}");
        } else {
            assert!(notices.is_empty(), "{notices:?}");
        }
        let stored = state.store_archives();
        assert!(stored.deferred.is_empty());
        assert_eq!(stored.orphans, vec![(key.clone(), archive)]);
        assert_eq!(state.store_archives().orphans.len(), 1);
    }

    fn restore_report(
        outcome: AraRestoreOutcome,
        sources: &[AraRestoreOutcome],
    ) -> AraRestoreReport {
        AraRestoreReport {
            error: None,
            outcome,
            sources: sources
                .iter()
                .enumerate()
                .map(|(index, outcome)| sphere_ara_host::AraSourceRestore {
                    key: AraSourceKey(format!("source-{index}")),
                    persistent_id: format!("source-{index}"),
                    archived_id: None,
                    outcome: *outcome,
                    grade_before: Some(0),
                    grade_after: Some(1),
                    analysis_incomplete: Some(false),
                })
                .collect(),
            unplaced_sources: Vec::new(),
            unplaced_modifications: Vec::new(),
            harmony_resynced: false,
        }
    }

    #[test]
    fn only_a_matched_or_in_place_restore_stands_for_the_archive() {
        use AraRestoreOutcome::{Inconclusive, Matched, Missed};
        let matched = restore_report(Matched, &[Matched]);
        assert_eq!(archive_fate(Some(&matched), false), ArchiveFate::Restored);
        assert_eq!(archive_fate(Some(&matched), true), ArchiveFate::Restored);

        // The evidence case: nothing matched and the plug-in re-analyses.
        let missed = restore_report(Missed, &[Missed]);
        assert_eq!(archive_fate(Some(&missed), false), ArchiveFate::Lost);
        // One source matched and one missed is no match.
        let mixed = restore_report(Inconclusive, &[Matched, Missed]);
        assert_eq!(archive_fate(Some(&mixed), false), ArchiveFate::Lost);
        // Everything placed matched, but some archived state had no place.
        let mut unplaced = restore_report(Inconclusive, &[Matched]);
        unplaced.unplaced_modifications = vec!["clip-2".to_string()];
        assert_eq!(archive_fate(Some(&unplaced), false), ArchiveFate::Lost);
        // A matched verdict with a refusal is not a restore.
        let mut refused = restore_report(Matched, &[Matched]);
        refused.error = Some(AraHostError::Plugin("refused".to_string()));
        assert_eq!(archive_fate(Some(&refused), false), ArchiveFate::Lost);
        assert_eq!(archive_fate(Some(&refused), true), ArchiveFate::Lost);
        // No evidence either way: trusted when every recorded object is in
        // place, kept beside the live document otherwise.
        let unsure = restore_report(Inconclusive, &[Inconclusive]);
        assert_eq!(archive_fate(Some(&unsure), true), ArchiveFate::Trusted);
        assert_eq!(archive_fate(Some(&unsure), false), ArchiveFate::Unconfirmed);
        // Not asked at all: nothing had a place.
        assert_eq!(archive_fate(None, false), ArchiveFate::Lost);
        assert_eq!(archive_fate(None, true), ArchiveFate::Lost);
        // Anything missing is lost, in place or not.
        assert_eq!(archive_fate(Some(&missed), true), ArchiveFate::Lost);
    }

    /// Finding (review): a retry after a build that failed part way reads a
    /// grade the plug-in already analysed to, so its verdict is unconfirmed,
    /// not lost: no notice, whatever happened.
    #[test]
    fn a_retry_into_analysed_sources_is_unconfirmed_not_lost() {
        let mut retry = restore_report(
            AraRestoreOutcome::Inconclusive,
            &[AraRestoreOutcome::Inconclusive],
        );
        retry.sources[0].grade_before = Some(1);
        retry.sources[0].grade_after = Some(1);
        assert_eq!(archive_fate(Some(&retry), false), ArchiveFate::Unconfirmed);
        assert_eq!(archive_fate(Some(&retry), true), ArchiveFate::Trusted);
    }

    #[test]
    fn what_a_live_session_saves() {
        let pending = saved("id", &[1]);
        let stored = SavedArchive {
            stored_now: true,
            ..saved("id", &[3])
        };
        let failing = || -> AraResult<SavedArchive> { Err(AraHostError::Poisoned) };
        let never = || -> AraResult<SavedArchive> { panic!("must not store over a kept archive") };

        // An archive waiting for its restore: as it is.
        let mut last_good = None;
        assert_eq!(
            document_to_save(Some(&pending), never, &mut last_good),
            (Some(pending.clone()), None)
        );
        assert_eq!(last_good, None);

        // The live document, which becomes the last good one.
        let fresh = stored.clone();
        assert_eq!(
            document_to_save(None, move || Ok(fresh), &mut last_good),
            (Some(stored.clone()), None)
        );
        assert_eq!(last_good, Some(saved("id", &[3])), "kept verbatim");

        // A plug-in that cannot store writes the last good document, not
        // none, and not as a store of this save.
        let (written, error) = document_to_save(None, failing, &mut last_good);
        assert_eq!(written, Some(saved("id", &[3])));
        assert!(error.is_some());
        let mut nothing = None;
        let (written, error) = document_to_save(None, failing, &mut nothing);
        assert_eq!(written, None);
        assert!(error.is_some());
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

        /// One periodic poll at `millis` that delivers `reports`: whether it
        /// found unsaved work.
        fn poll(&mut self, millis: u64, view: bool, reports: &[AraModelUpdate]) -> bool {
            for update in reports {
                self.bridge.notify(update.clone());
            }
            let report = self.bridge.signals.report();
            self.watch.poll(report, view, self.at(millis))
        }
    }

    const CHANGE: AraModelUpdate = AraModelUpdate::DocumentDataChanged;

    /// Once the document has settled every change is unsaved work: with or
    /// without the editor open, and the plug-in's answer to a host edit too.
    #[test]
    fn a_change_after_the_document_settled_is_unsaved_work() {
        let mut doc = Document::new();
        doc.host_edit(0, &[]);
        assert!(!doc.poll(100, false, &[]));
        assert!(doc.poll(10_000, false, &[CHANGE]));
        assert!(!doc.poll(10_100, false, &[]), "counted once");
        // A restore, or a region move Melodyne re-derives scales for: the
        // answer comes back inside the host's own edit.
        doc.host_edit(12_000, &[CHANGE]);
        assert!(doc.poll(12_016, true, &[]));
        assert!(doc.poll(12_500, true, &[CHANGE]), "the editor just opened");
    }

    /// Task rule: opening a project must not by itself leave it dirty. The
    /// restore is answered inside the host's edit and the analysis the load
    /// set off lands while the document is still settling; changes after it
    /// settled count, host-edit answers included.
    #[test]
    fn opening_a_document_does_not_mark_the_project_dirty() {
        let mut doc = Document::new();
        // The graph built and the archive restored, answered in the edit.
        doc.host_edit(0, &[CHANGE]);
        assert!(!doc.poll(16, false, &[]));
        // A later sync, and a source that missed being analysed afresh.
        doc.host_edit(900, &[]);
        assert!(!doc.poll(1_000, false, &[progress(ANALYSIS_STARTED)]));
        assert!(!doc.poll(4_000, false, &[CHANGE]), "still analysing");
        let done = [
            progress(ANALYSIS_COMPLETED),
            AraModelUpdate::SourceContentChanged {
                source: Some(AraSourceKey::from("asset-1")),
            },
            CHANGE,
        ];
        assert!(!doc.poll(6_000, false, &done));
        // A late straggler right after, and then quiet: the load has settled.
        assert!(!doc.poll(7_000, false, &[CHANGE]));
        assert!(!doc.poll(9_100, false, &[]));
        // From here every change is unsaved work.
        assert!(doc.poll(9_200, false, &[CHANGE]));
        doc.host_edit(20_000, &[CHANGE]);
        assert!(doc.poll(20_016, false, &[]));
        assert!(doc.poll(30_000, false, &[CHANGE]));
    }

    /// Finding (review): once a change was held while the editor was open and
    /// not settled, every later change during settling was held too, the
    /// plug-in's own analysis reports included, so a project opened with its
    /// editor showing came up dirty without an edit. Melodyne reports the
    /// data change of a finished analysis with the analysis completing (seen
    /// with Melodyne 5.4.2): that change, and one right after analysis news or
    /// a host edit, is the plug-in's own and is passed over.
    #[test]
    fn the_plugins_own_changes_do_not_dirty_a_project_opened_with_its_editor_open() {
        let mut doc = Document::new();
        // Restored inside the build, and analysis requested inside it too.
        doc.host_edit(0, &[CHANGE, progress(ANALYSIS_STARTED)]);
        assert!(
            !doc.poll(16, true, &[]),
            "the editor opens with the project"
        );
        assert!(!doc.poll(600, true, &[progress(ANALYSIS_UPDATED)]));
        // The analysis finishes: completion, content and the data change in
        // one report.
        let done = [
            progress(ANALYSIS_COMPLETED),
            AraModelUpdate::SourceContentChanged {
                source: Some(AraSourceKey::from("asset-1")),
            },
            CHANGE,
        ];
        assert!(!doc.poll(1_200, true, &done));
        // A straggler right after it.
        assert!(!doc.poll(1_600, true, &[CHANGE]));
        // The view settles: nothing was held, so nothing counts.
        assert!(!doc.poll(2_100, true, &[]));
        assert!(!doc.poll(3_300, true, &[]));
        // A change now is unsaved work.
        assert!(doc.poll(3_400, true, &[CHANGE]));
    }

    /// Finding (review, a 6 s analysis): with the editor open longer than the
    /// settle window, the plug-in's own data change at the end of a long
    /// analysis dirtied a freshly opened project. It comes with the source's
    /// analysis ending, which explains it, however long the analysis ran.
    /// An edit during the analysis that nothing ending explains still counts.
    #[test]
    fn a_long_analysis_ending_does_not_dirty_a_project_opened_with_its_editor_open() {
        let mut doc = Document::new();
        doc.host_edit(0, &[CHANGE, progress(ANALYSIS_STARTED)]);
        for at in (16..6_000).step_by(500) {
            assert!(
                !doc.poll(at, true, &[progress(ANALYSIS_UPDATED)]),
                "at {at}"
            );
        }
        let done = [
            progress(ANALYSIS_COMPLETED),
            AraModelUpdate::SourceContentChanged {
                source: Some(AraSourceKey::from("asset-1")),
            },
            CHANGE,
        ];
        assert!(!doc.poll(6_016, true, &done), "the plug-in's own change");
        assert!(
            !doc.poll(6_500, true, &[CHANGE]),
            "a straggler right after it"
        );
        // Settled: the user's edits count from here.
        assert!(!doc.poll(8_100, true, &[]));
        assert!(doc.poll(8_200, true, &[CHANGE]));

        // During the analysis, with the view settled, an edit counts.
        let mut doc = Document::new();
        doc.host_edit(0, &[progress(ANALYSIS_STARTED)]);
        assert!(!doc.poll(16, true, &[]));
        assert!(doc.poll(3_000, true, &[progress(ANALYSIS_UPDATED), CHANGE]));
        // A completion whose start was never seen still explains its change.
        let mut doc = Document::new();
        doc.host_edit(0, &[]);
        assert!(!doc.poll(16, true, &[]));
        assert!(!doc.poll(2_500, true, &[progress(ANALYSIS_COMPLETED), CHANGE]));
    }

    /// While the document settles, a change with the editor open and settled
    /// can be the user's, so it counts.
    #[test]
    fn a_change_in_the_open_editor_counts_while_the_document_settles() {
        let mut doc = Document::new();
        doc.host_edit(0, &[]);
        doc.poll(100, true, &[progress(ANALYSIS_STARTED)]);
        assert!(!doc.poll(1_000, true, &[CHANGE]), "view just opened");
        assert!(doc.poll(2_200, true, &[CHANGE]));
    }

    /// Finding (review): a change held back while the document settled was
    /// counted as seen, so an edit made just after opening the editor, during
    /// a long analysis, never marked the project dirty. It is held while
    /// nothing explains it, and counts once the view or the document has
    /// settled; a change the plug-in reports meanwhile is still passed over.
    #[test]
    fn an_edit_held_back_while_the_document_settles_counts_once_it_can() {
        let mut doc = Document::new();
        doc.host_edit(0, &[]);
        doc.poll(100, false, &[progress(ANALYSIS_STARTED)]);
        // A long analysis, quiet for a while; the user opens the editor and
        // edits before it has settled.
        assert!(!doc.poll(5_000, true, &[]));
        assert!(!doc.poll(5_500, true, &[CHANGE]));
        // Nothing new, but once the view has settled the held edit counts,
        // once.
        assert!(doc.poll(7_100, true, &[]));
        assert!(!doc.poll(7_200, true, &[]));

        // The editor closed again before it settled: the edit still counts,
        // once the document has settled.
        let mut doc = Document::new();
        doc.host_edit(0, &[]);
        doc.poll(100, false, &[progress(ANALYSIS_STARTED)]);
        doc.poll(5_000, true, &[]);
        assert!(!doc.poll(5_500, true, &[CHANGE]));
        assert!(!doc.poll(6_000, false, &[]), "still analysing");
        // The analysis finishing, with its own change: passed over, and the
        // held edit still waits.
        let done = [progress(ANALYSIS_COMPLETED), CHANGE];
        assert!(!doc.poll(9_000, false, &done));
        assert!(doc.poll(11_100, false, &[]), "the held edit, once");
        assert!(!doc.poll(11_200, false, &[]));

        // With no editor open the settling is passed over, as before.
        let mut doc = Document::new();
        doc.host_edit(0, &[]);
        assert!(!doc.poll(1_000, false, &[CHANGE]));
        assert!(!doc.poll(3_000, false, &[]));
    }

    /// Finding (review): Melodyne's creation-time start was never completed
    /// after a restore, and pinned the document as analysing for good.
    #[test]
    fn an_analysis_start_nothing_completes_can_be_settled() {
        let signals = ModelSignals::default();
        signals.record(&progress(ANALYSIS_STARTED));
        assert!(signals.report().analysing);
        signals.settle(&AraSourceKey::from("asset-1"));
        assert!(!signals.report().analysing);

        // Its content landing ends it too.
        signals.record(&progress(ANALYSIS_STARTED));
        signals.record(&AraModelUpdate::SourceContentChanged {
            source: Some(AraSourceKey::from("asset-1")),
        });
        assert!(!signals.report().analysing);

        // A start the host could not name only settles all at once.
        signals.record(&AraModelUpdate::AnalysisProgress {
            source: None,
            state: ANALYSIS_STARTED,
            value: 0.0,
        });
        signals.settle(&AraSourceKey::from("asset-1"));
        assert!(signals.report().analysing);
        signals.settle_all();
        assert!(!signals.report().analysing);
    }

    #[test]
    fn one_sources_stuck_start_does_not_hide_another_finishing() {
        let signals = ModelSignals::default();
        let other = |state| AraModelUpdate::AnalysisProgress {
            source: Some(AraSourceKey::from("asset-2")),
            state,
            value: 0.0,
        };
        signals.record(&progress(ANALYSIS_STARTED));
        signals.record(&other(ANALYSIS_STARTED));
        signals.record(&other(ANALYSIS_COMPLETED));
        assert_eq!(
            signals.running().sources.keys().collect::<Vec<_>>(),
            vec![&AraSourceKey::from("asset-1")]
        );
    }

    #[test]
    fn the_plugin_is_asked_about_its_analyses_only_when_it_has_gone_quiet() {
        let start = Instant::now();
        let at = |millis| start + Duration::from_millis(millis);
        assert!(
            !analysis_check_due(false, None, None, at(0)),
            "nothing runs"
        );
        assert!(analysis_check_due(true, None, None, at(0)));
        // Reports still coming: it is analysing.
        assert!(!analysis_check_due(true, Some(at(0)), None, at(2_000)));
        assert!(analysis_check_due(true, Some(at(0)), None, at(3_000)));
        // At most once per interval.
        assert!(!analysis_check_due(
            true,
            Some(at(0)),
            Some(at(3_000)),
            at(4_000)
        ));
        assert!(analysis_check_due(
            true,
            Some(at(0)),
            Some(at(3_000)),
            at(5_000)
        ));
    }

    #[test]
    fn the_plugins_own_answer_decides_which_analyses_are_over() {
        let key = |name: &str| AraSourceKey::from(name);
        let running = RunningAnalyses {
            sources: HashMap::from([(key("done"), 1), (key("busy"), 1), (key("gone"), 2)]),
            unknown: 0,
        };
        let graph = [key("done"), key("busy"), key("quiet")];
        let answer = |source: &AraSourceKey| match source.as_str() {
            "done" => Some(false),
            "busy" => Some(true),
            _ => None,
        };
        let finished = finished_analyses(&running, &graph, answer);
        assert_eq!(finished.sources, vec![key("done"), key("gone")]);
        assert!(!finished.all);

        // Unnamed starts settle only when every source says it is done.
        let unnamed = RunningAnalyses {
            sources: HashMap::new(),
            unknown: 1,
        };
        assert!(!finished_analyses(&unnamed, &graph, answer).all);
        assert!(finished_analyses(&unnamed, &graph, |_| Some(false)).all);
        assert!(
            finished_analyses(&unnamed, &[], |_| None).all,
            "nothing left to analyse"
        );
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
    /// so its start can come back inside the host edit's own report rather
    /// than from the periodic poll. It still keeps the document settling.
    #[test]
    fn an_analysis_that_starts_inside_a_host_edit_keeps_the_document_settling() {
        let mut doc = Document::new();
        doc.host_edit(0, &[progress(ANALYSIS_STARTED)]);
        assert!(!doc.poll(30_000, false, &[CHANGE]), "still analysing");
        assert!(!doc.poll(31_000, false, &[progress(ANALYSIS_COMPLETED)]));
        assert!(!doc.poll(33_100, false, &[]));
        assert!(doc.poll(33_200, false, &[CHANGE]));
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

#[cfg(test)]
mod ara_history_tests {
    use super::*;

    #[test]
    fn a_user_edit_is_a_step_and_undo_redo_walk_it() {
        let mut history = AraHistory::default();
        history.record(vec![0], false);
        history.record(vec![1], true);
        history.record(vec![2], true);
        assert_eq!(history.depth(), (2, 0));

        assert_eq!(history.peek(true).as_deref(), Some(&vec![1]));
        history.commit(true);
        assert_eq!(history.baseline.as_deref(), Some(&vec![1]));
        assert_eq!(history.depth(), (1, 1));

        assert_eq!(history.peek(false).as_deref(), Some(&vec![2]));
        history.commit(false);
        assert_eq!(history.baseline.as_deref(), Some(&vec![2]));
        assert_eq!(history.depth(), (2, 0));
    }

    /// The plug-in's own changes (an analysis, a host sync) move the
    /// baseline without becoming something Undo would take back, and an
    /// unchanged document records nothing.
    #[test]
    fn only_user_edits_that_change_the_document_are_steps() {
        let mut history = AraHistory::default();
        history.record(vec![0], false);
        history.record(vec![0], true);
        history.record(vec![5], false);
        assert_eq!(history.depth(), (0, 0));
        assert_eq!(history.baseline.as_deref(), Some(&vec![5]));
    }

    #[test]
    fn a_new_edit_after_undo_drops_the_redo() {
        let mut history = AraHistory::default();
        history.record(vec![0], false);
        history.record(vec![1], true);
        history.commit(true);
        assert_eq!(history.depth(), (0, 1));
        history.record(vec![9], true);
        assert_eq!(history.depth(), (1, 0));
    }

    #[test]
    fn the_history_is_bounded() {
        let mut history = AraHistory::default();
        history.record(vec![0], false);
        for step in 1..=(ARA_UNDO_DEPTH as u8 + 10) {
            history.record(vec![step], true);
        }
        assert_eq!(history.depth().0, ARA_UNDO_DEPTH);
    }

    #[test]
    fn record_answers_whether_it_added_a_step() {
        let mut history = AraHistory::default();
        assert!(!history.record(vec![0], true));
        assert!(!history.record(vec![0], true));
        assert!(!history.record(vec![1], false));
        assert!(history.record(vec![2], true));
    }

    /// Melodyne reports a note edit as a modification content change even
    /// where it never reports its document data changing; an undo the host
    /// restored is its own and not seen as a new one.
    #[test]
    fn a_modification_change_is_an_edit_until_an_undo_absorbs_it() {
        let signals = ModelSignals::default();
        let mut watch = DocumentWatch::default();
        assert!(!watch.take_content_edits(signals.report()));

        signals.record(&AraModelUpdate::ModificationContentChanged { clip: None });
        assert!(watch.take_content_edits(signals.report()));
        assert!(!watch.take_content_edits(signals.report()));

        signals.record(&AraModelUpdate::ModificationContentChanged { clip: None });
        watch.absorb(signals.report(), Instant::now());
        assert!(!watch.take_content_edits(signals.report()));
    }
}
