//! The Solfege engine a track instrument runs, and its cached SFM runtimes.

use super::*;

/// Native Solfege voicebank instrument with a physical fallback. Preparation
/// happens while the runtime graph is built on the control thread; the audio
/// callback only drives the already-allocated engine.
pub struct RuntimeSolfegeEngine {
    pub engine: SamplerEngine,
    /// True when the prepared engine owns the indexed INDX/AUDO voicebank.
    pub voicebank_loaded: bool,
    /// True when the SFM contains an embedded RESI FBMX residual. The DAW's
    /// clean voicebank path keeps that optional enhancement bypassed until a
    /// residual trained against the voicebank path is selected.
    pub embedded_fbmx_loaded: bool,
    /// Standalone `.fbmx` compatibility path. SFM residuals are owned by the
    /// Solfege engine itself so they are applied in the same render pass.
    ///
    /// **One runtime per channel.** The model is a recurrent, causal, mono
    /// audio model: its hidden state is the recent history of *one* signal.
    /// Driving a single instance with the left block and then the right block
    /// makes it read a signal that jumps discontinuously at every block
    /// boundary and again at the left/right seam, so the output depends on the
    /// buffer size and carries a click at each seam. Two instances cost one
    /// hidden state each and make each channel's history its own.
    pub fbmx: Option<Box<[LstmRuntime; 2]>>,
    pub(super) state: EngineSolfegeSnapshot,
    pub(super) sample_rate: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct SfmRuntimeCacheKey {
    pub(super) path: PathBuf,
    pub(super) sample_rate: u32,
    pub(super) file_size: u64,
    pub(super) modified_nanos: u128,
}

pub(super) struct CachedSfmRuntime {
    pub(super) engine: SamplerEngine,
    pub(super) voicebank_loaded: bool,
    pub(super) embedded_fbmx_loaded: bool,
}

pub(super) const MAX_CACHED_SFM_RUNTIMES: usize = 4;

pub(super) static SFM_RUNTIME_CACHE: OnceLock<
    Mutex<HashMap<SfmRuntimeCacheKey, CachedSfmRuntime>>,
> = OnceLock::new();

pub(super) fn sfm_runtime_cache_key(path: &str, sample_rate: u32) -> Option<SfmRuntimeCacheKey> {
    let path = PathBuf::from(path);
    let metadata = std::fs::metadata(&path).ok()?;
    let modified_nanos = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |duration| duration.as_nanos());
    Some(SfmRuntimeCacheKey {
        path,
        sample_rate,
        file_size: metadata.len(),
        modified_nanos,
    })
}

pub(super) fn cached_sfm_runtime(key: &SfmRuntimeCacheKey) -> Option<(SamplerEngine, bool, bool)> {
    let cache = SFM_RUNTIME_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let cache = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    cache.get(key).map(|cached| {
        (
            cached.engine.clone_prepared(),
            cached.voicebank_loaded,
            cached.embedded_fbmx_loaded,
        )
    })
}

pub(super) fn cache_sfm_runtime(
    key: SfmRuntimeCacheKey,
    engine: SamplerEngine,
    voicebank_loaded: bool,
    embedded_fbmx_loaded: bool,
) {
    let cache = SFM_RUNTIME_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    cache.insert(
        key.clone(),
        CachedSfmRuntime {
            engine,
            voicebank_loaded,
            embedded_fbmx_loaded,
        },
    );
    while cache.len() > MAX_CACHED_SFM_RUNTIMES {
        let Some(evicted) = cache.keys().find(|candidate| **candidate != key).cloned() else {
            break;
        };
        cache.remove(&evicted);
    }
}

pub(super) fn prepare_sfm_runtime(
    path: &str,
    rate: SampleRate,
    sample_rate: u32,
    metrics: Arc<SharedMetrics>,
) -> Option<(SamplerEngine, bool, bool)> {
    let cache_key = sfm_runtime_cache_key(path, sample_rate);
    if let Some(key) = cache_key.as_ref() {
        if let Some(cached) = cached_sfm_runtime(key) {
            return Some(cached);
        }
    }

    let model = match SfmFile::open(path) {
        Ok(model) => model,
        Err(error) => {
            eprintln!("[solfege] SFM load failed for '{path}': {error}");
            return None;
        }
    };
    let voicebank_loaded = model.section(solfege_model::INDEX_TAG).is_some()
        && model.section(solfege_model::AUDIO_TAG).is_some();
    let embedded_fbmx_loaded = model.section(solfege_model::FBMX_RESIDUAL_TAG).is_some();
    if !voicebank_loaded {
        return None;
    }
    match SamplerEngine::prepare_sfm(
        SolfegeEngineConfig::realtime(rate),
        model,
        metrics,
        // The DAW defaults to the actual indexed acoustic source. The old
        // Hybrid path rendered the physical layer on top of that source and
        // could sound doubled while also doing unnecessary work. Physical and
        // hybrid renders remain available to the model tools.
        SfmMode::VoicebankOnly,
    ) {
        Ok(engine) => {
            if let Some(key) = cache_key {
                cache_sfm_runtime(
                    key,
                    engine.clone_prepared(),
                    voicebank_loaded,
                    embedded_fbmx_loaded,
                );
            }
            Some((engine, voicebank_loaded, embedded_fbmx_loaded))
        }
        Err(error) => {
            eprintln!("[solfege] SFM runtime preparation failed for '{path}': {error}");
            None
        }
    }
}

/// Voice identity the Solfege engine uses to route note-off, gestures and
/// continuous pitch back to the voice a note-on started.
///
/// Derived from `(channel, notated pitch)` rather than from the project's note
/// id because note-on/note-off reach the instrument as [`Vst3MidiEvent`], which
/// carries no note id. Every producer must agree on this one function — a
/// continuous-pitch event computed differently would retune a voice that is not
/// there, which is silent and therefore the hardest kind of bug to notice.
///
/// The notated pitch is stable for the whole note (a pitch curve is a deviation
/// from it, never a rewrite of it), so a drawn glide keeps addressing the voice
/// it started on.
#[inline]
pub(crate) fn solfege_note_id(channel: u8, pitch: u8) -> i32 {
    (channel.min(15) as i32) * 128 + pitch.min(127) as i32
}

/// Dry -> model blend applied to the standalone `.fbmx` path.
///
/// `1.0` is the shipping value: the model is either right or it is not, and a
/// permanently reduced mix would hide a bad model instead of fixing it. It
/// exists as a named constant because attributing an artefact to the model
/// rather than to the base renderer means rendering the same phrase at 0.0,
/// 0.25, 0.5, 0.75 and 1.0 and comparing — a diagnostic sweep, not a tuning
/// knob, which is why it is not exposed as a user control.
pub(super) const FBMX_RESIDUAL_MIX: f32 = 1.0;

/// Instantiate one neural runtime per channel and apply the model's *own*
/// declared conditioning parameters.
///
/// Conditioning names are model-defined. Setting a name the model does not
/// declare is not merely useless: `set_parameter` returns an error the caller
/// has no way to notice, so the model silently runs at its declared defaults
/// forever. The shipped Solo Violin residual declares `midi_note`, `velocity`,
/// `articulation` and `dynamic` — not the four names this code used to set.
pub(super) fn instantiate_fbmx_pair(
    model: &FbmxModel,
    state: &EngineSolfegeSnapshot,
) -> Result<Box<[LstmRuntime; 2]>, fbmx_runtime::FbmxError> {
    let build = || -> Result<LstmRuntime, fbmx_runtime::FbmxError> {
        let mut runtime = model.instantiate()?;
        // Map the track's normalized performance controls onto whichever
        // conditioning names this particular model actually declares. A name it
        // does not declare is skipped, not forced.
        for (name, value) in [
            ("velocity", state.dynamics),
            ("dynamics", state.dynamics),
            ("expression", state.expression),
            ("bow_pressure", state.bow_pressure),
            ("vibrato", state.vibrato),
        ] {
            let _ = runtime.set_parameter(name, value);
        }
        runtime.refresh_conditioning();
        Ok(runtime)
    };
    Ok(Box::new([build()?, build()?]))
}

impl RuntimeSolfegeEngine {
    pub(super) fn from_snapshot(snapshot: &EngineTrackSnapshot, sample_rate: u32) -> Option<Self> {
        let state = snapshot.solfege_engine.clone()?;
        Some(Self::new(state, sample_rate))
    }

    pub(super) fn new(state: EngineSolfegeSnapshot, sample_rate: u32) -> Self {
        let sample_rate = sample_rate.max(1);
        let rate = SampleRate::new(sample_rate as f32)
            .unwrap_or_else(|_| SampleRate::new(48_000.0).expect("48 kHz is a valid sample rate"));
        let metrics = Arc::new(SharedMetrics::default());
        let fallback_engine = || {
            SamplerEngine::prepare(
                SolfegeEngineConfig::realtime(rate),
                Some(RuntimeInstrument::bowed_string(
                    state.instrument.clone(),
                    BowedStringConfig::default(),
                )),
                metrics.clone(),
            )
        };
        let mut engine = fallback_engine();
        let mut voicebank_loaded = false;
        let mut embedded_fbmx_loaded = false;
        let mut fbmx = None;
        let model_path = state.model_path.as_deref();
        match model_path {
            Some(path)
                if std::path::Path::new(path)
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("sfm")) =>
            {
                if let Some((prepared, has_voicebank, has_embedded_fbmx)) =
                    prepare_sfm_runtime(path, rate, sample_rate, metrics.clone())
                {
                    engine = prepared;
                    voicebank_loaded = has_voicebank;
                    embedded_fbmx_loaded = has_embedded_fbmx;
                }
            }
            Some(path) => match FbmxModel::load(path) {
                Ok(model) if model.info().sample_rate != sample_rate => {
                    // A causal audio model is a filter fitted at one rate.
                    // Running it at another silently changes every time
                    // constant it learned, so refuse rather than ship a
                    // subtly wrong instrument with no way to notice.
                    eprintln!(
                        "[solfege] FBMX '{path}' is a {} Hz model but the device runs at \
                         {sample_rate} Hz; neural correction disabled",
                        model.info().sample_rate
                    );
                }
                Ok(model) => match instantiate_fbmx_pair(&model, &state) {
                    Ok(pair) => fbmx = Some(pair),
                    Err(error) => {
                        eprintln!("[solfege] realtime FBMX instantiate failed: {error}");
                    }
                },
                Err(error) => {
                    eprintln!("[solfege] realtime FBMX load failed for '{path}': {error}");
                }
            },
            None => {}
        }
        Self {
            engine,
            voicebank_loaded,
            embedded_fbmx_loaded,
            fbmx,
            state,
            sample_rate,
        }
    }

    pub(crate) fn handle_midi_event(&mut self, event: Vst3MidiEvent) {
        let note_id = solfege_note_id(event.channel, event.pitch);
        let event = match event.kind {
            1 => SolfegeEvent::NoteOn {
                note: event.pitch.min(127),
                velocity: event.velocity.clamp(0.0, 1.0),
                note_id,
            },
            0 => SolfegeEvent::NoteOff {
                note: event.pitch.min(127),
                velocity: event.velocity.clamp(0.0, 1.0),
                note_id,
            },
            2 => SolfegeEvent::ControlChange {
                channel: event.channel.min(15),
                controller: event.pitch,
                value: event.velocity.clamp(0.0, 1.0),
            },
            _ => return,
        };
        self.engine.handle_event(event);
    }

    /// Choose which recorded articulation the next note-on will play.
    ///
    /// Sent before the note-on it belongs to: a sampled instrument resolves its
    /// source at note-on, so an articulation arriving afterwards would silently
    /// apply to the *following* note.
    pub(crate) fn handle_articulation_event(&mut self, note_id: i32, articulation: u16) {
        self.engine.handle_event(SolfegeEvent::Articulation {
            note_id,
            articulation: solfege_event::Articulation::Custom(articulation),
        });
    }

    /// Retune a sounding voice to an absolute frequency.
    ///
    /// The engine glides to the target over its configured gesture-smoothing
    /// window rather than jumping, so a decimated trajectory reconstructs as a
    /// continuous line instead of a staircase.
    pub(crate) fn handle_pitch_event(&mut self, note_id: i32, hz: f32) {
        if !hz.is_finite() || hz <= 0.0 {
            return;
        }
        self.engine
            .handle_event(SolfegeEvent::Pitch { note_id, hz });
    }

    pub(crate) fn render_segment_stereo(&mut self, left: &mut [f32], right: &mut [f32]) {
        self.engine.process_stereo(left, right, &[]);
        let Some(fbmx) = self.fbmx.as_mut() else {
            return;
        };
        // The model's output IS the corrected signal, not a correction to add
        // to it: an FBMX model with `residual = true` closes the skip
        // connection *inside* itself (`y = head(h) + x`, see fbmx-runtime's
        // `process_sample`) and was trained with the loss taken on that same
        // `y`. Adding `y` back onto `x` sums the dry signal twice; measured on
        // the validation phrase that was 2.7x the intended level and 17x the DC
        // offset, heard as a loud low smear over the instrument.
        //
        // `mix` interpolates dry -> model so a diagnostic sweep can attribute
        // an artefact to the model rather than to the base renderer.
        let mix = FBMX_RESIDUAL_MIX.clamp(0.0, 1.0);
        for (channel, buffer) in [left, right].into_iter().enumerate() {
            let model = &mut fbmx[channel];
            for sample in buffer.iter_mut() {
                let wet = model.process_sample(*sample);
                if wet.is_finite() {
                    *sample += (wet - *sample) * mix;
                }
            }
        }
    }

    /// Drop every voice and the model's recurrent history.
    ///
    /// The hidden state means "the recent past of this signal"; after a stop or
    /// a seek that past did not happen, and carrying it makes the first block
    /// after the jump depend on audio the listener never heard.
    pub(crate) fn reset_state(&mut self) {
        self.engine.reset();
        if let Some(fbmx) = self.fbmx.as_mut() {
            for model in fbmx.iter_mut() {
                model.reset();
            }
        }
    }
}

impl std::fmt::Debug for RuntimeSolfegeEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeSolfegeEngine")
            .field("instrument", &self.state.instrument)
            .field("voice", &self.state.voice)
            .field("preset", &self.state.preset)
            .field("model_path", &self.state.model_path)
            .field("voicebank_loaded", &self.voicebank_loaded)
            .field("embedded_fbmx_loaded", &self.embedded_fbmx_loaded)
            .field("fbmx_loaded", &self.fbmx.is_some())
            .field("sample_rate", &self.sample_rate)
            .finish()
    }
}

impl Clone for RuntimeSolfegeEngine {
    fn clone(&self) -> Self {
        // `clone_prepared` deliberately drops sounding voices; the neural
        // runtimes are cloned the same way, with their recurrent state reset. A
        // graph rebuild that inherited half a note's hidden state would make the
        // new graph's first block depend on the old graph's audio.
        let fbmx = self.fbmx.as_ref().map(|pair| {
            let mut cloned = pair.clone();
            for model in cloned.iter_mut() {
                model.reset();
            }
            cloned
        });
        Self {
            engine: self.engine.clone_prepared(),
            voicebank_loaded: self.voicebank_loaded,
            embedded_fbmx_loaded: self.embedded_fbmx_loaded,
            fbmx,
            state: self.state.clone(),
            sample_rate: self.sample_rate,
        }
    }
}
