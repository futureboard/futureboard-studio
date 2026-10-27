//! Studio's master-bus tap, which its visualizer windows pull from. The
//! plug-in has no visualizer windows: the tap is always empty, and nothing
//! in the WhiteSharp editor reads it.

pub struct VisualizerTap;

pub struct TapRead {
    pub frames: usize,
    pub next: u64,
}

impl VisualizerTap {
    pub fn sample_rate(&self) -> u32 {
        48_000
    }

    pub fn read(&self, cursor: u64, _left: &mut [f32], _right: &mut [f32]) -> TapRead {
        TapRead {
            frames: 0,
            next: cursor,
        }
    }
}

pub fn visualizer_tap() -> &'static VisualizerTap {
    &VisualizerTap
}
