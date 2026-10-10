//! Playback on the audio thread: a take's files popped from their rings
//! into the channels they feed, virtual soundcheck switching (and
//! crossfading) on and off, the transport playing, pausing, locating and
//! looping while the reader thread decodes — and not one heap allocation on
//! the audio thread while it plays.
//!
//! ```txt
//! cargo test -p livestage-engine --no-default-features --test playback_alloc
//! ```

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use livestage_engine::graph::{
    Dest, Graph, MonitorShared, PatchFrom, PlaybackTap, ProcessorCell, RtChannel, RtMonitor,
    RtPatch, RtPlayback, RtStrip, StripShared,
};
use livestage_engine::playback::{Player, Transport, probe_take};
use livestage_engine::processing::Processing;
use sphere_encoder::{
    AudioEncodeOptions, AudioEncodeSpec, AudioFileFormat, AudioSampleFormat, create_encoder,
};

struct Counting;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static COUNT: Cell<u64> = const { Cell::new(0) };
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.with(Cell::get) {
            COUNT.with(|c| c.set(c.get() + 1));
        }
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if COUNTING.with(Cell::get) {
            COUNT.with(|c| c.set(c.get() + 1));
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

const SR: u32 = 48_000;

/// A take of a mono WAV and a stereo FLAC, two seconds of tones.
fn write_take() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("livestage-playback-alloc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let frames = SR as usize * 2;
    for (name, channels, format, sample_format) in [
        (
            "Kick.wav",
            1u16,
            AudioFileFormat::Wav,
            AudioSampleFormat::F32,
        ),
        (
            "Keys.flac",
            2,
            AudioFileFormat::Flac,
            AudioSampleFormat::I24,
        ),
    ] {
        let mut encoder = create_encoder(
            &dir.join(name),
            AudioEncodeSpec {
                sample_rate: SR,
                channels,
                sample_format,
            },
            AudioEncodeOptions {
                format,
                ..AudioEncodeOptions::default()
            },
        )
        .unwrap();
        let samples: Vec<f32> = (0..frames)
            .flat_map(|f| {
                (0..channels).map(move |c| ((f as f32) * 0.01 * f32::from(c + 1)).sin() * 0.5)
            })
            .collect();
        encoder.write_interleaved_f32(&samples).unwrap();
        encoder.finalize().unwrap();
    }
    dir
}

#[test]
fn playback_and_virtual_soundcheck_allocate_nothing() {
    let take = write_take();
    let files = probe_take(&take).unwrap();
    let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["Keys.flac", "Kick.wav"]);
    let player = Player::open(files, SR).unwrap();

    let strip = || {
        let (processor, _) = ProcessorCell::new(SR, &Processing::default());
        RtStrip::new(
            Arc::new(StripShared::default()),
            processor,
            Vec::new(),
            None,
        )
    };
    let channels = vec![
        // Mono, fed by the stereo file: (L+R)/2.
        RtChannel {
            strip: strip(),
            input_left: Some(0),
            input_right: None,
            sends: Vec::new(),
            dest: Dest::Master,
            record_input: true,
        },
        // Stereo, fed by the mono file: both sides.
        RtChannel {
            strip: strip(),
            input_left: Some(0),
            input_right: Some(1),
            sends: Vec::new(),
            dest: Dest::Master,
            record_input: true,
        },
        // No file: always live.
        RtChannel {
            strip: strip(),
            input_left: Some(1),
            input_right: None,
            sends: Vec::new(),
            dest: Dest::Master,
            record_input: true,
        },
    ];
    let patches = vec![RtPatch {
        from: PatchFrom::Master,
        left: 0,
        right: Some(1),
    }];
    let monitor = RtMonitor::new(Arc::new(MonitorShared::default()));
    let mut graph = Graph::new(2, 2, channels, Vec::new(), strip(), monitor, patches)
        .with_playback(RtPlayback {
            cell: player.cell().clone(),
            taps: vec![
                Some(PlaybackTap {
                    track: 0,
                    stereo: false,
                }),
                Some(PlaybackTap {
                    track: 1,
                    stereo: true,
                }),
                None,
            ],
        });

    let frames = 256;
    let input: Vec<f32> = (0..frames * 2)
        .map(|i| ((i as f32) * 0.02).sin() * 0.1)
        .collect();
    let mut output = vec![0.0; frames * 2];

    // Filled from the start before anything is counted.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !player.is_ready() {
        assert!(
            Instant::now() < deadline,
            "the reader never filled the rings"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    player.set_loop(true);
    player.transport(Transport::Play);

    let mut allocations = 0;
    let mut peak = 0.0f32;
    for block in 0..600 {
        // The control side (not counted): virtual soundcheck on and off,
        // locates, a pause, the loop …
        match block % 150 {
            10 => player.set_virtual_soundcheck(true),
            60 => player.locate(1.9),
            90 => player.transport(Transport::Pause),
            100 => player.transport(Transport::Play),
            120 => player.set_virtual_soundcheck(false),
            130 => player.set_virtual_soundcheck(true),
            140 => player.locate(0.25),
            _ => {}
        }
        if block == 450 {
            player.set_loop(false);
            player.transport(Transport::Stop);
            player.transport(Transport::Play);
        }
        // … and the block itself, counted.
        COUNT.with(|c| c.set(0));
        COUNTING.with(|c| c.set(true));
        graph.process(&input, &mut output);
        COUNTING.with(|c| c.set(false));
        allocations += COUNT.with(Cell::get);
        peak = output.iter().fold(peak, |p, s| p.max(s.abs()));
        player.poll();
        // Roughly real time, so the reader keeps up as it would.
        std::thread::sleep(Duration::from_micros(1500));
    }
    assert_eq!(allocations, 0, "the audio thread allocated");
    assert!(output.iter().all(|s| s.is_finite()));
    // Live, the three inputs sum to at most 0.3.
    assert!(peak > 0.45, "the take was heard: {peak}");
    let status = player.status();
    assert_eq!(status.error, None);
    drop(graph);
    drop(player);
    let _ = std::fs::remove_dir_all(&take);
}
