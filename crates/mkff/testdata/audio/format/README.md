# Audio container fixtures (`mkff-audio-format`)

Small clips for the demuxer / muxer tests in `bindings/rust/mkff-audio-format/tests`. All are
a 440 Hz sine (330 Hz / 660 Hz where noted) from FFmpeg's `sine` source, made with
FFmpeg N-117098 (2024-09, with libmp3lame, libopus, libvorbis). The tests also use clips from
`testdata/codecs` (`tone.mp3`, `tone.aac`, `tone.flac`, `tone.opus`, `tone_alac.m4a`,
`tone51_ac3.mp4`, `tone51_eac3.mp4`, `av1_aac.mp4`, `prores_hq.mov`).

Expected values in the tests (packet counts, durations, priming, padding) were checked with
`ffprobe -show_streams -show_packets` when the files were made.

```sh
S="-f lavfi -i sine=frequency=440:sample_rate=44100:duration=0.1"
ffmpeg $S -ac 2 -c:a pcm_s16le s16_stereo.wav
ffmpeg $S -c:a pcm_u8 u8_mono.wav
ffmpeg -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=0.05" \
       -af "pan=5.1|c0=c0|c1=c0|c2=c0|c3=c0|c4=c0|c5=c0" -c:a pcm_s24le s24_51.wav
ffmpeg $S -c:a pcm_f32le f32_mono.wav
ffmpeg $S -c:a pcm_f64le f64_mono.wav
ffmpeg $S -c:a pcm_alaw alaw.wav
ffmpeg $S -c:a pcm_mulaw mulaw.wav
ffmpeg $S -c:a pcm_s16le -rf64 always rf64.wav
ffmpeg $S -ac 2 -c:a pcm_s16be s16.aiff
ffmpeg $S -c:a pcm_s24be s24.aiff
ffmpeg $S -c:a pcm_f32be -f aiff f32.aifc
ffmpeg $S -c:a pcm_s16le -f aiff sowt.aifc
ffmpeg $S -ac 2 -c:a pcm_s16le sowt.mov
ffmpeg $S -c:a pcm_s24le in24le.mov
ffmpeg $S -c:a pcm_f32be fl32.mov

S="-f lavfi -i sine=frequency=440:sample_rate=44100:duration=0.25"
S48="-f lavfi -i sine=frequency=440:sample_rate=48000:duration=0.25"
ffmpeg -f lavfi -i "sine=frequency=440:sample_rate=44100:duration=1" -ac 2 -c:a libmp3lame -q:a 6 \
       -id3v2_version 3 -write_id3v1 1 -metadata title=Tone -metadata artist=MKFF vbr.mp3
ffmpeg $S48 -c:a mp2 -b:a 64k tone.mp2
ffmpeg -f lavfi -i "sine=frequency=440:sample_rate=22050:duration=0.5" -c:a libmp3lame -b:a 32k \
       -write_xing 0 mpeg2_noxing.mp3
ffmpeg -f lavfi -i "sine=frequency=440:sample_rate=44100:duration=1" -ac 2 -c:a flac \
       -compression_level 0 seek.flac
ffmpeg -f lavfi -i "sine=frequency=440:sample_rate=44100:duration=0.5" -c:a libvorbis -q:a 0 tone.ogg
ffmpeg $S -c:a flac -f ogg flac.oga
ffmpeg -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=0.5" \
       -f lavfi -i "sine=frequency=660:sample_rate=44100:duration=0.5" -map 0 -map 1 \
       -c:a:0 libopus -b:a:0 24k -c:a:1 libvorbis -q:a:1 0 multi.ogg
ffmpeg -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=4" -c:a libopus -b:a 12k \
       -page_duration 500000 long.opus
ffmpeg -f lavfi -i "sine=frequency=330:sample_rate=22050:duration=3" -c:a libvorbis -q:a 0 \
       -page_duration 500000 long_vorbis.ogg
ffmpeg $S -c:a aac -b:a 64k aac.m4a
ffmpeg $S -c:a aac -b:a 64k -movflags frag_keyframe+empty_moov+default_base_moof \
       -frag_duration 100000 aac_frag.mp4
ffmpeg $S -c:a flac flac.mp4
ffmpeg $S48 -c:a libopus -b:a 24k opus.mp4
ffmpeg $S -c:a libmp3lame -b:a 64k mp3.mp4
```

| File | What it covers |
|---|---|
| `s16_stereo.wav`, `u8_mono.wav`, `f32_mono.wav`, `f64_mono.wav`, `alaw.wav`, `mulaw.wav` | WAV sample formats |
| `s24_51.wav` | `WAVE_FORMAT_EXTENSIBLE`, 6 channels, mask 0x3F |
| `rf64.wav` | RF64 with `ds64` |
| `s16.aiff`, `s24.aiff`, `f32.aifc`, `sowt.aifc` | AIFF / AIFF-C (`NONE`, `fl32`, `sowt`) |
| `vbr.mp3` | ID3v2 + ID3v1, VBR with Xing TOC and LAME delay / padding |
| `mpeg2_noxing.mp3` | MPEG-2 layer III (576-sample frames), no Xing frame |
| `tone.mp2` | layer II |
| `seek.flac` | stereo FLAC, 39 frames, no SEEKTABLE (the FLAC writer test covers seek tables) |
| `tone.ogg`, `long_vorbis.ogg` | Vorbis, single page / many pages (end trimming 30 / 410 samples) |
| `long.opus` | Opus over many pages (pre-skip 312, end trimming 648) |
| `flac.oga` | FLAC in Ogg |
| `multi.ogg` | two logical streams (Opus + Vorbis) |
| `aac.m4a` | AAC with edit list (priming 1024) |
| `aac_frag.mp4` | fragmented MP4 (`moof` / `trun`, `default-base-is-moof`) |
| `flac.mp4`, `opus.mp4`, `mp3.mp4` | `fLaC` + `dfLa`, `Opus` + `dOps`, MP3 in `mp4a` (OTI 0x6B) |
| `sowt.mov`, `in24le.mov`, `fl32.mov` | QuickTime PCM sample descriptions (`sowt`, `in24` + `enda`, `fl32`) |
