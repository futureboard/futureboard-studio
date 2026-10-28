//! Engine sharing between the UI (FFI) thread and the native audio thread,
//! plus the Android AAudio output stream.
//!
//! The audio callback never blocks: it `try_lock`s the engine for a bounded
//! number of spins and renders silence (counted in `lock_misses`) if the UI
//! thread holds it. Playhead, level and play state are published through
//! atomics after every block so the UI reads them without taking the lock.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, TryLockError};

use crate::engine::CantoEngine;

pub struct Shared {
    engine: Mutex<CantoEngine>,
    sample_rate: f32,
    playhead_bits: AtomicU64,
    level_bits: AtomicU32,
    playing: AtomicBool,
    auditioning: AtomicBool,
    lock_misses: AtomicU32,
}

impl Shared {
    pub fn new(engine: CantoEngine) -> Self {
        let sample_rate = engine.sample_rate();
        Self {
            engine: Mutex::new(engine),
            sample_rate,
            playhead_bits: AtomicU64::new(0f64.to_bits()),
            level_bits: AtomicU32::new(0f32.to_bits()),
            playing: AtomicBool::new(false),
            auditioning: AtomicBool::new(false),
            lock_misses: AtomicU32::new(0),
        }
    }

    pub fn sample_rate(&self) -> f32 {
        self.sample_rate
    }

    /// Blocking lock for the UI thread. A panic inside the engine poisons the
    /// mutex; the engine state is still usable, so recover it.
    pub fn lock(&self) -> MutexGuard<'_, CantoEngine> {
        self.engine.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn try_lock(&self) -> Option<MutexGuard<'_, CantoEngine>> {
        // UI-side critical sections are a few microseconds (single-note
        // syncs during drags); spinning briefly avoids a silent block.
        for _ in 0..1024 {
            match self.engine.try_lock() {
                Ok(guard) => return Some(guard),
                Err(TryLockError::Poisoned(p)) => return Some(p.into_inner()),
                Err(TryLockError::WouldBlock) => std::hint::spin_loop(),
            }
        }
        None
    }

    pub fn publish(&self, engine: &CantoEngine) {
        self.playhead_bits
            .store(engine.playhead_sec.to_bits(), Ordering::Relaxed);
        self.level_bits
            .store(engine.meter_level().to_bits(), Ordering::Relaxed);
        self.playing.store(engine.playing, Ordering::Relaxed);
        self.auditioning
            .store(engine.is_auditioning(), Ordering::Relaxed);
    }

    pub fn playhead(&self) -> f64 {
        f64::from_bits(self.playhead_bits.load(Ordering::Relaxed))
    }

    pub fn level(&self) -> f32 {
        f32::from_bits(self.level_bits.load(Ordering::Relaxed))
    }

    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::Relaxed)
    }

    pub fn is_auditioning(&self) -> bool {
        self.auditioning.load(Ordering::Relaxed)
    }

    pub fn lock_misses(&self) -> u32 {
        self.lock_misses.load(Ordering::Relaxed)
    }

    /// Audio-thread render: never blocks, never allocates.
    pub fn render_realtime(&self, out: &mut [f32]) {
        match self.try_lock() {
            Some(mut engine) => {
                engine.render_stereo_f32(out);
                self.publish(&engine);
            }
            None => {
                out.fill(0.0);
                self.lock_misses.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn realtime_render_never_blocks_on_a_held_lock() {
        let shared = Shared::new(CantoEngine::new(48_000.0));
        let mut out = vec![1.0f32; 256];

        // Unlocked: renders (silence, nothing loaded) and publishes state.
        shared.render_realtime(&mut out);
        assert!(out.iter().all(|&s| s == 0.0));
        assert_eq!(shared.lock_misses(), 0);

        // UI thread holds the engine: the callback must return immediately
        // with silence and count the miss instead of waiting.
        out.fill(1.0);
        let guard = shared.lock();
        let started = std::time::Instant::now();
        shared.render_realtime(&mut out);
        assert!(started.elapsed() < std::time::Duration::from_millis(5));
        drop(guard);
        assert!(out.iter().all(|&s| s == 0.0));
        assert_eq!(shared.lock_misses(), 1);
    }

    #[test]
    fn published_transport_matches_engine() {
        let shared = Shared::new(CantoEngine::new(48_000.0));
        {
            let mut engine = shared.lock();
            engine.playing = true;
            engine.playhead_sec = 1.25;
            shared.publish(&engine);
        }
        assert!(shared.is_playing());
        assert_eq!(shared.playhead(), 1.25);
    }
}

#[cfg(target_os = "android")]
pub use android::Output;

#[cfg(target_os = "android")]
mod android {
    use std::ffi::c_void;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::Shared;

    use crate::aaudio::*;

    struct CallbackContext {
        shared: Arc<Shared>,
        disconnected: AtomicBool,
    }

    unsafe extern "C" fn data_callback(
        _stream: *mut Stream,
        user: *mut c_void,
        audio: *mut c_void,
        frames: i32,
    ) -> i32 {
        let ctx = unsafe { &*(user as *const CallbackContext) };
        let len = frames.max(0) as usize * 2;
        let out = unsafe { std::slice::from_raw_parts_mut(audio as *mut f32, len) };
        // Never unwind across the C boundary.
        if catch_unwind(AssertUnwindSafe(|| ctx.shared.render_realtime(out))).is_err() {
            let out = unsafe { std::slice::from_raw_parts_mut(audio as *mut f32, len) };
            out.fill(0.0);
        }
        CALLBACK_CONTINUE
    }

    unsafe extern "C" fn error_callback(_stream: *mut Stream, user: *mut c_void, _error: i32) {
        // Called on device loss (e.g. headphones unplugged). The stream can't
        // be reopened from here; `Output::is_disconnected` lets the owner do it.
        let ctx = unsafe { &*(user as *const CallbackContext) };
        ctx.disconnected.store(true, Ordering::Relaxed);
    }

    pub struct Output {
        api: &'static Api,
        stream: *mut Stream,
        ctx: *mut CallbackContext,
        running: AtomicBool,
    }

    // The raw pointers are only touched from the owning FFI handle.
    unsafe impl Send for Output {}

    impl Output {
        pub fn open(shared: Arc<Shared>) -> Result<Output, String> {
            let api = Api::get().ok_or("AAudio is not available (requires Android 8.0+)")?;
            let sample_rate = shared.sample_rate() as i32;
            let ctx = Box::into_raw(Box::new(CallbackContext {
                shared,
                disconnected: AtomicBool::new(false),
            }));
            unsafe {
                let mut builder: *mut Builder = std::ptr::null_mut();
                if (api.create_builder)(&mut builder) != AAUDIO_OK || builder.is_null() {
                    drop(Box::from_raw(ctx));
                    return Err("AAudio_createStreamBuilder failed".into());
                }
                (api.set_direction)(builder, DIRECTION_OUTPUT);
                (api.set_sample_rate)(builder, sample_rate);
                (api.set_channel_count)(builder, 2);
                (api.set_format)(builder, FORMAT_PCM_FLOAT);
                (api.set_sharing_mode)(builder, SHARING_MODE_SHARED);
                (api.set_performance_mode)(builder, PERFORMANCE_MODE_LOW_LATENCY);
                (api.set_data_callback)(builder, data_callback, ctx as *mut c_void);
                (api.set_error_callback)(builder, error_callback, ctx as *mut c_void);
                let mut stream: *mut Stream = std::ptr::null_mut();
                let result = (api.open_stream)(builder, &mut stream);
                (api.delete_builder)(builder);
                if result != AAUDIO_OK || stream.is_null() {
                    drop(Box::from_raw(ctx));
                    return Err(format!("AAudio openStream failed ({result})"));
                }
                let output = Output {
                    api,
                    stream,
                    ctx,
                    running: AtomicBool::new(false),
                };
                if (api.get_sample_rate)(stream) != sample_rate
                    || (api.get_channel_count)(stream) != 2
                    || (api.get_format)(stream) != FORMAT_PCM_FLOAT
                {
                    return Err("AAudio opened with an unexpected format".into());
                }
                // ~20 ms of buffering, in whole bursts: enough headroom for
                // the PSOLA render on slower cores without audible latency.
                let burst = (api.get_frames_per_burst)(stream).max(1);
                let target = ((sample_rate / 50 + burst - 1) / burst).max(2) * burst;
                let capacity = (api.get_buffer_capacity)(stream);
                let size = if capacity > 0 {
                    target.min(capacity)
                } else {
                    target
                };
                (api.set_buffer_size)(stream, size);
                Ok(output)
            }
        }

        /// Idempotent: starting a running stream is a no-op (AAudio would
        /// report an invalid-state error for a second requestStart).
        pub fn start(&self) -> bool {
            if self.running.load(Ordering::Relaxed) {
                return true;
            }
            let ok = unsafe { (self.api.request_start)(self.stream) == AAUDIO_OK };
            self.running.store(ok, Ordering::Relaxed);
            ok
        }

        pub fn pause(&self) {
            if self.running.swap(false, Ordering::Relaxed) {
                unsafe {
                    (self.api.request_pause)(self.stream);
                }
            }
        }

        pub fn is_disconnected(&self) -> bool {
            unsafe { (*self.ctx).disconnected.load(Ordering::Relaxed) }
        }

        pub fn xrun_count(&self) -> i32 {
            unsafe { (self.api.get_xrun_count)(self.stream) }
        }
    }

    impl Drop for Output {
        fn drop(&mut self) {
            unsafe {
                (self.api.request_stop)(self.stream);
                // close() waits for any in-flight callback, so the context
                // can be freed afterwards.
                (self.api.close)(self.stream);
                drop(Box::from_raw(self.ctx));
            }
        }
    }
}
