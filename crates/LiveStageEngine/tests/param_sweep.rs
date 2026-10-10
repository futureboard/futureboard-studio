//! What turning a knob does to the audio thread, for every built-in effect.
//!
//! Each parameter is dragged across its range the way an editor sends it — a
//! new value every block — while the effect plays a tone with some noise. Per
//! parameter it reports the slowest block (apply + process), heap allocations
//! on the audio path (there must be none), and the biggest sample-to-sample
//! jump in the output against the same effect left alone (a click or a zipper
//! shows up as a jump many times the untouched one).
//!
//! ```txt
//! cargo test --release -p livestage-engine --no-default-features --test param_sweep -- --nocapture
//! ```

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::time::Instant;

use livestage_engine::builtin_fx::{BUILTIN_EFFECTS, BuiltinFx, builtin_params};

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
const BLOCK: usize = 128;
/// One drag across the range, in blocks: about a second.
const DRAG_BLOCKS: usize = 375;

/// Two low tones and no noise: smooth, so a step in the gain shows as a kink
/// (a big second difference) where the block starts.
fn signal(n: usize, _seed: &mut u32) -> (f32, f32) {
    let t = n as f64 / SR as f64;
    let tone = (std::f64::consts::TAU * 110.0 * t).sin() * 0.3
        + (std::f64::consts::TAU * 330.0 * t).sin() * 0.1;
    (tone as f32, (tone * 0.8) as f32)
}

#[derive(Default)]
struct Run {
    max_block_us: f64,
    max_apply_us: f64,
    allocs: u64,
    max_jump: f32,
    finite: bool,
}

/// Plays `blocks` blocks; `change(block)` gives the parameter change to apply
/// before that block, if any.
fn run(stem: &str, blocks: usize, change: impl Fn(usize) -> Option<(u32, f32)>) -> Run {
    let mut fx = BuiltinFx::new(stem, SR).expect("a built-in");
    let mut seed = 0x1234_5678;
    let mut left = vec![0.0f32; BLOCK];
    let mut right = vec![0.0f32; BLOCK];
    let mut n = 0;
    // Settle first (fill delay lines, let envelopes arrive).
    for _ in 0..200 {
        for i in 0..BLOCK {
            (left[i], right[i]) = signal(n, &mut seed);
            n += 1;
        }
        fx.process(&mut left, &mut right);
    }
    let mut out = Run {
        finite: true,
        ..Run::default()
    };
    let mut last = [(left[BLOCK - 2], right[BLOCK - 2]), (left[BLOCK - 1], right[BLOCK - 1])];
    for block in 0..blocks {
        for i in 0..BLOCK {
            (left[i], right[i]) = signal(n, &mut seed);
            n += 1;
        }
        COUNT.with(|c| c.set(0));
        COUNTING.with(|c| c.set(true));
        let started = Instant::now();
        if let Some((index, value)) = change(block) {
            fx.apply_wire_param(index, value);
        }
        let applied = started.elapsed();
        fx.process(&mut left, &mut right);
        let total = started.elapsed();
        COUNTING.with(|c| c.set(false));
        out.allocs += COUNT.with(Cell::get);
        out.max_apply_us = out.max_apply_us.max(applied.as_secs_f64() * 1e6);
        out.max_block_us = out.max_block_us.max(total.as_secs_f64() * 1e6);
        // The kink where this block meets the last one, against the biggest
        // kink inside the block: a gain that steps per block makes the first
        // stand out; a smoothed one does not.
        let mut edge = 0.0f32;
        let mut inside = 1.0e-7f32;
        for i in 0..BLOCK {
            if !left[i].is_finite() || !right[i].is_finite() {
                out.finite = false;
            }
            let d2 = |a: f32, b: f32, c: f32| (c - 2.0 * b + a).abs();
            let kink = d2(last[0].0, last[1].0, left[i]).max(d2(last[0].1, last[1].1, right[i]));
            if i < 2 {
                edge = edge.max(kink);
            } else {
                inside = inside.max(kink);
            }
            last = [last[1], (left[i], right[i])];
        }
        out.max_jump = out.max_jump.max(edge / inside);
    }
    out
}

/// Fails on what is wrong on any machine — an allocation on the audio path,
/// a non-finite sample, a dragged control that steps once per block (edge kink
/// over [`MAX_JUMP`] times the in-block one) — and prints the table, with
/// timings, for a person to read (timings depend on the machine and its load,
/// so they are not asserted).
#[test]
fn turning_every_knob() {
    let budget_us = BLOCK as f64 / SR as f64 * 1e6;
    let mut failures = Vec::new();
    println!(
        "{:<12} {:<18} {:>9} {:>9} {:>7} {:>8}  (block budget {:.0} us)",
        "effect", "param", "apply us", "block us", "allocs", "jump x", budget_us
    );
    for info in BUILTIN_EFFECTS {
        let stem = info.stem;
        let still = run(stem, DRAG_BLOCKS, |_| None);
        let base = 1.0;
        println!(
            "{:<12} {:<18} {:>9.1} {:>9.1} {:>7} {:>8.1}",
            stem, "(untouched)", 0.0, still.max_block_us, still.allocs, still.max_jump
        );
        for param in builtin_params(stem) {
            let switch = param.unit == "bool" || param.unit == "enum";
            let (min, max) = (param.min, param.max);
            let swept = run(stem, DRAG_BLOCKS, |block| {
                if switch {
                    // A click every quarter second, through every step.
                    (block % 94 == 0).then(|| {
                        let steps = (max - min).round().max(1.0);
                        (param.index, min + ((block / 94) as f32 % (steps + 1.0)))
                    })
                } else {
                    // Up and back down across the range, a value per block.
                    let phase = block as f32 / DRAG_BLOCKS as f32 * 2.0;
                    let x = if phase < 1.0 { phase } else { 2.0 - phase };
                    Some((param.index, min + (max - min) * x))
                }
            });
            let ratio = swept.max_jump / base;
            // Where the effect's own output is flat against a clip, the
            // in-block kink is ~0 and the ratio means nothing (67Clipper's
            // threshold and input reach the clip right at a block edge).
            let ratio_known = !(stem == "clipper67" && matches!(param.id, "thresholdDb" | "inputDb"));
            if swept.allocs > 0 {
                failures.push(format!("{stem} {}: {} allocations", param.id, swept.allocs));
            }
            if !swept.finite {
                failures.push(format!("{stem} {}: non-finite output", param.id));
            }
            if ratio_known && ratio > MAX_JUMP {
                failures.push(format!("{stem} {}: steps (edge kink x{ratio:.1})", param.id));
            }
            let flag = if swept.allocs > 0 || !swept.finite || swept.max_block_us > budget_us * 0.5
                || ratio > 4.0
            {
                "  <--"
            } else {
                ""
            };
            println!(
                "{:<12} {:<18} {:>9.1} {:>9.1} {:>7} {:>8.1}{}{}",
                stem,
                format!("{}{}", param.id, if switch { "*" } else { "" }),
                swept.max_apply_us,
                swept.max_block_us,
                swept.allocs,
                ratio,
                if swept.finite { "" } else { " NaN" },
                flag
            );
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// An edge kink this many times the biggest in-block one is a step.
const MAX_JUMP: f32 = 4.0;
