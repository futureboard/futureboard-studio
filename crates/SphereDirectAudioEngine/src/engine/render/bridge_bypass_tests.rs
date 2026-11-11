use super::apply_bridge_insert_output;

#[test]
fn effect_with_fresh_block_replaces_dry_signal() {
    let mut block_l = vec![1.0, 1.0, 1.0, 1.0];
    let mut block_r = vec![1.0, 1.0, 1.0, 1.0];
    let scratch_l = vec![0.5, 0.25, 0.0, -0.75];
    let scratch_r = vec![-0.5, 0.0, 0.25, 0.75];
    apply_bridge_insert_output(true, 4, &mut block_l, &mut block_r, &scratch_l, &scratch_r);
    assert_eq!(block_l, scratch_l, "effect output replaces the dry block");
    assert_eq!(block_r, scratch_r);
}

#[test]
fn effect_not_ready_passes_dry_signal_through() {
    // got == 0: the host produced no fresh block. An effect must BYPASS —
    // the dry input is left untouched, never silenced, never a stale replay.
    let mut block_l = vec![0.3, -0.4, 0.5, -0.6];
    let mut block_r = vec![-0.3, 0.4, -0.5, 0.6];
    let dry_l = block_l.clone();
    let dry_r = block_r.clone();
    let scratch_l = vec![9.0, 9.0, 9.0, 9.0]; // must be ignored
    let scratch_r = vec![9.0, 9.0, 9.0, 9.0];
    let (pl, pr) =
        apply_bridge_insert_output(true, 0, &mut block_l, &mut block_r, &scratch_l, &scratch_r);
    assert_eq!(block_l, dry_l, "effect keeps the dry signal when not ready");
    assert_eq!(block_r, dry_r);
    assert_eq!((pl, pr), (0.0, 0.0));
}

#[test]
fn instrument_not_ready_contributes_silence() {
    // got == 0: an instrument must add nothing — the accumulator block is
    // left as-is (whatever else summed into it), i.e. this insert is silent.
    let mut block_l = vec![0.2, 0.2, 0.2, 0.2];
    let mut block_r = vec![0.1, 0.1, 0.1, 0.1];
    let before_l = block_l.clone();
    let before_r = block_r.clone();
    let scratch_l = vec![9.0, 9.0, 9.0, 9.0];
    let scratch_r = vec![9.0, 9.0, 9.0, 9.0];
    let (pl, pr) =
        apply_bridge_insert_output(false, 0, &mut block_l, &mut block_r, &scratch_l, &scratch_r);
    assert_eq!(block_l, before_l, "instrument adds silence when not ready");
    assert_eq!(block_r, before_r);
    assert_eq!((pl, pr), (0.0, 0.0));
}

#[test]
fn instrument_with_fresh_block_sums_into_accumulator() {
    let mut block_l = vec![0.2, 0.2, 0.2];
    let mut block_r = vec![0.1, 0.1, 0.1];
    let scratch_l = vec![0.5, -0.3, 0.0];
    let scratch_r = vec![0.0, 0.4, -0.6];
    apply_bridge_insert_output(false, 3, &mut block_l, &mut block_r, &scratch_l, &scratch_r);
    let approx = |a: &[f32], b: &[f32]| a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-5);
    assert!(approx(&block_l, &[0.7, -0.1, 0.2]), "got {block_l:?}");
    assert!(approx(&block_r, &[0.1, 0.5, -0.5]), "got {block_r:?}");
}

#[test]
fn effect_partial_block_keeps_full_dry() {
    // got < frames: refuse the wet|dry splice — leave the whole dry block.
    let mut block_l = vec![1.0, 1.0, 1.0, 1.0];
    let mut block_r = vec![1.0, 1.0, 1.0, 1.0];
    let scratch_l = vec![0.5, 0.5, 0.0, 0.0];
    let scratch_r = vec![0.5, 0.5, 0.0, 0.0];
    apply_bridge_insert_output(true, 2, &mut block_l, &mut block_r, &scratch_l, &scratch_r);
    assert_eq!(
        block_l,
        vec![1.0, 1.0, 1.0, 1.0],
        "partial effect must not splice"
    );
    assert_eq!(block_r, vec![1.0, 1.0, 1.0, 1.0]);
}

#[test]
fn instrument_partial_block_only_sums_read_frames() {
    let mut block_l = vec![1.0, 1.0, 1.0, 1.0];
    let mut block_r = vec![1.0, 1.0, 1.0, 1.0];
    let scratch_l = vec![0.5, 0.5, 9.0, 9.0];
    let scratch_r = vec![0.25, 0.25, 9.0, 9.0];
    apply_bridge_insert_output(false, 2, &mut block_l, &mut block_r, &scratch_l, &scratch_r);
    assert_eq!(block_l, vec![1.5, 1.5, 1.0, 1.0]);
    assert_eq!(block_r, vec![1.25, 1.25, 1.0, 1.0]);
}
