//! File audition and its decode worker.

use super::*;

/// Decode the newest queued Browser preview and hand it to the callback.
///
/// Requests superseded before or during their decode are dropped here rather
/// than sent, and the ones that do get sent still carry their token so the
/// callback can reject a source that lost the race after this check.
pub(super) fn audition_decode_worker(
    requests: Arc<AuditionRequests>,
    engine: std::sync::Weak<EngineInner>,
) {
    loop {
        let next = {
            let mut pending = requests.pending.lock();
            while pending.is_none() {
                // Timed waits let the worker notice a dropped engine instead of
                // parking forever on a mailbox nobody can fill again.
                if requests
                    .ready
                    .wait_for(&mut pending, std::time::Duration::from_millis(500))
                    .timed_out()
                    && engine.strong_count() == 0
                {
                    requests.worker_running.store(false, Ordering::Release);
                    return;
                }
            }
            pending.take()
        };
        let Some((token, path)) = next else {
            continue;
        };
        let Some(engine) = engine.upgrade() else {
            requests.worker_running.store(false, Ordering::Release);
            return;
        };
        let is_current = || token == engine.shared.audition_request_token.load(Ordering::Relaxed);
        if !is_current() {
            continue; // superseded while it sat in the mailbox — never decode it
        }
        // Preview only ever plays AUDITION_PREVIEW_SECONDS, so decode that head
        // rather than the whole file. This is what makes a long stem audible at
        // all: the full-file WAV path rejects anything >= 64 MB.
        match crate::audio_file::load_audio_file_head(
            &path,
            crate::audio_file::AUDITION_PREVIEW_SECONDS,
        ) {
            Ok(source) => {
                if !is_current() {
                    continue; // a newer selection landed while this decoded
                }
                if let Err(error) = engine.send_command(EngineCommand::StartAudition {
                    token,
                    source: Box::new(source),
                }) {
                    eprintln!("[audition] could not start preview for {path}: {error}");
                }
            }
            Err(error) => eprintln!("[audition] could not decode {path}: {error}"),
        }
    }
}

impl EngineInner {
    /// Queue a Browser sample preview. The decode itself runs on the single
    /// audition worker, so a burst of selections collapses to the newest one
    /// instead of racing a thread per click.
    pub(crate) fn audition_file_async(
        self: &Arc<Self>,
        path: String,
    ) -> Result<(), SphereAudioError> {
        self.cmd_sender().ok_or(SphereAudioError::EngineNotOpen)?;
        // Retiring a previous preview is the callback's first graveyard use in
        // a session that never loaded a project; prime it from here so that
        // enqueue can't spawn the drop thread on the audio thread.
        prime_audio_thread_resources();
        let token = self
            .shared
            .audition_request_token
            .fetch_add(1, Ordering::Relaxed)
            + 1;
        *self.audition_requests.pending.lock() = Some((token, path));
        self.audition_requests.ready.notify_one();
        self.ensure_audition_worker()
    }

    pub(crate) fn stop_audition(&self) -> Result<(), SphereAudioError> {
        // Invalidate any decode still in flight so it cannot start playing
        // after the user asked for silence.
        self.shared
            .audition_request_token
            .fetch_add(1, Ordering::Relaxed);
        self.audition_requests.pending.lock().take();
        self.send_command(EngineCommand::StopAudition)
    }

    pub(super) fn ensure_audition_worker(self: &Arc<Self>) -> Result<(), SphereAudioError> {
        if self
            .audition_requests
            .worker_running
            .swap(true, Ordering::AcqRel)
        {
            return Ok(()); // already draining the mailbox
        }
        let requests = Arc::clone(&self.audition_requests);
        let engine = Arc::downgrade(self);
        match std::thread::Builder::new()
            .name("browser-audition-decode".to_string())
            .spawn(move || audition_decode_worker(requests, engine))
        {
            Ok(_) => Ok(()),
            Err(error) => {
                self.audition_requests
                    .worker_running
                    .store(false, Ordering::Release);
                Err(SphereAudioError::NativeError(error.to_string()))
            }
        }
    }
}
