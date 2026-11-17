//! Hardware decode through the roster. Skips (passes) on machines without a GPU decoder.

use mkff_mediakit::{AccelPolicy, CodecFamily, MediaFormat, MediaRoster, PixelFormat};

fn fixture(name: &str) -> Vec<u8> {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../testdata/codecs")
        .join(name);
    std::fs::read(p).expect("fixture")
}

#[test]
fn h264_on_the_gpu() {
    let roster = MediaRoster::roster();
    let fmt = MediaFormat::encoded(CodecFamily::H264, Vec::new(), 0, 0);
    let (flavor, mut dec) = match roster.video_decoder(&fmt, AccelPolicy::HardwareOnly) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("skipping: {e}");
            return;
        }
    };
    assert!(flavor.info.accel.is_hardware());
    let mut frames = dec.decode(&fixture("h264_high.h264"), 0).unwrap();
    frames.extend(dec.flush().unwrap());
    assert!(!frames.is_empty());
    for f in &frames {
        assert_eq!((f.width, f.height, f.format), (64, 48, PixelFormat::Nv12));
        assert_eq!(f.planes.len(), 2);
        assert_eq!(f.planes[0].len(), 64 * 48);
        assert_eq!(f.planes[1].len(), 64 * 24);
        assert!(f.gpu.is_none(), "CPU mode returns surfaces to the decoder");
        // a real picture, not a cleared surface
        let (min, max) = f.planes[0]
            .iter()
            .fold((255u8, 0u8), |(a, b), &v| (a.min(v), b.max(v)));
        assert!(max > min + 16, "flat luma {min}..{max}");
    }
    // presentation order
    assert!(frames.windows(2).all(|w| w[0].pts < w[1].pts));
    // reset + decode again gives the same first picture
    dec.reset();
    let again = dec.decode(&fixture("h264_high.h264"), 0).unwrap();
    let again = if again.is_empty() {
        dec.flush().unwrap()
    } else {
        again
    };
    assert_eq!(again[0].planes[0], frames[0].planes[0]);
}

#[test]
fn policy_filters_flavors() {
    let roster = MediaRoster::roster();
    let h264 = MediaFormat::encoded(CodecFamily::H264, Vec::new(), 0, 0);
    // no software video add-on is registered in this crate
    assert!(roster
        .video_decoder(&h264, AccelPolicy::SoftwareOnly)
        .is_err());
    let hw = roster.find_flavors(
        mkff_mediakit::Kinds::DECODER,
        &h264,
        &MediaFormat::Wildcard,
        AccelPolicy::HardwareOnly,
    );
    assert!(hw.iter().all(|f| f.info.accel.is_hardware()));
}

#[test]
fn h264_gpu_surfaces() {
    let roster = MediaRoster::roster();
    let fmt = MediaFormat::encoded(CodecFamily::H264, Vec::new(), 0, 0);
    let mut want = MediaFormat::raw_video(0, 0, PixelFormat::Any);
    if let MediaFormat::RawVideo(v) = &mut want {
        v.gpu = true;
    }
    let flavors = roster.find_flavors(
        mkff_mediakit::Kinds::DECODER,
        &fmt,
        &want,
        AccelPolicy::HardwareOnly,
    );
    let Some(flavor) = flavors.first() else {
        eprintln!("skipping: no GPU decoder");
        return;
    };
    let mkff_mediakit::Instance::VideoDecoder(mut dec) =
        roster.instantiate(flavor, &fmt, &want).unwrap()
    else {
        panic!()
    };
    // three passes over the clip, dropping frames as we go: surfaces must recycle
    let mut total = 0;
    for _ in 0..3 {
        let mut frames = dec.decode(&fixture("h264_high.h264"), 0).unwrap();
        frames.extend(dec.flush().unwrap());
        for f in &frames {
            let s = f.gpu.as_ref().expect("GPU mode gives textures");
            assert_eq!((s.width, s.height), (64, 48));
            assert!(f.planes.is_empty());
            #[cfg(windows)]
            assert!(mkff_mediakit::accelerant::win32::texture_of(s).is_some());
        }
        total += frames.len();
        drop(frames);
        dec.reset();
    }
    assert!(total >= 18);
}
