//! Stand-in for Studio's frame profiler. The mounted playhead helpers open a
//! scope; the plug-in does not record it.

pub struct PerfScope;

impl PerfScope {
    pub fn enter(_name: &'static str) -> Self {
        Self
    }
}
