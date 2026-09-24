//! The Tone/Amp slot: a mutually-exclusive choice of engine — the classic
//! modeled [`Amp`], the neural [`NamCapture`], or a bare pass-through — never
//! more than one running at a time, and NAM state never embedded inside the
//! classic amp implementation (they're two independent processors this stage
//! merely dispatches between).

use super::AmpModel;
use super::amp::Amp;
use super::nam::{NamCapture, NamCaptureInfo, PreparedNamRuntime};

/// Which engine the Tone/Amp slot currently runs. Mutually exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ToneEngineKind {
    /// The modeled tube-stage + passive tonestack ([`Amp`]).
    Classic,
    /// A loaded `.nam` neural capture ([`NamCapture`]).
    NamCapture,
    /// Pass the signal through unmodified.
    Bypass,
}

impl ToneEngineKind {
    pub const ALL: &'static [Self] = &[Self::Classic, Self::NamCapture, Self::Bypass];

    pub fn from_index(i: u32) -> Self {
        Self::ALL.get(i as usize).copied().unwrap_or(Self::Classic)
    }

    pub fn index(self) -> u8 {
        match self {
            Self::Classic => 0,
            Self::NamCapture => 1,
            Self::Bypass => 2,
        }
    }
}

pub(super) struct ToneStage {
    engine: ToneEngineKind,
    classic: Amp,
    nam: NamCapture,
}

impl ToneStage {
    pub(super) fn new(sample_rate: f32) -> Self {
        Self {
            engine: ToneEngineKind::Classic,
            classic: Amp::new(sample_rate),
            nam: NamCapture::new(sample_rate),
        }
    }

    pub(super) fn set_sample_rate(&mut self, sample_rate: f32) {
        self.classic.set_sample_rate(sample_rate);
        self.nam.set_sample_rate(sample_rate);
    }

    pub(super) fn reset(&mut self) {
        self.classic.reset();
        self.nam.reset();
    }

    /// Audio thread: call once per audio block (never per sample) so the NAM
    /// engine can adopt a pending capture swap at a safe boundary.
    pub(super) fn begin_block(&mut self) {
        self.nam.begin_block();
    }

    pub(super) fn set_engine(&mut self, engine: ToneEngineKind) {
        self.engine = engine;
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn configure_classic(
        &mut self,
        model: AmpModel,
        gain: f32,
        bass: f32,
        middle: f32,
        treble: f32,
        presence: f32,
        master: f32,
    ) {
        self.classic
            .configure(model, gain, bass, middle, treble, presence, master);
    }

    pub(super) fn configure_nam(
        &mut self,
        input_trim_db: f32,
        output_trim_db: f32,
        mix_pct: f32,
        loudness_norm_on: bool,
        slim_size: f32,
    ) {
        self.nam.configure(
            input_trim_db,
            output_trim_db,
            mix_pct,
            loudness_norm_on,
            slim_size,
        );
    }

    /// Control thread: submit a freshly-built capture for the audio thread to
    /// adopt at the next block boundary.
    pub(super) fn submit_nam_runtime(&self, runtime: Box<PreparedNamRuntime>) {
        self.nam.submit(runtime);
    }

    /// Clone out a control-side NAM loader handle (see [`super::nam::NamLoader`]).
    pub(super) fn nam_loader(&self) -> super::nam::NamLoader {
        self.nam.loader()
    }

    /// Control thread: drop any capture the audio thread has retired.
    pub(super) fn poll_nam_garbage(&mut self) {
        self.nam.poll_garbage();
    }

    pub(super) fn nam_capture_info(&self) -> Option<NamCaptureInfo> {
        self.nam.active_info()
    }

    pub(super) fn nam_latency_samples(&self) -> usize {
        self.nam.latency_samples()
    }

    #[inline]
    pub(super) fn process(&mut self, left: f32, right: f32) -> (f32, f32) {
        match self.engine {
            ToneEngineKind::Classic => self.classic.process(left, right),
            ToneEngineKind::NamCapture => self.nam.process(left, right),
            ToneEngineKind::Bypass => (left, right),
        }
    }
}
