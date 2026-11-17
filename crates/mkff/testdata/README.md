# Testdata

Checked-in Annex-B elementary streams and progressive MP4 containers.

| File | Codec | Notes |
|------|-------|-------|
| `tiny_baseline_64x64.h264` | H.264 Baseline | Minimal SPS/PPS fixture for CLI smoke |
| `tiny_baseline_64x64.mp4` | H.264 `avc1` | Progressive moov+mdat remux of the `.h264` fixture |
| `tiny_main_64x64.hevc` | HEVC Main | Synthetic VPS/SPS/PPS only (no VCL) |
| `tiny_main_256x144.hevc` | HEVC Main | Single IDR; software decode must succeed |
| `tiny_main_256x144.mp4` | HEVC `hvc1` | Progressive moov+mdat remux of the `.hevc` fixture |
| `tiny_main_p_256x144.hevc` | HEVC Main | I+P sample (optional inter coverage) |
| `tiny_main10_64x64.hevc` | HEVC Main10 | Single IDR 10-bit; HW → P010; SW returns CODEC_UNAVAILABLE |

`tiny_main_256x144.hevc` / `tiny_main_p_256x144.hevc` originate from the
[oxideav-h265](https://github.com/OxideAV/oxideav-h265) test fixtures
(Apache-2.0 / MIT-compatible open test data). `tiny_main10_64x64.hevc`
was generated with libx265 (`profile=main10`, WPP off).

Progressive `.mp4` fixtures are generated offline (no FFmpeg runtime dependency)
via `scripts/gen_tiny_mp4.py` from the matching Annex-B files, then checked in.

## `codecs/` — software codec fixtures

One tiny synthetic clip (ffmpeg `testsrc2` pattern + 440 Hz `sine`) per
container / codec that `mkff-codecs` decodes, regenerated with
`scripts/gen_codec_fixtures.sh` (ffmpeg is only the fixture *encoder*):

| File | Container | Video | Audio |
|------|-----------|-------|-------|
| `vp8.ivf` / `vp9.ivf` / `av1.ivf` | IVF | VP8 / VP9 / AV1 | — |
| `h264_high.h264` | Annex-B | H.264 High, B-frames | — |
| `vp9_opus.webm` | WebM | VP9 | Opus |
| `vp8_vorbis.webm` | WebM | VP8 | Vorbis |
| `av1_aac.mp4` | MP4 | AV1 | AAC |
| `hevc10_ac3.mkv` | Matroska | HEVC Main10 | AC-3 |
| `mpeg2_mp2.ts` | MPEG-TS | MPEG-2 | MP2 |
| `h264_aac.ts` | MPEG-TS | H.264 | AAC (ADTS) |
| `prores_hq.mov` | QuickTime | ProRes 422 HQ 10-bit | PCM |
| `prores_4444_alpha.mov` | QuickTime | ProRes 4444 + alpha | — |
| `dnxhr.mxf` | MXF OP1a | DNxHR LB (256x128) | PCM |
| `mjpeg_pcm.avi` | AVI | Motion JPEG | PCM |
| `tone.flac` / `tone.mp3` / `tone.opus` / `tone.aac` | audio | — | FLAC / MP3 / Opus / AAC ADTS |
| `tone_eac3.mka` / `tone_alac.m4a` | Matroska / MP4 | — | E-AC-3 / ALAC |
| `tone51_ac3.mp4` / `tone51_eac3.mp4` | MP4 | — | 5.1 AC-3 / E-AC-3, one tone per channel (FL 300, FR 500, FC 700, LFE 60, SL 900, SR 1100 Hz), sample entry `ChannelCount` 2 as the spec requires |
