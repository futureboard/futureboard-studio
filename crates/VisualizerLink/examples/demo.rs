//! Publishes a synthetic session on the visualizer link, so the native
//! visualizer app can be run and checked without Studio.
//!
//! ```text
//! cargo run -p visualizer_link --example demo [-- --seconds 30]
//! ```
//!
//! Sixteen tracks with moving levels, a playing transport, a two-tone chord
//! with a slow sweep on the master ring, and a per-track tone on the track
//! ring for whichever track the app asks for. Refuses to start while Studio
//! (or another demo) is publishing.

use std::f32::consts::TAU;
use std::time::{Duration, Instant};

use visualizer_link::{
    Frame, Levels, TrackEntry, Writer, fill_name, flags, kind, track_key, transport,
};

const RATE: u32 = 48_000;
const BLOCK: usize = 480;

struct DemoTrack {
    id: String,
    name: &'static str,
    kind: u8,
    color: u32,
    depth: u8,
    /// Tone the track ring plays when this track is requested.
    hz: f32,
}

fn tracks() -> Vec<DemoTrack> {
    let rows: [(&str, u8, u32, u8, f32); 16] = [
        ("Drums", kind::GROUP, 0xe0_5a_47, 0, 0.0),
        ("Kick", kind::AUDIO, 0xe0_7a_47, 1, 55.0),
        ("Snare", kind::AUDIO, 0xe0_9a_47, 1, 190.0),
        ("Hi-Hat", kind::AUDIO, 0xe0_ba_47, 1, 7_000.0),
        ("Bass", kind::INSTRUMENT, 0x47_a0_e0, 0, 82.4),
        ("Keys", kind::INSTRUMENT, 0x6a_47_e0, 0, 261.6),
        ("Pad", kind::INSTRUMENT, 0x9a_47_e0, 0, 392.0),
        ("Lead", kind::MIDI, 0xc0_47_e0, 0, 880.0),
        ("Gtr L", kind::AUDIO, 0x47_e0_8a, 0, 329.6),
        ("Gtr R", kind::AUDIO, 0x47_e0_b0, 0, 493.9),
        ("Vox", kind::AUDIO, 0xe0_47_8a, 0, 440.0),
        ("Vox Dbl", kind::AUDIO, 0xe0_47_aa, 0, 659.3),
        ("FX Bus", kind::BUS, 0x88_88_88, 0, 1_200.0),
        ("Reverb", kind::RETURN, 0x5a_c8_c8, 0, 2_400.0),
        ("Delay", kind::RETURN, 0x5a_a8_c8, 0, 3_600.0),
        ("Video", kind::VIDEO, 0x60_60_60, 0, 0.0),
    ];
    rows.iter()
        .enumerate()
        .map(|(i, (name, kind, color, depth, hz))| DemoTrack {
            id: format!("demo-track-{i}"),
            name,
            kind: *kind,
            color: *color,
            depth: *depth,
            hz: *hz,
        })
        .collect()
}

fn main() {
    let seconds = std::env::args()
        .skip_while(|arg| arg != "--seconds")
        .nth(1)
        .and_then(|v| v.parse::<u64>().ok());
    let mut writer = match Writer::create() {
        Ok(writer) => writer,
        Err(error) => {
            eprintln!("demo: cannot publish ({error}); is Studio running?");
            std::process::exit(1);
        }
    };
    let audio = writer.audio();
    audio.set_sample_rate(RATE);
    let demo = tracks();
    let keys: Vec<u64> = demo.iter().map(|t| track_key(&t.id)).collect();
    println!(
        "demo: publishing {} tracks on {}",
        demo.len(),
        visualizer_link::REGION_NAME
    );

    let started = Instant::now();
    let mut next = started;
    let mut sample = 0u64;
    let mut tapped = 0u64;
    let (mut left, mut right) = (vec![0.0f32; BLOCK], vec![0.0f32; BLOCK]);
    let mut entries = vec![TrackEntry::default(); demo.len()];
    let mut frame_due = started;
    loop {
        let elapsed = started.elapsed();
        if seconds.is_some_and(|s| elapsed >= Duration::from_secs(s)) {
            break;
        }
        writer.stamp_heartbeat();

        // Master: A3 + E4 with a slow sweep, panned apart.
        let sweep_hz = 200.0 + 4_000.0 * (0.5 + 0.5 * (elapsed.as_secs_f32() * 0.2).sin());
        for i in 0..BLOCK {
            let t = (sample + i as u64) as f32 / RATE as f32;
            let a = (TAU * 220.0 * t).sin() * 0.25;
            let e = (TAU * 329.6 * t).sin() * 0.2;
            let s = (TAU * sweep_hz * t).sin() * 0.1;
            left[i] = a + s * 0.3 + e * 0.6;
            right[i] = e + s * 0.7 + a * 0.6;
        }
        audio.write_master_audio(&left, &right);

        // Track: the requested track's tone.
        let requested = writer.requested_track_key();
        if requested != tapped {
            tapped = requested;
            writer.set_tapped_track_key(tapped);
        }
        if let Some(index) = keys.iter().position(|k| *k == tapped) {
            let hz = demo[index].hz;
            for i in 0..BLOCK {
                let t = (sample + i as u64) as f32 / RATE as f32;
                let v = if hz > 0.0 {
                    (TAU * hz * t).sin() * 0.4
                } else {
                    0.0
                };
                left[i] = v;
                right[i] = v * 0.8;
            }
            audio.write_track_audio(&left, &right);
        }
        sample += BLOCK as u64;

        if Instant::now() >= frame_due {
            frame_due += Duration::from_millis(16);
            let t = elapsed.as_secs_f32();
            for (i, (entry, track)) in entries.iter_mut().zip(&demo).enumerate() {
                let phase = t * (0.7 + i as f32 * 0.13) + i as f32;
                let level = if track.hz > 0.0 {
                    (0.15 + 0.6 * (0.5 + 0.5 * phase.sin())).powf(1.5)
                } else {
                    0.0
                };
                let pan = (i as f32 * 0.37).sin() * 0.6;
                let mut track_flags = 0;
                if i == 10 {
                    track_flags |= flags::ARMED;
                }
                if i == 6 {
                    track_flags |= flags::MUTED;
                }
                if keys[i] == tapped {
                    track_flags |= flags::SELECTED;
                }
                if level > 0.97 {
                    track_flags |= flags::CLIPPED;
                }
                *entry = TrackEntry {
                    key: keys[i],
                    kind: track.kind,
                    flags: track_flags,
                    depth: track.depth,
                    color: track.color,
                    volume_db: -6.0 + 6.0 * (i as f32 * 0.9).cos(),
                    pan,
                    levels: Levels {
                        peak_l: level * (1.0 - pan.max(0.0)),
                        peak_r: level * (1.0 + pan.min(0.0)),
                        rms_l: level * 0.5 * (1.0 - pan.max(0.0)),
                        rms_r: level * 0.5 * (1.0 + pan.min(0.0)),
                    },
                    ..TrackEntry::default()
                };
                fill_name(&mut entry.name, track.name);
            }
            let beats = f64::from(t) * 2.0;
            let mut frame = Frame {
                position_beats: beats,
                position_seconds: f64::from(t),
                bpm: 120.0,
                time_sig_num: 4,
                time_sig_den: 4,
                transport: transport::PLAYING | transport::LOOPING,
                master: Levels {
                    peak_l: 0.55,
                    peak_r: 0.6,
                    rms_l: 0.25,
                    rms_r: 0.27,
                },
                monitor: Levels {
                    peak_l: 0.5,
                    peak_r: 0.55,
                    ..Levels::default()
                },
                master_volume_db: 0.0,
                selected_track_key: tapped,
                ..Frame::default()
            };
            fill_name(&mut frame.project_name, "Visualizer Demo");
            writer.write_frame(&frame, &entries);
        }

        next += Duration::from_secs_f64(BLOCK as f64 / f64::from(RATE));
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        }
    }
    println!("demo: done");
}
