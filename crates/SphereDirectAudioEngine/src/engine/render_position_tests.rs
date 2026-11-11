use super::SharedState;
use std::sync::atomic::Ordering;

fn shared_at(position: u64) -> SharedState {
    let shared = SharedState::default();
    shared.sample_rate.store(48_000, Ordering::Relaxed);
    shared.position_samples.store(position, Ordering::Relaxed);
    shared
}

#[test]
fn nothing_rendered_yet_reads_the_position() {
    let shared = shared_at(10_000);
    assert_eq!(shared.render_position_now(), 10_000);
}

#[test]
fn between_callbacks_it_reads_inside_the_last_block() {
    let shared = shared_at(10_000);
    shared.note_output_block(10_000, 4_800);
    shared.position_samples.store(14_800, Ordering::Relaxed);
    let now = shared.render_position_now();
    // Not the next block's start, which is what a stamp used to read.
    assert!((10_000..14_800).contains(&now), "{now}");
}

#[test]
fn a_seek_since_the_block_reads_the_new_position() {
    let shared = shared_at(10_000);
    shared.note_output_block(10_000, 512);
    shared.position_samples.store(96_000, Ordering::Relaxed);
    assert_eq!(shared.render_position_now(), 96_000);
}

#[test]
fn an_overdue_callback_reads_the_position() {
    let shared = shared_at(10_000);
    shared.note_output_block(10_000, 256);
    shared.position_samples.store(10_256, Ordering::Relaxed);
    // Two blocks of 256 frames at 48 kHz are under 11 ms.
    std::thread::sleep(std::time::Duration::from_millis(25));
    assert_eq!(shared.render_position_now(), 10_256);
}
