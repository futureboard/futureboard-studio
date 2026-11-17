//! Headless playback through the real engine: decode threads, the simulated audio device and the
//! audio-master clock. No GPU or sound card needed.
//!
//! Set `MKFF_PLAYER_MEDIA=/path/to/clip.mp4` to also play a few seconds of a real clip (e.g. the
//! Dolby "Audio_ID_720p_50fps_h264_514ch_448kbps_ddp.mp4" test signal) with timing checks.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use mkff_player::{AudioOutput, PictureData, PlaybackState, Player, PlayerOptions};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../testdata")
        .join(name)
}

fn open(name: &str, opts: PlayerOptions) -> Player {
    let media = mkff_player::open_media(&fixture(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
    Player::new(media, opts)
}

/// Call `update` like a 120 Hz UI until `done` or the timeout.
fn run_until(p: &mut Player, timeout: Duration, mut done: impl FnMut(&Player) -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        p.update();
        if done(p) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(8));
    }
    false
}

fn null_audio() -> PlayerOptions {
    PlayerOptions {
        audio: AudioOutput::Null,
        ..PlayerOptions::default()
    }
}

#[test]
fn plays_to_the_end_on_the_audio_clock() {
    for name in [
        "codecs/h264_aac.ts",
        "codecs/vp9_opus.webm",
        "codecs/hevc10_ac3.mkv",
        "codecs/mpeg2_mp2.ts",
    ] {
        let mut p = open(name, null_audio());
        assert!(p.has_video() && p.has_audio(), "{name}");
        let started = Instant::now();
        assert!(
            run_until(&mut p, Duration::from_secs(10), |p| p.state()
                == PlaybackState::Ended),
            "{name}: never ended (at {:.3} s)",
            p.position()
        );
        let wall = started.elapsed().as_secs_f64();
        let s = p.stats();
        assert!(s.audio_master, "{name}");
        // every frame shown, or dropped as late (a stalled CI runner), none lost
        assert!(
            s.presented + s.dropped >= p.frame_count() - 1,
            "{name}: {s:?}"
        );
        assert!(s.presented * 2 >= p.frame_count(), "{name}: {s:?}");
        assert_eq!(s.video_errors, 0, "{name}");
        assert_eq!(s.audio_errors, 0, "{name}");
        // real time: not much faster than the media, and only start-up overhead slower
        assert!(wall >= p.duration() * 0.9, "{name}: {wall} s wall");
        assert!(wall < p.duration() + 2.0, "{name}: {wall} s wall");
        assert_eq!(p.position(), p.duration());
    }
}

#[test]
fn every_container_shows_a_picture() {
    for name in [
        "codecs/av1_aac.mp4",
        "codecs/vp8_vorbis.webm",
        "codecs/prores_hq.mov",
        "codecs/prores_4444_alpha.mov",
        "codecs/dnxhr.mxf",
        "codecs/mjpeg_pcm.avi",
        "tiny_main_256x144.mp4",
        "tiny_baseline_64x64.mp4",
    ] {
        let mut p = open(
            name,
            PlayerOptions {
                audio: AudioOutput::Disabled,
                autoplay: false,
                ..PlayerOptions::default()
            },
        );
        assert!(
            run_until(&mut p, Duration::from_secs(5), |p| p
                .current_picture()
                .is_some()),
            "{name}: no picture"
        );
        let pic = p.current_picture().unwrap();
        assert_eq!(pic.index, 0, "{name}");
        let w = p.info().video.as_ref().unwrap().width;
        assert_eq!(pic.width, w, "{name}");
        let rgba = pic.to_rgba8();
        assert_eq!(rgba.len(), (pic.width * pic.height * 4) as usize, "{name}");
        let (lo, hi) = rgba
            .chunks_exact(4)
            .map(|px| px[1])
            .fold((255u8, 0u8), |(a, b), g| (a.min(g), b.max(g)));
        // the tiny Baseline fixture is a flat parser-smoke picture
        if name != "tiny_baseline_64x64.mp4" {
            assert!(hi - lo > 50, "{name}: flat picture");
        }
    }
}

#[test]
fn ten_bit_video_is_stored_at_eight_bits() {
    let mut p = open(
        "codecs/hevc10_ac3.mkv",
        PlayerOptions {
            audio: AudioOutput::Disabled,
            autoplay: false,
            ..PlayerOptions::default()
        },
    );
    assert!(run_until(&mut p, Duration::from_secs(5), |p| p
        .current_picture()
        .is_some()));
    let pic = p.current_picture().unwrap();
    assert_eq!(pic.source_bit_depth, 10);
    match &pic.data {
        PictureData::Yuv {
            y,
            chroma_width,
            chroma_height,
            ..
        } => {
            assert_eq!(y.len(), (pic.width * pic.height) as usize);
            assert_eq!(
                (*chroma_width, *chroma_height),
                (pic.width.div_ceil(2), pic.height.div_ceil(2))
            );
        }
        PictureData::Rgba(_) => panic!("expected Y'CbCr"),
    }
}

#[test]
fn seeking_while_paused_shows_the_frame_there() {
    let mut p = open(
        "codecs/h264_aac.ts",
        PlayerOptions {
            autoplay: false,
            ..null_audio()
        },
    );
    let fps = p.frame_rate();
    for target in [4usize, 1, 5, 0, 3] {
        let t = target as f64 / fps + 0.001;
        p.seek(t);
        assert!(
            run_until(&mut p, Duration::from_secs(5), |p| p
                .current_picture()
                .is_some_and(|c| c.index == target as u64)),
            "frame {target}"
        );
        assert_eq!(p.state(), PlaybackState::Paused);
        assert!((p.position() - t).abs() < 1e-9);
    }
}

#[test]
fn frame_stepping() {
    let mut p = open(
        "codecs/vp9_opus.webm",
        PlayerOptions {
            autoplay: false,
            ..null_audio()
        },
    );
    assert!(run_until(&mut p, Duration::from_secs(5), |p| p
        .current_picture()
        .is_some()));
    for expect in 1..4 {
        p.step(true);
        assert!(
            run_until(&mut p, Duration::from_secs(5), |p| p
                .current_picture()
                .is_some_and(|c| c.index == expect)),
            "forward to {expect}"
        );
    }
    p.step(false);
    assert!(run_until(&mut p, Duration::from_secs(5), |p| p
        .current_picture()
        .is_some_and(|c| c.index == 2)));
    assert_eq!(p.state(), PlaybackState::Paused);
}

#[test]
fn loops_and_replays() {
    let mut p = open(
        "codecs/vp8_vorbis.webm",
        PlayerOptions {
            looping: true,
            speed: 2.0,
            ..null_audio()
        },
    );
    let d = p.duration();
    let start = Instant::now();
    let mut wraps = 0;
    let mut last = 0.0;
    run_until(&mut p, Duration::from_secs_f64(d * 1.5 + 0.5), |p| {
        let pos = p.position();
        if pos + 0.05 < last {
            wraps += 1;
        }
        last = pos;
        false
    });
    assert!(wraps >= 1, "no wrap in {:?}", start.elapsed());
    assert_eq!(p.state(), PlaybackState::Playing);

    p.set_looping(false);
    assert!(run_until(&mut p, Duration::from_secs(5), |p| p.state()
        == PlaybackState::Ended));
    // play after the end starts over
    p.play();
    assert_eq!(p.state(), PlaybackState::Playing);
    assert!(p.position() < 0.05);
}

#[test]
fn audio_only_files_play() {
    for name in [
        "codecs/tone.flac",
        "codecs/tone.mp3",
        "codecs/tone.opus",
        "codecs/tone_eac3.mka",
        "codecs/tone_alac.m4a",
        "codecs/tone.aac",
        // 5.1, downmixed for the (stereo) simulated device
        "codecs/tone51_eac3.mp4",
        "codecs/tone51_ac3.mp4",
    ] {
        let mut p = open(name, null_audio());
        assert!(!p.has_video(), "{name}");
        assert!(p.has_audio(), "{name}");
        assert!(
            run_until(&mut p, Duration::from_secs(5), |p| p.state()
                == PlaybackState::Ended),
            "{name}: never ended"
        );
        let s = p.stats();
        assert!(s.audio_master);
        assert_eq!(s.audio_errors, 0, "{name}");
    }
}

#[test]
fn without_audio_the_system_clock_drives_video() {
    let mut p = open(
        "codecs/h264_aac.ts",
        PlayerOptions {
            audio: AudioOutput::Disabled,
            ..PlayerOptions::default()
        },
    );
    assert!(!p.has_audio());
    assert!(run_until(&mut p, Duration::from_secs(5), |p| p.state()
        == PlaybackState::Ended));
    let s = p.stats();
    assert!(!s.audio_master);
    assert!(s.presented + s.dropped >= p.frame_count() - 1, "{s:?}");
    assert!(s.presented * 2 >= p.frame_count(), "{s:?}");
}

#[test]
fn pause_freezes_the_clock() {
    let mut p = open("codecs/h264_aac.ts", null_audio());
    assert!(run_until(&mut p, Duration::from_secs(5), |p| p.position() > 0.05));
    p.pause();
    let at = p.position();
    std::thread::sleep(Duration::from_millis(100));
    p.update();
    assert_eq!(p.position(), at);
    p.play();
    assert!(run_until(&mut p, Duration::from_secs(5), |p| p.position() > at + 0.02));
}

#[test]
fn real_clip_plays_in_sync() {
    let Some(path) = std::env::var_os("MKFF_PLAYER_MEDIA") else {
        eprintln!("MKFF_PLAYER_MEDIA not set: skipped");
        return;
    };
    let media = mkff_player::open_media(path.as_ref()).unwrap();
    let mut p = Player::new(
        media,
        PlayerOptions {
            start: 10.0,
            ..null_audio()
        },
    );
    let start = Instant::now();
    let mut worst: f64 = 0.0;
    run_until(&mut p, Duration::from_secs(4), |p| {
        if p.current_picture().is_some() && !p.is_buffering() {
            worst = worst.max(p.stats().av_offset.abs());
        }
        false
    });
    let played = p.position() - 10.0;
    let s = p.stats();
    eprintln!("{s:?}, played {played:.3} s in {:?}", start.elapsed());
    assert!(played > 3.5 && played < 4.1, "played {played}");
    assert!(s.audio_master);
    assert_eq!(s.audio_underruns, 0);
    assert_eq!(s.video_errors, 0);
    assert!(s.dropped * 100 <= s.presented, "{s:?}");
    // within about one frame of the audio
    assert!(worst <= 1.5 / p.frame_rate(), "A-V {worst}");
}
