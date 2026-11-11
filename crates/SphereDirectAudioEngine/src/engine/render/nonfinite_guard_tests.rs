use super::silence_nonfinite_block;

/// One NaN from a plug-in used to turn the whole master sum into NaN,
/// which the device plays as silence. The channel that made it goes
/// quiet for the block instead.
#[test]
fn a_block_with_nan_or_infinity_is_silenced() {
    let mut l = vec![0.5, f32::NAN, 0.25];
    let mut r = vec![0.5, 0.5, 0.5];
    assert!(silence_nonfinite_block(&mut l, &mut r));
    assert!(l.iter().chain(r.iter()).all(|s| *s == 0.0));

    let mut l = vec![0.1, 0.2];
    let mut r = vec![f32::INFINITY, 0.0];
    assert!(silence_nonfinite_block(&mut l, &mut r));
    assert!(l.iter().chain(r.iter()).all(|s| *s == 0.0));
}

#[test]
fn a_finite_block_is_left_alone() {
    let mut l = vec![0.5, -1.5, 0.25];
    let mut r = vec![0.0, 2.0, -0.75];
    assert!(!silence_nonfinite_block(&mut l, &mut r));
    assert_eq!(l, vec![0.5, -1.5, 0.25]);
    assert_eq!(r, vec![0.0, 2.0, -0.75]);
}
