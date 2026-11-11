//! Insert parameters, plug-in bridge sinks, ARA renderers and insert editors.

use super::*;

impl EngineInner {
    pub fn set_insert_param(
        &self,
        track_id: String,
        insert_id: String,
        param_id: String,
        value: f32,
    ) -> Result<(), SphereAudioError> {
        // Held until the command is queued. A project sync replays the
        // journal, stores its graph in the mirror and queues it under this
        // same lock, so the indices resolved below always name the graph that
        // is ahead of this command on the audio thread, and an edit the
        // sync's (older) snapshot lacks is carried into its graph (#365).
        let mut journal = self.insert_param_journal.lock();
        journal.record(&track_id, &insert_id, &param_id, value);

        let mut runtime = self.runtime.lock();
        let Some(track_index) = runtime.tracks.iter().position(|t| t.id == track_id) else {
            return Ok(());
        };
        let Some(insert_index) = runtime.tracks[track_index]
            .inserts
            .iter()
            .position(|i| i.id == insert_id)
        else {
            return Ok(());
        };
        let sample_rate = self.shared.sample_rate.load(Ordering::Relaxed).max(1);
        let (resolved, change) = stage_insert_param(
            &mut runtime.tracks[track_index].inserts[insert_index],
            &param_id,
            value,
            sample_rate,
        );
        drop(runtime);

        if change.is_some() {
            crate::graveyard::prime();
        }

        let result = self.send_command(EngineCommand::SetInsertParam {
            track_index,
            insert_index,
            param_id: resolved,
            value,
            change,
        });
        drop(journal);
        result
    }

    /// The revision of the newest plug-in parameter edit. Read it as a
    /// project snapshot is taken and pass it to [`Self::load_project_as_of`].
    pub fn insert_param_revision(&self) -> u64 {
        self.insert_param_journal.lock().revision
    }

    /// Stage 3b: install (or clear, with `None`) the realtime sink for
    /// `track_id` — the audio callback mixes its external plugin-host DSP output
    /// into the master. Applied between blocks; no realtime allocation.
    pub fn set_plugin_bridge_sink(
        &self,
        insert_id: String,
        sink: Option<std::sync::Arc<dyn crate::plugin_bridge::PluginBridgeSink>>,
    ) -> Result<(), SphereAudioError> {
        {
            let mut sinks = self.plugin_bridge_sinks.lock();
            // The studio re-installs every sink after each project sync — each
            // note edit. The graph already holds an identical one (every graph
            // load copies this map in), and re-installing it costs the audio
            // thread a map insert, a key free and a re-resolve of every insert.
            let unchanged = match (sink.as_ref(), sinks.get(&insert_id)) {
                (Some(next), Some(current)) => std::sync::Arc::ptr_eq(next, current),
                (None, None) => true,
                _ => false,
            };
            if unchanged {
                return Ok(());
            }
            match sink.as_ref() {
                Some(sink) => {
                    sinks.insert(insert_id.clone(), sink.clone());
                }
                None => {
                    sinks.remove(&insert_id);
                }
            }
        }
        // The control-side graph seeds a stream opened later: it needs the
        // sink too, or a reopened device would come up without it now that
        // an unchanged sink is not sent again.
        {
            let mut mirror = self.runtime.lock();
            match sink.as_ref() {
                Some(sink) => {
                    mirror
                        .plugin_bridge_sinks
                        .insert(insert_id.clone(), sink.clone());
                }
                None => {
                    mirror.plugin_bridge_sinks.remove(&insert_id);
                }
            }
            mirror.resolve_bridge_sinks();
        }
        self.send_command(EngineCommand::SetPluginBridgeSink { insert_id, sink })
    }

    /// Install the ARA playback renderers for one track, replacing any previous
    /// set. Pass an empty list to remove them.
    ///
    /// The caller owns ARA: it creates the in-process VST3 instances, binds them
    /// to its `SphereAraHost` document, and assigns their playback regions
    /// before handing them over. The engine only calls `process()` on them and
    /// folds their latency into PDC.
    ///
    /// The renderers are mirrored on the control thread so a later `LoadProject`
    /// can carry them into the rebuilt graph instead of silently dropping them,
    /// which would tear down live ARA bindings on every project sync.
    pub fn set_ara_renderers(
        &self,
        track_id: String,
        renderers: Vec<crate::runtime::RuntimeAraRenderer>,
    ) -> Result<(), SphereAudioError> {
        // The control thread's copy of the graph follows at once. It used to
        // keep the old renderers until the next project sync replaced it, so
        // an instance its host had already let go was destroyed there, on a
        // sync worker thread and after its ARA document was closed — the
        // crash deleting an ARA clip ended in.
        self.runtime
            .lock()
            .set_ara_renderers(&track_id, renderers.clone());
        let mut mirror = self.ara_renderers.lock();
        if renderers.is_empty() {
            mirror.remove(&track_id);
        } else {
            mirror.insert(track_id.clone(), renderers.clone());
        }
        // Lent to an offline render: the graph gets them when they come back.
        // Sent under the lock, so a give-back cannot overtake this change.
        if self.ara_lent.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.send_command(EngineCommand::SetAraRenderers {
            track_id,
            renderers,
        })
    }

    /// Take every ARA playback renderer out of the realtime graph and hand
    /// them to an offline render, by track id.
    ///
    /// An ARA track's clips are rendered by its plug-in instance, bound to the
    /// document, and that one instance must never be processed by the audio
    /// callback and an export worker at once. Blocks the calling control
    /// thread until the callback has dropped them (bounded). Until
    /// [`Self::return_ara_renderers`], the live graph plays those tracks
    /// silent; an export pauses playback anyway.
    pub fn lend_ara_renderers(&self) -> crate::export::AraRendererMap {
        let lent = {
            let mirror = self.ara_renderers.lock();
            self.ara_lent.store(true, Ordering::SeqCst);
            for track_id in mirror.keys() {
                let _ = self.send_command(EngineCommand::SetAraRenderers {
                    track_id: track_id.clone(),
                    renderers: Vec::new(),
                });
            }
            mirror.clone()
        };
        if !lent.is_empty() && !self.wait_for_command_barrier(std::time::Duration::from_millis(500))
        {
            // No open stream, or a stalled callback: the same grace the bridge
            // handoff falls back to.
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        lent
    }

    /// Give the renderers [`Self::lend_ara_renderers`] took back to the
    /// realtime graph — as the mirror has them now, so a change made while
    /// they were lent is kept.
    pub fn return_ara_renderers(&self) {
        let mirror = self.ara_renderers.lock();
        if !self.ara_lent.swap(false, Ordering::SeqCst) {
            return;
        }
        for (track_id, renderers) in mirror.iter() {
            let _ = self.send_command(EngineCommand::SetAraRenderers {
                track_id: track_id.clone(),
                renderers: renderers.clone(),
            });
        }
    }

    pub fn set_bridge_editor_active(
        &self,
        track_id: String,
        active: bool,
    ) -> Result<(), SphereAudioError> {
        let mut runtime = self.runtime.lock();
        if active {
            runtime.bridge_editor_active.insert(track_id);
        } else {
            runtime.bridge_editor_active.remove(&track_id);
        }
        let next = Box::new(runtime.bridge_editor_active.clone());
        drop(runtime);
        crate::graveyard::prime();
        self.send_command(EngineCommand::SetBridgeEditorActive(next))
    }

    pub fn update_insert_param(
        &self,
        track_id: &str,
        insert_id: &str,
        param_id: &str,
        value: f64,
    ) -> Result<(), SphereAudioError> {
        self.set_insert_param(
            track_id.to_string(),
            insert_id.to_string(),
            param_id.to_string(),
            value as f32,
        )
    }

    pub fn open_insert_editor(
        &self,
        track_id: &str,
        insert_id: &str,
        window_id: &str,
        title: &str,
        width: i32,
        height: i32,
    ) -> Result<u64, SphereAudioError> {
        let mut runtime = self.runtime.lock();
        let Some(track) = runtime.tracks.iter_mut().find(|track| track.id == track_id) else {
            return Err(SphereAudioError::NativeError(format!(
                "track not found for insert editor: {track_id}"
            )));
        };
        let Some(insert) = track
            .inserts
            .iter_mut()
            .find(|insert| insert.id == insert_id)
        else {
            return Err(SphereAudioError::NativeError(format!(
                "insert not found for editor: track={track_id} insert={insert_id}"
            )));
        };
        let Some(vst3) = insert.vst3.as_mut() else {
            return Err(SphereAudioError::NativeError(format!(
                "insert has no ready VST3 processor: track={track_id} insert={insert_id}"
            )));
        };
        let handle = vst3
            .open_editor(window_id, title, width, height)
            .ok_or_else(|| {
                SphereAudioError::NativeError(format!(
                    "failed to open VST3 editor: track={track_id} insert={insert_id}"
                ))
            })?;
        eprintln!(
            "[SphereAudio] opened insert editor track={} insert={} handle={} processorHandle=0x{:x}",
            track_id,
            insert_id,
            handle,
            vst3.handle_value()
        );
        Ok(handle)
    }

    pub fn close_insert_editor(
        &self,
        track_id: &str,
        insert_id: &str,
    ) -> Result<(), SphereAudioError> {
        let mut runtime = self.runtime.lock();
        if let Some(vst3) = runtime
            .tracks
            .iter_mut()
            .find(|track| track.id == track_id)
            .and_then(|track| {
                track
                    .inserts
                    .iter_mut()
                    .find(|insert| insert.id == insert_id)
            })
            .and_then(|insert| insert.vst3.as_mut())
        {
            vst3.close_editor();
            eprintln!(
                "[SphereAudio] closed insert editor track={} insert={}",
                track_id, insert_id
            );
        }
        Ok(())
    }

    /// The rate the engine renders at now, and so the rate an ARA instance
    /// created now is prepared at ([`Self::create_ara_processor`]). An
    /// instance made before the device settled on its rate keeps the old one,
    /// and plays its audio at the wrong pitch and speed; the host compares
    /// against this to know to reload it.
    pub fn render_sample_rate(&self) -> u32 {
        self.shared.sample_rate.load(Ordering::Relaxed)
    }

    /// Whether an offline render holds the ARA renderers right now
    /// ([`Self::lend_ara_renderers`]).
    pub fn ara_renderers_lent(&self) -> bool {
        self.ara_lent.load(Ordering::SeqCst)
    }

    /// Clone the live runtime VST3 processor handle for an insert, if it has a
    /// ready native plugin instance. The returned handle is `Arc`-backed and
    /// cheap to clone; holding it keeps the C++ instance alive even across a
    /// runtime rebuild or insert removal (the GUI editor relies on this so it
    /// can attach to / refresh the *existing* instance without re-locking the
    /// engine every frame). Used by the GPUI PluginView for the embedded editor.
    /// Instantiate a VST3 plug-in in this process for ARA hosting.
    ///
    /// ARA is the one plug-in path that does not go through the out-of-process
    /// host: the plug-in reads the clip's samples through host callbacks that
    /// talk straight to project state, which cannot cross the bridge without an
    /// ARA IPC layer. So the instance lives here, beside the engine that will
    /// process it.
    ///
    /// Returns `None` when the module cannot be loaded, or when it exposes no
    /// ARA main factory for `class_id` — an ARA session must never be opened
    /// against a plug-in that will refuse to bind.
    ///
    /// Control thread only. The caller owns the returned handle, binds it to its
    /// ARA document, and installs it with [`Self::set_ara_renderers`].
    pub fn create_ara_processor(
        &self,
        plugin_path: &str,
        class_id: &str,
    ) -> Option<crate::vst3_processor::Vst3RuntimeProcessor> {
        let sample_rate = self.shared.sample_rate.load(Ordering::Relaxed).max(1);
        // Deliberately unactivated: ARA binding has to precede the first
        // `setActive()`, so the caller binds and then calls `activate()`.
        let processor = crate::vst3_processor::Vst3RuntimeProcessor::new_for_ara(
            plugin_path,
            class_id,
            sample_rate,
        )?;
        if !processor.supports_ara() {
            processor.set_destroy_reason("not-ara-capable");
            return None;
        }
        Some(processor)
    }

    pub fn insert_processor(
        &self,
        track_id: &str,
        insert_id: &str,
    ) -> Option<crate::vst3_processor::Vst3RuntimeProcessor> {
        let mut runtime = self.runtime.lock();
        runtime
            .tracks
            .iter_mut()
            .find(|track| track.id == track_id)
            .and_then(|track| {
                track
                    .inserts
                    .iter_mut()
                    .find(|insert| insert.id == insert_id)
            })
            .and_then(|insert| insert.vst3.as_ref().cloned())
    }

    /// What one insert costs and how much delay it adds, for its own editor.
    ///
    /// Traced under `FUTUREBOARD_PLUGIN_VIEW_DEBUG`, once a second: every value
    /// in a plug-in editor's CPU/latency readout crosses a process boundary and
    /// three layers of lookup, and "it reads zero" says nothing about which of
    /// them came up empty.
    ///
    /// Returns `(cpu_share, latency_samples)` where `cpu_share` is the fraction
    /// of one audio block this insert's processing took, smoothed. `None` when
    /// the insert is not in the graph. The latency is the plug-in's own report,
    /// not the host's block buffering around it: a plug-in editor is answering
    /// for the plug-in.
    ///
    /// Control thread only -- it takes the runtime lock.
    pub fn insert_load(&self, track_id: &str, insert_id: &str) -> Option<(f32, u32)> {
        use std::sync::atomic::Ordering;
        let deadline_us = self.shared.callback_deadline_us.load(Ordering::Relaxed);
        let runtime = self.runtime.lock();
        let Some(track) = runtime.tracks.iter().find(|track| track.id == track_id) else {
            insert_load_trace(|| {
                format!(
                    "no runtime track '{track_id}'; known={:?}",
                    runtime.tracks.iter().map(|t| &t.id).collect::<Vec<_>>()
                )
            });
            return None;
        };
        let Some(insert) = track.inserts.iter().find(|insert| insert.id == insert_id) else {
            insert_load_trace(|| {
                format!(
                    "no insert '{insert_id}' on '{track_id}'; known={:?}",
                    track.inserts.iter().map(|i| &i.id).collect::<Vec<_>>()
                )
            });
            return None;
        };
        let bridged = insert.kind_tag == crate::runtime::RuntimeInsertKind::ExternalBridge;
        let sink = insert.bridge_sink.as_ref();
        // A bridged plug-in runs in another process, so the engine's own timing
        // around it measures the shared-memory handshake, not the plug-in. The
        // host measures the real thing around `process()` and publishes it in
        // the region; prefer that, and fall back to what this side saw.
        let share = sink
            .filter(|_| bridged)
            .and_then(|sink| sink.reported_process_load())
            .unwrap_or_else(|| {
                let cpu_us = insert.cpu_us.load(Ordering::Relaxed) as f32;
                if deadline_us > 0 {
                    cpu_us / deadline_us as f32
                } else {
                    0.0
                }
            });
        let latency = if bridged {
            sink.map(|sink| sink.reported_latency_samples())
                .unwrap_or(0)
        } else {
            insert
                .vst3
                .as_ref()
                .map(|vst3| vst3.get_latency_samples().max(0) as u32)
                .unwrap_or(0)
        };
        insert_load_trace(|| {
            format!(
                "'{insert_id}' kind={:?} sink={} host_load={:?} engine_us={} deadline_us={deadline_us}                  -> share={share:.4} latency={latency}",
                insert.kind_tag,
                sink.is_some(),
                sink.and_then(|sink| sink.reported_process_load()),
                insert.cpu_us.load(Ordering::Relaxed),
            )
        });
        Some((share, latency))
    }

    pub fn focus_insert_editor(
        &self,
        track_id: &str,
        insert_id: &str,
    ) -> Result<bool, SphereAudioError> {
        let mut runtime = self.runtime.lock();
        if let Some(vst3) = runtime
            .tracks
            .iter_mut()
            .find(|track| track.id == track_id)
            .and_then(|track| {
                track
                    .inserts
                    .iter_mut()
                    .find(|insert| insert.id == insert_id)
            })
            .and_then(|insert| insert.vst3.as_mut())
        {
            return Ok(vst3.focus_editor());
        }
        Ok(false)
    }

    /// Structured per-insert instantiation status for UI readback (Phase 2b).
    ///
    /// Built-in DSP inserts are always `ready`. Native-plugin inserts are
    /// `ready` only when their `Vst3RuntimeProcessor` instantiated and is live;
    /// a native insert with `ready == false` is a definitive instantiation
    /// failure (the worker attempted it during `load_project` and got `None`).
    /// Used by the native shell to flip `InsertLoadStatus::Failed`.
    pub fn insert_statuses(&self) -> Vec<crate::native::EngineInsertStatus> {
        let runtime = self.runtime.lock();
        runtime
            .tracks
            .iter()
            .flat_map(|track| {
                let track_id = track.id.clone();
                track.inserts.iter().map(move |insert| {
                    let native = insert.kind.eq_ignore_ascii_case("native-plugin");
                    let ready = if native {
                        insert.vst3.as_ref().map(|p| p.is_ready()).unwrap_or(false)
                    } else {
                        true
                    };
                    crate::native::EngineInsertStatus {
                        track_id: track_id.clone(),
                        insert_id: insert.id.clone(),
                        native,
                        ready,
                    }
                })
            })
            .collect()
    }
}
