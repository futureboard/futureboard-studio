# ALAC test fixtures

Small ALAC-in-M4A files made with ffmpeg (N-117098, its native `alac` encoder) for the
`mkff-alac` decoder tests. ffmpeg is only the fixture *encoder*: the tests never run it. A
test-only MP4 sample extractor (`bindings/rust/mkff-alac/tests/common/mod.rs`) pulls the
magic cookie and the packets out; every packet is decoded and the result compared with the MD5
of ffmpeg's own decode (16-bit as `s16le`, 24-bit left-justified in `s32le`).

```sh
MUSIC="aevalsrc='if(lt(t,0.3),0.4*sin(2*PI*440*t)+0.2*sin(2*PI*660*t+1)*sin(2*PI*3*t)+0.03*(random(0)-0.5),if(lt(t,0.4),0,0.9*(random(2)-0.5)))|if(lt(t,0.3),0.3*sin(2*PI*330*t)+0.25*sin(2*PI*880*t)+0.03*(random(1)-0.5),if(lt(t,0.4),0,0.5*sin(2*PI*1000*t)))'"
X="-fflags +bitexact -map_metadata -1 -movflags +faststart"

ffmpeg -f lavfi -i "$MUSIC:s=44100:d=0.5" -c:a alac -sample_fmt s16p $X stereo16.m4a
ffmpeg -f lavfi -i "$MUSIC:s=48000:d=0.3" -c:a alac -sample_fmt s32p -bits_per_raw_sample 24 $X stereo24.m4a
ffmpeg -f lavfi -i "$MUSIC:s=44100:d=0.2" -c:a alac -sample_fmt s16p -compression_level 0 $X stereo16_uncompressed.m4a
ffmpeg -f lavfi -i "$MUSIC:s=44100:d=0.5" -ac 1 -c:a alac -sample_fmt s32p -bits_per_raw_sample 24 \
  -max_prediction_order 30 $X mono24_order30.m4a
ffmpeg -f lavfi -i "aevalsrc='0.3*sin(2*PI*300*t)|0.3*sin(2*PI*500*t)|0.3*sin(2*PI*700*t)':s=44100:d=0.15:c=3.0" \
  -c:a alac -sample_fmt s16p $X surround30.m4a
ffmpeg -f lavfi -i "aevalsrc='0.3*sin(2*PI*300*t)|0.3*sin(2*PI*500*t)|0.3*sin(2*PI*700*t)|0.2*sin(2*PI*60*t)|0.3*sin(2*PI*900*t)+0.02*(random(0)-0.5)|0.3*sin(2*PI*1100*t)':s=48000:d=0.2:c=5.1" \
  -c:a alac -sample_fmt s16p $X surround51.m4a
ffmpeg -f lavfi -i "aevalsrc='0.3*sin(2*PI*300*t)|0.3*sin(2*PI*500*t)|0.3*sin(2*PI*700*t)|0.2*sin(2*PI*60*t)|0.3*sin(2*PI*900*t)|0.3*sin(2*PI*1100*t)|0.3*sin(2*PI*1300*t)':s=48000:d=0.15:c=6.1(back)" \
  -c:a alac -sample_fmt s16p $X surround61.m4a
```

| File | What it covers | ffmpeg decode MD5 |
|------|----------------|-------------------|
| `stereo16.m4a` | CPE with mixres / mixbits, zero runs, escaped residuals, uncompressed (noise) and partial frames | `b3b57b9f3c2b9709e62ddc606b43758c` |
| `stereo24.m4a` | 24-bit with one shifted-off byte per sample | `c6d3d02701017d5045b27ce2f6a73c43` |
| `stereo16_uncompressed.m4a` | escape (uncompressed) elements only; cookie with `kb` = 0 | `c945692dd2439426911e821a925106ef` |
| `mono24_order30.m4a` | SCE, predictor orders up to 30, shifted byte | `885de01a0710be5b8d451bb53d8515f2` |
| `surround30.m4a` | 3.0: SCE + CPE | `84223d2df51f581feeda9ed843d42897` |
| `surround51.m4a` | 5.1: SCE + CPE + CPE + LFE, mapped to WAV order | `aecf7148a3364fac2b34675a47a8c6ab` |
| `surround61.m4a` | 6.1: SCE + CPE + CPE + SCE + LFE | `86fb711b87c1c6b5bfbe6b808ff24275` |

`testdata/codecs/tone_alac.m4a` (mono 16-bit, `02e51bdbaa961229fcc721869bb43385`) is used too.
`stereo16.m4a` decodes to the same PCM as `../flac/stereo16_l5.flac` (same source).

ffmpeg's ALAC encoder only writes 16 and 24-bit and at most 7 channels; 20 / 32-bit, 8 channels,
prediction mode 15 and DSE / FIL elements are covered by unit tests and encoder round trips.
