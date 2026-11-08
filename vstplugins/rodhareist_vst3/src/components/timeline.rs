//! The fader's dB scale, from Studio's mixer state (whose module needs the
//! whole timeline). Keep it in step with Studio's `volume`.

pub mod timeline_state {
    pub mod volume {
        pub const MIN_DB: f32 = -60.0;
        pub const MAX_DB: f32 = 6.0;

        pub fn db_to_norm(db: f32) -> f32 {
            ((db - MIN_DB) / (MAX_DB - MIN_DB)).clamp(0.0, 1.0)
        }
    }
}
