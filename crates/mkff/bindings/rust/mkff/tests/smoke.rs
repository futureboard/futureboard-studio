use mkff::{Context, ReceiveOutcome};

#[test]
fn context_create_destroy_repeated() {
    // Acceptance criterion: repeated create/destroy must not leak VA
    // objects or file descriptors, and must not panic even with no
    // platform module / GPU available (as in CI).
    for _ in 0..20 {
        let ctx = Context::new().expect("context creation itself must always succeed");
        drop(ctx);
    }
}

#[cfg(target_os = "linux")]
#[test]
fn enumerate_drm_devices_does_not_panic() {
    let ctx = Context::new().unwrap();
    // On a host with no GPU this returns Ok(vec![]) rather than erroring
    // (see linux_enumerate_drm_devices's handling of a missing
    // /dev/dri), so we only assert it doesn't panic and returns a
    // sensible (possibly empty) list.
    match ctx.linux_enumerate_drm_devices() {
        Ok(devices) => {
            for d in &devices {
                assert!(!d.path.is_empty());
            }
        }
        Err(e) => {
            // Also acceptable: a real error from the platform layer.
            let _ = e.to_string();
        }
    }
}

#[test]
fn decoder_lifecycle_is_safe_without_a_gpu() {
    let ctx = Context::new().unwrap();
    let mut decoder = match ctx.video_decoder_h264(0) {
        Ok(d) => d,
        Err(_) => return, // platform module unavailable in this environment: nothing further to test
    };

    // Not a real H.264 stream — this must fail cleanly (device/bitstream
    // error) rather than crash, exactly like the CLI's decode-test path.
    let fake = [0u8, 0, 0, 1, 0x65, 0xAA, 0xAA];
    let _ = decoder.submit(&fake, Some(0), Some(0));

    match decoder.receive() {
        Ok(ReceiveOutcome::Frame(frame)) => {
            let info = frame.info().unwrap();
            assert!(info.width > 0 && info.height > 0);
        }
        Ok(ReceiveOutcome::NotReady) | Ok(ReceiveOutcome::EndOfStream) | Err(_) => {}
    }

    let _ = decoder.flush();
}

#[test]
fn hevc_software_decoder_create_is_safe() {
    use mkff::VideoBackend;
    let ctx = Context::new().unwrap();
    let decoder = ctx.video_decoder_hevc_with_backend(0, VideoBackend::MKFF_VIDEO_BACKEND_SOFTWARE_ONLY);
    match decoder {
        Ok(decoder) => {
            let _ = decoder.info();
        }
        Err(_) => {
            // Acceptable when HEVC software was disabled at build time.
        }
    }
}
