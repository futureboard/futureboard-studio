//! Sidechain keys: one track's signal fed to another track's insert.
//!
//! ```txt
//! source track ── inserts ─┬─ fader ─┬─ PDC ── route
//!                          │ (pre)   │ (post)
//!                          ▼         ▼
//!                    RuntimeSidechainTap ──(next callback)──▶ insert key ──▶ plug-in aux input
//! ```
//!
//! # State
//!
//! An insert names its key in its params, `sidechain = { "sourceTrackId": id,
//! "tap": "pre" | "post" }`, so the setting is saved, undone and synced with
//! the insert's other opaque params. [`sidechain_from_params`] reads it when
//! the graph is built and [`RuntimeProject::resolve_sidechains`] resolves the
//! source to an index on the control thread.
//!
//! # Timing
//!
//! Like a loopback input, the key an insert hears this callback is what its
//! source rendered in the **previous** one: every keyed insert copies its
//! source's tap before any track renders, then the taps start the block silent.
//! That keeps the parallel chain batches free of ordering constraints and a
//! source routed into its own key's track a plain delay instead of a hang.
//!
//! Delay compensation then lines the two inputs up: the key left the timeline
//! one period plus its source's path latency ago, the insert's main input its
//! own path latency ago. [`RuntimeProject::update_sidechain_alignment`] delays
//! whichever is early, inside the insert; a delayed main input is latency the
//! insert adds to its track, so the project's compensation plan absorbs it like
//! any plug-in's.

use super::*;

/// The source track publishes its post-insert, pre-fader block.
pub(crate) const SIDECHAIN_TAP_PRE: u8 = 1;
/// The source track publishes its post-fader block (before compensation).
pub(crate) const SIDECHAIN_TAP_POST: u8 = 2;

/// Param key an insert keeps its sidechain route under.
pub const SIDECHAIN_PARAM: &str = "sidechain";

/// Ring length of each alignment delay. A power of two above the largest
/// shift [`MAX_SIDECHAIN_ALIGN_SAMPLES`] plus one block.
const ALIGN_RING_FRAMES: usize = 32_768;

/// Most either input is delayed to line the two up: a little over a third of a
/// second at 48 kHz. A shift beyond it is clamped rather than grown on the
/// audio thread.
pub(crate) const MAX_SIDECHAIN_ALIGN_SAMPLES: i32 = 16_384;

/// One track's published sidechain signal. Empty unless some insert keys from
/// the track; [`RuntimeProject::resolve_sidechains`] sizes it.
#[derive(Debug, Clone, Default)]
pub struct RuntimeSidechainTap {
    /// [`SIDECHAIN_TAP_PRE`] | [`SIDECHAIN_TAP_POST`].
    pub flags: u8,
    pub pre_l: Vec<f32>,
    pub pre_r: Vec<f32>,
    pub post_l: Vec<f32>,
    pub post_r: Vec<f32>,
}

impl RuntimeSidechainTap {
    /// Copy `block_*` into the tap at `stage`. Allocation-free: buffers were
    /// sized on the control thread, and a longer block is truncated.
    #[inline]
    pub(crate) fn capture(&mut self, stage: u8, block_l: &[f32], block_r: &[f32], frames: usize) {
        if self.flags & stage == 0 {
            return;
        }
        let (l, r) = if stage == SIDECHAIN_TAP_PRE {
            (&mut self.pre_l, &mut self.pre_r)
        } else {
            (&mut self.post_l, &mut self.post_r)
        };
        let n = frames
            .min(l.len())
            .min(r.len())
            .min(block_l.len())
            .min(block_r.len());
        l[..n].copy_from_slice(&block_l[..n]);
        r[..n].copy_from_slice(&block_r[..n]);
    }

    /// Start a block silent: a source the graph skips this block publishes
    /// nothing rather than its last block again.
    #[inline]
    pub(crate) fn clear(&mut self, frames: usize) {
        if self.flags == 0 {
            return;
        }
        for buf in [
            &mut self.pre_l,
            &mut self.pre_r,
            &mut self.post_l,
            &mut self.post_r,
        ] {
            let n = frames.min(buf.len());
            buf[..n].fill(0.0);
        }
    }
}

/// A stereo delay ring for one side of the alignment.
#[derive(Debug, Clone, Default)]
pub struct SidechainDelay {
    l: Vec<f32>,
    r: Vec<f32>,
    pos: usize,
}

impl SidechainDelay {
    fn allocate() -> Self {
        Self {
            l: vec![0.0; ALIGN_RING_FRAMES],
            r: vec![0.0; ALIGN_RING_FRAMES],
            pos: 0,
        }
    }

    /// Delay `l`/`r` in place by `delay` samples. Allocation-free; a ring that
    /// was never allocated (or a zero delay) leaves the signal as it is.
    #[inline]
    fn run(&mut self, l: &mut [f32], r: &mut [f32], delay: usize) {
        let len = self.l.len();
        if len == 0 || self.r.len() != len {
            return;
        }
        let mask = len - 1;
        let delay = delay.min(len - 1);
        for i in 0..l.len().min(r.len()) {
            self.l[self.pos] = l[i];
            self.r[self.pos] = r[i];
            let read = (self.pos + len - delay) & mask;
            l[i] = self.l[read];
            r[i] = self.r[read];
            self.pos = (self.pos + 1) & mask;
        }
    }

    fn reset(&mut self) {
        self.l.fill(0.0);
        self.r.fill(0.0);
        self.pos = 0;
    }
}

/// An insert's sidechain route, resolved for the render path.
#[derive(Debug, Clone)]
pub struct RuntimeSidechain {
    /// The source as the project names it.
    pub source_track_id: String,
    /// Post-fader tap instead of pre-fader.
    pub post_fader: bool,
    /// [`Self::source_track_id`] resolved by
    /// [`RuntimeProject::resolve_sidechains`]; `None` when the track is gone,
    /// is the insert's own, or would close a loop. The insert then gets no key.
    pub source_index: Option<usize>,
    /// This block's key, copied from the source's tap at the callback start.
    pub key_l: Vec<f32>,
    pub key_r: Vec<f32>,
    /// Alignment in samples: positive delays the insert's main input (the key
    /// is late), negative delays the key (it is early). Set by
    /// [`RuntimeProject::update_sidechain_alignment`].
    pub align_samples: i32,
    main_delay: SidechainDelay,
    key_delay: SidechainDelay,
}

impl RuntimeSidechain {
    /// Whether this route feeds a key at all.
    #[inline]
    pub fn is_live(&self) -> bool {
        self.source_index.is_some()
    }

    /// Latency the alignment adds to the insert's track: the main-input delay.
    #[inline]
    pub fn added_latency_samples(&self) -> u32 {
        if self.is_live() {
            self.align_samples.max(0) as u32
        } else {
            0
        }
    }

    /// Line this block's main input (`main_*`, in place) and key up. Returns
    /// the key slices, or `None` when the route has no source. Allocation-free.
    #[inline]
    pub fn align_block(
        &mut self,
        main_l: &mut [f32],
        main_r: &mut [f32],
        frames: usize,
    ) -> Option<(&[f32], &[f32])> {
        if !self.is_live() {
            return None;
        }
        let frames = frames
            .min(main_l.len())
            .min(main_r.len())
            .min(self.key_l.len())
            .min(self.key_r.len());
        if self.align_samples > 0 {
            self.main_delay.run(
                &mut main_l[..frames],
                &mut main_r[..frames],
                self.align_samples as usize,
            );
        } else if self.align_samples < 0 {
            self.key_delay.run(
                &mut self.key_l[..frames],
                &mut self.key_r[..frames],
                self.align_samples.unsigned_abs() as usize,
            );
        }
        Some((&self.key_l[..frames], &self.key_r[..frames]))
    }
}

/// Read an insert's sidechain route from its params, or `None` without one.
/// Control thread (graph build).
pub(crate) fn sidechain_from_params(
    params: &HashMap<String, Value>,
) -> Option<Box<RuntimeSidechain>> {
    let route = params.get(SIDECHAIN_PARAM)?.as_object()?;
    let source = route
        .get("sourceTrackId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())?;
    let post_fader = route
        .get("tap")
        .and_then(Value::as_str)
        .is_some_and(|tap| tap.eq_ignore_ascii_case("post"));
    Some(Box::new(RuntimeSidechain {
        source_track_id: source.to_string(),
        post_fader,
        source_index: None,
        key_l: Vec::new(),
        key_r: Vec::new(),
        align_samples: 0,
        main_delay: SidechainDelay::default(),
        key_delay: SidechainDelay::default(),
    }))
}

impl RuntimeProject {
    /// Resolve every insert's sidechain source to a track index, mark the taps
    /// sources publish, and size every buffer the render path touches.
    ///
    /// A source that is the insert's own track, or that the insert's track
    /// already reaches — through its output, a send, a multi-out child or
    /// another accepted sidechain — resolves to nothing: its key would depend
    /// on the track it keys, and aligning the two would chase its own latency
    /// forever. Control thread; allocates.
    pub fn resolve_sidechains(&mut self) {
        let index_of: HashMap<&str, usize> = self
            .tracks
            .iter()
            .enumerate()
            .map(|(index, track)| (track.id.as_str(), index))
            .collect();
        let mut wanted: Vec<(usize, usize, usize)> = Vec::new();
        for (dest, track) in self.tracks.iter().enumerate() {
            for (slot, insert) in track.inserts.iter().enumerate() {
                if let Some(route) = insert.sidechain.as_deref() {
                    if let Some(&source) = index_of.get(route.source_track_id.as_str()) {
                        wanted.push((dest, slot, source));
                    }
                }
            }
        }
        drop(index_of);

        let mut edges: Vec<(usize, usize)> = Vec::new();
        let mut accepted: Vec<(usize, usize, usize)> = Vec::new();
        for (dest, slot, source) in wanted {
            if source == dest || self.audio_reaches(dest, source, &edges) {
                continue;
            }
            edges.push((source, dest));
            accepted.push((dest, slot, source));
        }

        for track in &mut self.tracks {
            track.sidechain_tap.flags = 0;
            for insert in &mut track.inserts {
                if let Some(route) = insert.sidechain.as_deref_mut() {
                    route.source_index = None;
                }
            }
        }
        for (dest, slot, source) in accepted {
            let Some(route) = self.tracks[dest].inserts[slot].sidechain.as_deref_mut() else {
                continue;
            };
            route.source_index = Some(source);
            let stage = if route.post_fader {
                SIDECHAIN_TAP_POST
            } else {
                SIDECHAIN_TAP_PRE
            };
            if route.key_l.len() < DEFAULT_AUDIO_BLOCK_CAPACITY {
                route.key_l = vec![0.0; DEFAULT_AUDIO_BLOCK_CAPACITY];
                route.key_r = vec![0.0; DEFAULT_AUDIO_BLOCK_CAPACITY];
            }
            if route.main_delay.l.is_empty() {
                route.main_delay = SidechainDelay::allocate();
                route.key_delay = SidechainDelay::allocate();
            }
            self.tracks[source].sidechain_tap.flags |= stage;
        }
        for track in &mut self.tracks {
            let tap = &mut track.sidechain_tap;
            let size = |buf: &mut Vec<f32>, on: bool| {
                if on && buf.len() < DEFAULT_AUDIO_BLOCK_CAPACITY {
                    *buf = vec![0.0; DEFAULT_AUDIO_BLOCK_CAPACITY];
                } else if !on {
                    *buf = Vec::new();
                }
            };
            let (pre, post) = (
                tap.flags & SIDECHAIN_TAP_PRE != 0,
                tap.flags & SIDECHAIN_TAP_POST != 0,
            );
            size(&mut tap.pre_l, pre);
            size(&mut tap.pre_r, pre);
            size(&mut tap.post_l, post);
            size(&mut tap.post_r, post);
        }
    }

    /// Hand every keyed insert the block its source published last callback,
    /// then start every tap silent. Runs once per callback before any track
    /// renders. Allocation-free: a scan of resolved indices and slice copies.
    pub fn take_sidechain_keys(&mut self, frames: usize) {
        for dest in 0..self.tracks.len() {
            for slot in 0..self.tracks[dest].inserts.len() {
                let Some((source, post)) = self.tracks[dest].inserts[slot]
                    .sidechain
                    .as_deref()
                    .and_then(|route| Some((route.source_index?, route.post_fader)))
                else {
                    continue;
                };
                if source == dest || source >= self.tracks.len() {
                    continue;
                }
                let (dest_track, source_track) = if dest < source {
                    let (lo, hi) = self.tracks.split_at_mut(source);
                    (&mut lo[dest], &hi[0])
                } else {
                    let (lo, hi) = self.tracks.split_at_mut(dest);
                    (&mut hi[0], &lo[source])
                };
                let Some(route) = dest_track.inserts[slot].sidechain.as_deref_mut() else {
                    continue;
                };
                let tap = &source_track.sidechain_tap;
                let (src_l, src_r) = if post {
                    (&tap.post_l, &tap.post_r)
                } else {
                    (&tap.pre_l, &tap.pre_r)
                };
                let n = frames.min(route.key_l.len()).min(route.key_r.len());
                let copied = n.min(src_l.len()).min(src_r.len());
                route.key_l[..copied].copy_from_slice(&src_l[..copied]);
                route.key_r[..copied].copy_from_slice(&src_r[..copied]);
                route.key_l[copied..n].fill(0.0);
                route.key_r[copied..n].fill(0.0);
            }
        }
        for track in &mut self.tracks {
            track.sidechain_tap.clear(frames);
        }
    }

    /// Recompute each keyed insert's alignment from the current compensation
    /// plan. Returns whether any changed — the caller then replans, since a
    /// main-input delay is latency on the insert's track.
    ///
    /// The key is `period + out(source)` behind the timeline; the insert's main
    /// input is `in(track) + chain before it` behind. Realtime-safe: index
    /// reads and arithmetic only.
    pub(crate) fn update_sidechain_alignment(&mut self, bridge_period_frames: u32) -> bool {
        let graph = &self.latency_graph;
        let mut changed = false;
        for dest in 0..self.tracks.len() {
            let track_in = graph
                .track_output_latency
                .get(dest)
                .copied()
                .unwrap_or(0)
                .saturating_sub(graph.track_block_latency.get(dest).copied().unwrap_or(0));
            let mut before = self.tracks[dest]
                .soundfont_player
                .as_ref()
                .map(RuntimeSoundfontPlayer::latency_samples)
                .unwrap_or(0);
            for insert in self.tracks[dest].inserts.iter_mut() {
                let chain_before = before;
                before = before.max(insert.chain_latency_samples);
                let Some(route) = insert.sidechain.as_deref_mut() else {
                    continue;
                };
                let Some(source) = route.source_index else {
                    continue;
                };
                let key_lag = i64::from(bridge_period_frames)
                    + i64::from(graph.track_output_latency.get(source).copied().unwrap_or(0));
                let main_lag = i64::from(track_in) + i64::from(chain_before);
                let align = (key_lag - main_lag).clamp(
                    -i64::from(MAX_SIDECHAIN_ALIGN_SAMPLES),
                    i64::from(MAX_SIDECHAIN_ALIGN_SAMPLES),
                ) as i32;
                if align != route.align_samples {
                    // A new shift replays whatever the rings held at the old
                    // one; start them clean instead.
                    route.main_delay.reset();
                    route.key_delay.reset();
                    route.align_samples = align;
                    changed = true;
                }
            }
        }
        changed
    }

    /// Whether any insert in the graph has a live sidechain.
    pub fn has_live_sidechains(&self) -> bool {
        self.tracks.iter().any(|track| {
            track.inserts.iter().any(|insert| {
                insert
                    .sidechain
                    .as_deref()
                    .is_some_and(RuntimeSidechain::is_live)
            })
        })
    }
}
