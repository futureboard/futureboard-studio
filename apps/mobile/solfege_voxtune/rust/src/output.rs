//! The Android AAudio output stream around the shared engine.

pub use sphere_voxtune::shared::Shared;

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
