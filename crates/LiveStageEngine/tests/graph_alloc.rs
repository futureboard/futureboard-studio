//! The console graph on the audio thread: every strip's processing section
//! running, settings arriving every block, PFL/AFL cues and the monitor bus,
//! mono and stereo buses, sends before and after the fader, matrices,
//! talkback and the oscillator switching and ramping, the monitor source —
//! and not one heap allocation while it plays.
//!
//! ```txt
//! cargo test -p livestage-engine --no-default-features --test graph_alloc
//! ```

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use livestage_engine::OscillatorKind;
use livestage_engine::graph::{
    Dest, Graph, InjectTo, MixFrom, MonitorShared, OscillatorCell, PatchFrom, ProcessorCell, RtBus,
    RtChannel, RtMatrix, RtMatrixSource, RtMonitor, RtOscillator, RtPatch, RtSend, RtStrip,
    RtTalkback, SendGains, SendTap, StripShared, TalkbackCell,
};
use livestage_engine::processing::Processing;

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

/// Every section on, so the processor does all its work.
fn busy() -> Processing {
    let mut p = Processing::default();
    p.hpf.on = true;
    p.gate.on = true;
    p.gate.threshold_db = -30.0;
    p.eq.on = true;
    p.eq.bands[1].gain_db = 6.0;
    p.comp.on = true;
    p.comp.threshold_db = -30.0;
    p.delay.on = true;
    p.delay.ms = 12.0;
    p
}

#[test]
fn a_console_block_allocates_nothing() {
    let mut senders = Vec::new();
    let mut strip = |cue_pre: f32, cue_post: f32| {
        let shared = Arc::new(StripShared::default());
        shared.cue_pre.store(cue_pre);
        shared.cue_post.store(cue_post);
        let (processor, sender) = ProcessorCell::new(SR, &busy());
        senders.push((sender, processor.clone()));
        RtStrip::new(shared, processor, Vec::new(), None)
    };
    let gains = |l: f32, r: f32| Arc::new(SendGains::new(l, r));
    let send_gains = gains(0.5, 0.5);
    let channels = vec![
        RtChannel {
            strip: strip(1.0, 0.0),
            input_left: Some(0),
            input_right: None,
            sends: vec![
                RtSend::new(0, send_gains.clone(), SendTap::BeforeFader),
                RtSend::new(1, gains(0.3, 0.1), SendTap::AfterFader),
            ],
            dest: Dest::Master,
            record_input: true,
        },
        RtChannel {
            strip: strip(0.0, 1.0),
            input_left: Some(1),
            input_right: Some(2),
            sends: vec![RtSend::new(1, gains(0.2, 0.4), SendTap::BeforeFader)],
            dest: Dest::Bus(0),
            record_input: false,
        },
    ];
    let buses = vec![
        RtBus {
            strip: strip(1.0, 0.0),
            dest: Dest::Master,
            stereo: true,
        },
        // A mono aux: folded to (L+R)/2.
        RtBus {
            strip: strip(0.0, 1.0),
            dest: Dest::None,
            stereo: false,
        },
    ];
    let master = strip(0.0, 0.0);
    let matrix_gains = gains(0.7, 0.2);
    let matrices = vec![
        RtMatrix {
            strip: strip(0.0, 1.0),
            stereo: true,
            sources: vec![
                RtMatrixSource::new(MixFrom::Master, matrix_gains.clone()),
                RtMatrixSource::new(MixFrom::Bus(1), gains(0.5, 0.5)),
            ],
        },
        RtMatrix {
            strip: strip(1.0, 0.0),
            stereo: false,
            sources: vec![RtMatrixSource::new(MixFrom::Bus(0), gains(1.0, 1.0))],
        },
    ];
    let monitor_shared = Arc::new(MonitorShared::default());
    monitor_shared.master_feed.store(0.5);
    monitor_shared.source_feed.store(0.5);
    // Its first block crossfades from the master to the mono aux.
    let monitor =
        RtMonitor::new(monitor_shared.clone()).with_source(MixFrom::Bus(1), Some(MixFrom::Master));
    let patches = vec![
        RtPatch {
            from: PatchFrom::Master,
            left: 0,
            right: Some(1),
        },
        RtPatch {
            from: PatchFrom::Monitor,
            left: 2,
            right: Some(3),
        },
        RtPatch {
            from: PatchFrom::Matrix(0),
            left: 4,
            right: Some(5),
        },
        RtPatch {
            from: PatchFrom::Matrix(1),
            left: 6,
            right: None,
        },
    ];
    let talkback = TalkbackCell::new(SR);
    let oscillator = OscillatorCell::new(SR);
    let mut graph = Graph::new(3, 7, channels, buses, master, monitor, patches)
        .with_matrices(matrices)
        .with_talkback(RtTalkback {
            cell: talkback.clone(),
            input: Some(2),
            to: vec![InjectTo::Bus(1), InjectTo::Monitor, InjectTo::Matrix(1)],
        })
        .with_oscillator(RtOscillator {
            cell: oscillator.clone(),
            to: vec![InjectTo::Master, InjectTo::Matrix(0), InjectTo::Bus(0)],
        });

    let frames = 256;
    let input: Vec<f32> = (0..frames * 3)
        .map(|i| ((i as f32) * 0.013).sin() * 0.4)
        .collect();
    let mut output = vec![0.0; frames * 7];
    let mut settings = busy();
    let mut allocations = 0;
    let kinds = [
        OscillatorKind::Sine,
        OscillatorKind::Pink,
        OscillatorKind::White,
    ];
    for block in 0..400 {
        // A knob drag: new settings for every strip, every block, sent
        // from the control side (not counted) …
        settings.comp.ratio = 2.0 + (block % 50) as f32 * 0.2;
        settings.eq.bands[2].gain_db = (block % 30) as f32 * 0.4 - 6.0;
        for (sender, _) in &senders {
            sender.send(settings);
        }
        if block % 100 == 50 {
            monitor_shared.gain.store(0.1);
        }
        // … talk pressed and released, its HPF switched, the oscillator on
        // and off through every kind and level, sends and matrix levels
        // moving …
        talkback.active.store(block % 40 < 25, Ordering::Relaxed);
        talkback.hpf.store(block % 60 < 30, Ordering::Relaxed);
        talkback.level.store(0.5 + (block % 7) as f32 * 0.05);
        oscillator.on.store(block % 90 > 10, Ordering::Relaxed);
        oscillator.set_kind(kinds[(block / 30) % 3]);
        oscillator.hz.store(100.0 + block as f32 * 10.0);
        oscillator.level.store(0.05 + (block % 5) as f32 * 0.01);
        send_gains.store(0.1 * (block % 9) as f32, 0.5);
        matrix_gains.store(0.7, 0.05 * (block % 11) as f32);
        monitor_shared
            .source_feed
            .store(if block % 80 < 40 { 1.0 } else { 0.0 });
        // … and the block itself, counted.
        COUNT.with(|c| c.set(0));
        COUNTING.with(|c| c.set(true));
        graph.process(&input, &mut output);
        COUNTING.with(|c| c.set(false));
        allocations += COUNT.with(Cell::get);
    }
    assert_eq!(allocations, 0, "the audio thread allocated");
    assert!(output.iter().all(|s| s.is_finite()));
    assert!(output.iter().any(|s| s.abs() > 1.0e-4), "something plays");
    for matrix_output in [4, 6] {
        assert!(
            output
                .iter()
                .skip(matrix_output)
                .step_by(7)
                .any(|s| s.abs() > 1.0e-4),
            "matrix output {matrix_output} plays"
        );
    }
    let (l, r) = monitor_shared.meter.take();
    assert!(l > 0.0 && r > 0.0, "the monitor bus carries the cues");
    assert!(talkback.meter.take().0 > 0.0, "talkback was metered");
    for (_, cell) in &senders {
        let meters = cell.take_meters();
        assert!(meters.gate_db.is_finite() && meters.comp_db.is_finite());
    }
}
