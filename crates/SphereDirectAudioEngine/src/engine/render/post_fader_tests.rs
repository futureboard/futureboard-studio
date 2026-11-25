//! Post-fader inserts: where they run in a channel, and what the latency plan
//! makes of them.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use super::*;
use crate::engine::bridge_insert_tests::{bridge_effect_track, bridge_effect_track_with_id};
use crate::plugin_bridge::PluginBridgeSink;

const FRAMES: usize = 8;

/// Records the peak of each dry block it is handed and never answers: a
/// missed block keeps the dry signal, so the insert passes audio through and
/// the test sees exactly what reached it.
#[derive(Debug, Default)]
struct InputPeakSink {
    peak_bits: AtomicU32,
    writes: AtomicU64,
}

impl InputPeakSink {
    fn peak(&self) -> f32 {
        f32::from_bits(self.peak_bits.load(Ordering::Acquire))
    }

    fn writes(&self) -> u64 {
        self.writes.load(Ordering::Acquire)
    }
}

impl PluginBridgeSink for InputPeakSink {
    fn dsp_ready(&self) -> bool {
        true
    }

    fn read_output(&self, _: &mut [f32], _: &mut [f32], _: usize) -> usize {
        0
    }

    fn push_midi(&self, _: u8, _: u8, _: u8, _: u32) {}

    fn write_input(&self, left: &[f32], right: &[f32], frames: usize) {
        let peak = left[..frames]
            .iter()
            .chain(&right[..frames])
            .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
        self.peak_bits.store(peak.to_bits(), Ordering::Release);
        self.writes.fetch_add(1, Ordering::AcqRel);
    }

    fn request_block(&self, _: u32) {}

    fn reported_latency_samples(&self) -> u32 {
        0
    }
}

fn transport() -> RuntimeTransportContext {
    RuntimeTransportContext::default()
}

/// A track at `volume` with one pre-fader and one post-fader insert, each
/// listening through its own sink.
fn track_with_pre_and_post(volume: f32) -> (RuntimeTrack, Arc<InputPeakSink>, Arc<InputPeakSink>) {
    let pre = Arc::new(InputPeakSink::default());
    let post = Arc::new(InputPeakSink::default());
    let mut track = bridge_effect_track(1.0);
    track.volume = volume;
    track.inserts[0].bridge_sink = Some(pre.clone());
    let mut late = bridge_effect_track_with_id("post");
    late.post_fader = true;
    late.bridge_sink = Some(post.clone());
    track.inserts.push(late);
    (track, pre, post)
}

#[test]
fn the_chain_leaves_a_post_fader_insert_for_after_the_fader() {
    let (mut track, pre, post) = track_with_pre_and_post(0.5);
    apply_track_chain_block(&mut track, FRAMES, transport());
    assert!(
        (pre.peak() - 1.0).abs() < 1e-6,
        "pre-fader insert hears the dry signal"
    );
    assert_eq!(
        post.writes(),
        0,
        "the chain must not run the post-fader insert"
    );
}

#[test]
fn a_post_fader_insert_hears_the_fader() {
    let (mut track, pre, post) = track_with_pre_and_post(0.5);
    apply_track_chain_block(&mut track, FRAMES, transport());
    apply_fader(&mut track, FRAMES, 0.0, false);
    apply_post_fader_inserts(&mut track, FRAMES, transport());
    let (pan_l, pan_r) = pan_gains(0.0);
    let expected = 0.5 * pan_l.max(pan_r);
    assert_eq!(pre.writes(), 1);
    assert_eq!(post.writes(), 1);
    assert!(
        (post.peak() - expected).abs() < 1e-6,
        "post-fader insert hears {} — expected the faded {expected}",
        post.peak()
    );
}

#[test]
fn a_track_without_post_fader_inserts_runs_none_after_the_fader() {
    let (mut track, pre, _) = track_with_pre_and_post(0.5);
    track.inserts.truncate(1);
    apply_track_chain_block(&mut track, FRAMES, transport());
    apply_post_fader_inserts(&mut track, FRAMES, transport());
    assert_eq!(pre.writes(), 1, "the chain insert runs once, in the chain");
}

fn send_to(target: &str, pre_fader: bool) -> crate::runtime::RuntimeSend {
    crate::runtime::RuntimeSend {
        id: format!("send-{target}"),
        return_track_id: target.to_string(),
        return_track_index: None,
        level: 1.0,
        enabled: true,
        pre_fader,
        level_lane: None,
        enabled_lane: None,
        pdc: crate::latency_graph::PdcDelayLine::default(),
        wdw: false,
    }
}

/// A track whose only insert runs after the fader with `latency` samples.
fn track_with_post_latency(id: &str, latency: u32) -> RuntimeTrack {
    let mut track = bridge_effect_track(0.0);
    track.id = id.to_string();
    track.inserts[0].post_fader = true;
    track.inserts[0].chain_latency_samples = latency;
    track.plugin_latency_samples = latency;
    track
}

#[test]
fn post_fader_latency_counts_only_inserts_after_the_fader() {
    let mut track = track_with_post_latency("a", 256);
    assert_eq!(
        crate::latency_graph::post_fader_latency_samples(&track),
        256
    );
    track.inserts[0].post_fader = false;
    assert_eq!(crate::latency_graph::post_fader_latency_samples(&track), 0);
}

/// A pre-fader send taps ahead of a latent post-fader insert, so it arrives
/// at the return that much sooner and is delayed to match; a post-fader send
/// taps after it and needs no delay.
#[test]
fn a_pre_fader_send_is_aligned_without_the_post_fader_latency() {
    let mut source = track_with_post_latency("src", 512);
    source.sends = vec![send_to("verb", true)];
    let mut other = track_with_post_latency("other", 512);
    other.sends = vec![send_to("verb", false)];
    let mut verb = bridge_effect_track(0.0);
    verb.id = "verb".to_string();
    verb.track_type = "return".to_string();
    verb.inserts.clear();
    let mut master = bridge_effect_track(0.0);
    master.id = "master".to_string();
    master.track_type = "master".to_string();
    master.inserts.clear();
    let mut tracks = vec![source, other, verb, master];
    let audio_graph = crate::audio_graph::plan_runtime_audio_graph(&tracks).unwrap();
    let latency = crate::latency_graph::plan_runtime_latency_graph(&mut tracks, &audio_graph, true);

    // Both feeds arrive at the return together: the post-fader send at 512,
    // the pre-fader one tapped at 0 and delayed by 512.
    assert_eq!(latency.send_pdc_delay[0], vec![512]);
    assert_eq!(latency.send_pdc_delay[1], vec![0]);
}

/// A VCA's gain rides on the channel's own fader: both multiply the block.
#[test]
fn a_vca_gain_scales_the_channel_with_its_fader() {
    let mut track = bridge_effect_track(1.0);
    track.inserts.clear();
    track.volume = 0.5;
    track.vca_gain = 0.5;
    apply_fader(&mut track, FRAMES, 0.0, false);
    let (pan_l, _) = pan_gains(0.0);
    let expected = 0.25 * pan_l;
    assert!(
        (track.block_l[0] - expected).abs() < 1e-6,
        "fader 0.5 under a VCA at 0.5 plays {} — expected {expected}",
        track.block_l[0]
    );

    track.block_l.fill(1.0);
    track.block_r.fill(1.0);
    track.vca_gain = 0.0;
    apply_fader(&mut track, FRAMES, 0.0, false);
    assert_eq!(track.block_l[0], 0.0, "a muted VCA silences its members");
}
