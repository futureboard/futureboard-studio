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
//! the GPUI main thread. The plug-in calls back on its own threads; those
//! callbacks land on the provider objects below, which own everything they need
//! and hand results to the UI through bounded queues drained each frame. No
//! provider touches GPUI state or the audio thread.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use sphere_ara_host::{
    AraAudioAccess, AraClipKey, AraGraph, AraHostError, AraModelObserver, AraModelUpdate,
    AraMusicalTimeline, AraRendererId, AraResult, AraRoles, AraSampleReader, AraSession,
    AraSessionConfig, AraSourceKey, AraTransportControl, AraTransportRequest,
};

/// Identifies one ARA session: a plug-in hosted on one track.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AraSessionKey {
    pub plugin_id: String,
    pub track_id: String,
}

/// How many pending plug-in callbacks the UI will buffer before dropping.
///
/// Bounded on purpose: a plug-in analysing a long clip emits progress
/// continuously, and an unbounded queue would grow without limit whenever the UI
/// falls behind. Dropping the oldest keeps the newest progress, which is the
/// only value anyone looks at.
const INBOX_CAPACITY: usize = 512;

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

/// Queues plug-in model updates for the UI to apply.
struct ModelBridge {
    inbox: Arc<Inbox<AraModelUpdate>>,
}

impl AraModelObserver for ModelBridge {
    fn notify(&self, update: AraModelUpdate) {
        self.inbox.push(update);
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
    /// Clips currently assigned to the renderer, in graph order.
    clips: Vec<AraClipKey>,
    /// Sources this session's clips read from, so the audio library can be
    /// rebuilt without consulting the timeline.
    audio: Arc<AudioLibrary>,
    /// Archive identifier the plug-in writes under, captured at open.
    archive_id: String,
    plugin_name: String,
    /// The saved archive the plug-in refused to restore, written back in
    /// place of the live document until the plug-in changes its state.
    ///
    /// Storing the live document instead would overwrite the user's saved
    /// edits with the empty document the failed restore left behind, and the
    /// loss would only show the next time the project was opened.
    unrestored_archive: Option<(String, Vec<u8>)>,
}

/// Every live ARA session, plus the queues its plug-ins post into.
#[derive(Default)]
pub struct AraState {
    sessions: HashMap<AraSessionKey, AraTrackSession>,
    transport_inbox: Arc<Inbox<AraTransportRequest>>,
    model_inbox: Arc<Inbox<AraModelUpdate>>,
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

    /// Drains model updates posted by plug-ins.
    pub fn take_model_updates(&self) -> Vec<AraModelUpdate> {
        self.model_inbox.drain()
    }

    /// The plug-ins changed their own documents: from here on their live
    /// state is newer than any archive they failed to restore, so saving
    /// stores the live state again.
    pub fn document_data_changed(&mut self) {
        for session in self.sessions.values_mut() {
            session.unrestored_archive = None;
        }
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
        let config = AraSessionConfig {
            document_name: Some(format!("Futureboard — {}", key.track_id)),
            audio: Arc::clone(&audio) as Arc<dyn AraAudioAccess>,
            transport: Arc::new(TransportBridge {
                inbox: Arc::clone(&self.transport_inbox),
            }),
            observer: Arc::new(ModelBridge {
                inbox: Arc::clone(&self.model_inbox),
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
            },
        );
        Ok(self
            .sessions
            .get_mut(key)
            .expect("inserted immediately above"))
    }

    /// Applies a freshly built graph to one session and re-assigns its regions.
    ///
    /// Rendering is disabled across the change: a playback region may not be
    /// destroyed while a renderer still holds it, and the plug-in is entitled to
    /// assume the region set is stable while it renders.
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
            Self::install_renderers(engine, &self.sessions, &key.track_id);
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

        Self::install_renderers(engine, &self.sessions, &key.track_id);
        Ok(())
    }

    /// Rebuilds and installs the engine's renderer list for one track.
    ///
    /// The engine replaces a track's whole list at once, so every session on the
    /// track has to be sent together — installing one at a time would drop the
    /// others.
    fn install_renderers(
        engine: &DirectAudio::AudioEngine,
        sessions: &HashMap<AraSessionKey, AraTrackSession>,
        track_id: &str,
    ) {
        let renderers: Vec<DirectAudio::RuntimeAraRenderer> = sessions
            .iter()
            .filter(|(key, _)| key.track_id == track_id)
            .map(|(key, session)| DirectAudio::RuntimeAraRenderer {
                instance_id: format!("ara:{}:{}", key.plugin_id, key.track_id),
                latency_samples: session.processor.get_latency_samples().max(0) as u32,
                processor: session.processor.clone(),
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
            Self::install_renderers(engine, &self.sessions, &key.track_id);
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
        // The newest values survive: analysis progress is only useful at its
        // latest value, and an unbounded queue is not an option on a callback.
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
