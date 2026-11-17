# Opus fixtures (mkff-opus)

Small Ogg Opus files encoded with libopus (through ffmpeg) and their reference decodes, used by
`bindings/rust/mkff-opus/tests/fixtures.rs` and `tests/malformed.rs`. The tests never run
ffmpeg.

- `NAME.opus`: Ogg Opus stream.
- `NAME.s16`: reference decode by libopus (ffmpeg's `libopus` decoder, pre-skip and end padding
  trimmed by ffmpeg), 48 kHz, interleaved signed 16-bit little-endian, WAV channel order.

The sources are synthetic (ffmpeg `aevalsrc`): a harmonic "speech-like" signal with vibrato,
syllable-rate amplitude modulation and noise bursts, a stereo "music" mix of tones, noise and
clicks (transients), and a stereo mix of the two.

| File | Mode | Content |
| --- | --- | --- |
| `celt_stereo_20ms` | CELT FB, 20 ms, 128 kb/s stereo | transients, short blocks |
| `celt_stereo_60ms` | CELT FB, 3 x 20 ms per packet (code 3), 32 kb/s stereo | folding, intensity stereo |
| `celt_mono_2_5ms` / `_5ms` / `_10ms` | CELT FB mono, 2.5 / 5 / 10 ms frames | LM 0–2 |
| `silk_nb_mono` | SILK NB (8 kHz), 20 ms | |
| `silk_mb_60ms` | SILK MB (12 kHz), 60 ms (3 SILK frames per packet) | |
| `silk_wb_stereo_40ms` | SILK WB (16 kHz) stereo, 40 ms | mid/side prediction |
| `hybrid_fb_stereo` | Hybrid FB stereo, 20 ms | |
| `hybrid_swb_10ms` | Hybrid SWB mono, 10 ms | |
| `surround51` | 5.1, mapping family 1 (4 streams, 2 coupled) | Vorbis -> WAV order |
| `hybrid_to_celt` | Hybrid -> CELT switch chosen by the encoder | redundancy frame |
| `mode_switch` | SILK NB -> CELT -> hybrid SWB -> SILK MB -> CELT spliced | transitions concealed by PLC |

## Regenerating

`make_fixtures.sh` holds the exact commands (ffmpeg with libopus, plus `oggcat.py`, a small
Ogg packet slicer / concatenator used for `hybrid_to_celt` and `mode_switch`). Run it from this
directory: `FFMPEG=/path/to/ffmpeg sh make_fixtures.sh` (Python 3 is needed for `oggcat.py`).
Encoder settings per file:

```sh
# sources (2 s each): speech.wav (mono), music.wav (stereo), speechmusic.wav (stereo); see the script
ffmpeg -t 0.16 -i music.wav -c:a libopus -application lowdelay -b:a 128k -frame_duration 20 celt_stereo_20ms.opus
ffmpeg -t 0.18 -i music.wav -c:a libopus -application lowdelay -b:a 32k -frame_duration 60 celt_stereo_60ms.opus
ffmpeg -t 0.1 -i music.wav -ac 1 -c:a libopus -application lowdelay -b:a 64k -frame_duration 2.5 celt_mono_2_5ms.opus
ffmpeg -t 0.1 -i music.wav -ac 1 -c:a libopus -application lowdelay -b:a 48k -frame_duration 5 celt_mono_5ms.opus
ffmpeg -t 0.1 -i music.wav -ac 1 -c:a libopus -application lowdelay -b:a 40k -frame_duration 10 celt_mono_10ms.opus
ffmpeg -t 0.3 -i speech.wav -c:a libopus -application voip -b:a 8k -cutoff 4000 -frame_duration 20 silk_nb_mono.opus
ffmpeg -t 0.24 -i speech.wav -c:a libopus -application voip -b:a 12k -cutoff 6000 -frame_duration 60 silk_mb_60ms.opus
ffmpeg -t 0.16 -i speechmusic.wav -c:a libopus -application voip -b:a 20k -cutoff 8000 -frame_duration 40 silk_wb_stereo_40ms.opus
ffmpeg -t 0.12 -i speechmusic.wav -c:a libopus -application voip -b:a 32k -frame_duration 20 hybrid_fb_stereo.opus
ffmpeg -t 0.2 -i speech.wav -c:a libopus -application voip -b:a 24k -cutoff 12000 -frame_duration 10 hybrid_swb_10ms.opus
ffmpeg -t 0.04 -i music.wav -filter_complex "[0:a]pan=5.1|c0=c0|c1=c1|c2=0.5*c0+0.5*c1|c3=0.3*c0|c4=0.8*c0-0.2*c1|c5=0.8*c1-0.2*c0[o]" \
  -map "[o]" -c:a libopus -application audio -b:a 160k -mapping_family 1 surround51.opus
ffmpeg -t 2 -i speechmusic.wav -ac 1 -af "volume=enable='gt(t,1)':volume=0.3" -c:a libopus -application audio -b:a 24k transitions.opus
python oggcat.py hybrid_to_celt.opus "transitions.opus[68:84]"
python oggcat.py mode_switch.opus "silk_nb_mono.opus[0:10]" celt_mono_10ms.opus \
  "hybrid_swb_10ms.opus[0:10]" "silk_mb_60ms.opus[0:2]" "celt_mono_2_5ms.opus[0:20]"
# references
for f in *.opus; do ffmpeg -c:a libopus -i "$f" -f s16le -ar 48000 "${f%.opus}.s16"; done
```

The decoding is forced through libopus (`-c:a libopus` before `-i`), not ffmpeg's native Opus
decoder. Generated with an ffmpeg git build of 2024-09-22 (N-117098, libavcodec 61.17.100) and its bundled libopus.

## Results (mkff-opus vs libopus float output)

SNR of the mkff-opus decode against libopus' float output (measured during development; the
committed 16-bit references cap the SNR the tests can see at roughly 80–95 dB):

| File | SNR |
| --- | --- |
| celt_stereo_20ms | 82.3 dB |
| celt_stereo_60ms | 82.4 dB |
| celt_mono_2_5ms / 5ms / 10ms | 81.2 / 81.4 / 81.2 dB |
| silk_nb_mono, silk_mb_60ms, silk_wb_stereo_40ms | bit-exact |
| hybrid_fb_stereo | 83.7 dB |
| hybrid_swb_10ms | 87.1 dB |
| surround51 | 82.0 dB |
| hybrid_to_celt | 72.3 dB |
| mode_switch | 25.1 dB (2.5 ms concealments at the two SILK -> CELT splices differ; elsewhere 78–96 dB) |
