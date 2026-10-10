//! The strip processor's audio-thread calls never touch the heap: `set`,
//! `process`, `settle`, `reset` and `meters`, through every switch and odd
//! block lengths, mono and stereo.
//!
//! ```txt
//! cargo test -p livestage-engine --no-default-features --test processing_alloc
//! ```

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use livestage_engine::processing::{EqKind, Processing, ProcessingOrder, StripProcessor};

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

#[test]
fn the_audio_path_never_allocates() {
    let kinds = [EqKind::LowShelf, EqKind::Bell, EqKind::HighShelf];
    for sample_rate in [22_050, 48_000, 192_000] {
        let mut strip = StripProcessor::new(sample_rate);
        let mut left = vec![0.0f32; 1024];
        let mut right = vec![0.0f32; 1024];
        let mut p = Processing::default();
        let mut n = 0usize;

        COUNT.with(|c| c.set(0));
        COUNTING.with(|c| c.set(true));
        for block in 0..3_000usize {
            let x = (block % 200) as f32 / 200.0;
            p.hpf.on = block % 300 < 200;
            p.hpf.hz = 20.0 + 580.0 * x;
            p.hpf.slope_db = [12, 18, 24][(block / 50) % 3];
            p.gate.on = block % 400 < 300;
            p.gate.threshold_db = -80.0 + 80.0 * x;
            p.gate.range_db = -80.0 + 60.0 * x;
            p.eq.on = block % 500 < 450;
            for (i, band) in p.eq.bands.iter_mut().enumerate() {
                band.kind = kinds[(block / 70 + i) % 3];
                band.hz = 20.0 + 19_980.0 * x;
                band.gain_db = if block % 600 < 100 {
                    0.0
                } else {
                    -18.0 + 36.0 * x
                };
                band.q = 0.1 + 9.9 * x;
            }
            p.comp.on = block % 350 < 250;
            p.comp.threshold_db = -60.0 + 60.0 * x;
            p.comp.ratio = 1.0 + 19.0 * x;
            p.comp.knee_db = 24.0 * x;
            p.comp.makeup_db = 12.0 * x;
            p.delay.on = block % 250 < 200;
            p.delay.ms = 1_000.0 * x;
            p.order = if block % 120 < 60 {
                ProcessingOrder::EqThenComp
            } else {
                ProcessingOrder::CompThenEq
            };
            strip.set(&p);
            if block % 777 == 0 {
                strip.settle();
            }
            if block % 1_111 == 500 {
                strip.reset();
            }

            let len = 1 + (block * 37) % 1024;
            for i in 0..len {
                let t = (n + i) as f32 / sample_rate as f32;
                left[i] = (t * 700.0).sin() * 0.5;
                right[i] = (t * 450.0).cos() * 0.4;
            }
            n += len;
            let stereo = block % 3 != 0;
            strip.process(&mut left[..len], &mut right[..len], stereo);
            let _ = strip.meters();
        }
        COUNTING.with(|c| c.set(false));
        let allocations = COUNT.with(Cell::get);
        assert_eq!(
            allocations, 0,
            "{sample_rate} Hz: allocations on the audio path"
        );
        assert!(left.iter().chain(&right).all(|x| x.is_finite()));
    }
}
