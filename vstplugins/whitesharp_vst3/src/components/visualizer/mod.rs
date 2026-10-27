//! Studio's visualizer renderer, which draws the WhiteSharp stage. The
//! visualizer windows themselves are not part of the plug-in.

#[path = "../../../../../crates/SphereUIComponents/src/components/visualizer/analysis.rs"]
pub mod analysis;
#[cfg(feature = "gpu-renderer")]
#[path = "../../../../../crates/SphereUIComponents/src/components/visualizer/gpu.rs"]
pub mod gpu;
#[path = "../../../../../crates/SphereUIComponents/src/components/visualizer/scene.rs"]
pub mod scene;

/// The kinds `scene` lays out; WhiteSharp's stage uses none of them, but
/// the scene module is written against the enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VisualizerKind {
    Spectrum,
    StereoImage,
    Loudness,
    Oscilloscope,
    Spectrogram,
}

impl VisualizerKind {
    pub const ALL: [Self; 5] = [
        Self::Spectrum,
        Self::StereoImage,
        Self::Loudness,
        Self::Oscilloscope,
        Self::Spectrogram,
    ];

    pub fn features(self) -> analysis::Features {
        match self {
            Self::Spectrum | Self::Spectrogram => analysis::Features {
                spectrum: true,
                loudness: false,
            },
            Self::Loudness => analysis::Features {
                spectrum: false,
                loudness: true,
            },
            Self::StereoImage | Self::Oscilloscope => analysis::Features::default(),
        }
    }
}
