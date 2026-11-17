# FLAC test fixtures

Small FLAC streams made with ffmpeg (N-117098, its native `flac` encoder) for the
`mkff-flac` decoder tests. ffmpeg is only the fixture *encoder*: the tests never run it. They
decode every frame and compare against the MD5 of ffmpeg's own decode (16-bit streams as
`s16le`, wider ones left-justified in `s32le`) and against the STREAMINFO MD5.

The common two-channel source (`$MUSIC`) is 0.3 s of tones with a little noise, 0.1 s of
silence (constant subframes) and then loud noise:

```sh
MUSIC="aevalsrc='if(lt(t,0.3),0.4*sin(2*PI*440*t)+0.2*sin(2*PI*660*t+1)*sin(2*PI*3*t)+0.03*(random(0)-0.5),if(lt(t,0.4),0,0.9*(random(2)-0.5)))|if(lt(t,0.3),0.3*sin(2*PI*330*t)+0.25*sin(2*PI*880*t)+0.03*(random(1)-0.5),if(lt(t,0.4),0,0.5*sin(2*PI*1000*t)))'"
X="-fflags +bitexact -map_metadata -1"

ffmpeg -f lavfi -i "$MUSIC:s=44100:d=0.5" -c:a flac -sample_fmt s16 $X stereo16_l5.flac
ffmpeg -f lavfi -i "$MUSIC:s=48000:d=0.4" -c:a flac -sample_fmt s16 -compression_level 12 \
  -max_prediction_order 32 -lpc_coeff_precision 15 -frame_size 2304 -strict -2 $X stereo16_lpc32.flac
ffmpeg -f lavfi -i "$MUSIC:s=96000:d=0.25" -c:a flac -sample_fmt s32 -bits_per_raw_sample 24 $X stereo24_96k.flac
ffmpeg -f lavfi -i "$MUSIC:s=44100:d=0.25" -c:a flac -sample_fmt s32 -bits_per_raw_sample 32 -strict -2 $X stereo32.flac
ffmpeg -f lavfi -i "aevalsrc='0.3*sin(2*PI*300*t)|0.3*sin(2*PI*500*t)|0.3*sin(2*PI*700*t)|0.2*sin(2*PI*60*t)|0.3*sin(2*PI*900*t)+0.02*(random(0)-0.5)|0.3*sin(2*PI*1100*t)':s=48000:d=0.25:c=5.1" \
  -c:a flac -sample_fmt s16 $X surround51.flac
ffmpeg -f lavfi -i "$MUSIC:s=11025:d=0.5" -ac 1 -c:a flac -sample_fmt s16 -lpc_type fixed -frame_size 1000 $X mono_fixed_11025.flac
ffmpeg -f lavfi -i "$MUSIC:s=44100:d=0.3" -f s16le - | \
  ffmpeg -f s16le -ar 44100 -ac 2 -i - -c:a flac -sample_fmt s32 -bits_per_raw_sample 24 $X wasted24.flac
```

Reference hashes (`ffmpeg -i F -f s16le - | md5sum`, or `-f s32le` for 24 / 32-bit):

| File | What it covers | ffmpeg decode MD5 |
|------|----------------|-------------------|
| `stereo16_l5.flac` | default level: LPC order 8, all stereo modes, constant subframes | `b3b57b9f3c2b9709e62ddc606b43758c` |
| `stereo16_lpc32.flac` | LPC orders up to 32 (17-20 used), precision 15, block 2304, partition order 6 | `a23ce5ba92e220bcde7d8ae4adbf8d16` (s16le) |
| `stereo24_96k.flac` | 24-bit, 96 kHz, Rice2 (5-bit parameters), mid-side | `84b863cae8d1f026c7eae544cae5f1fb` |
| `stereo32.flac` | 32-bit, 33-bit side channel, Rice2 | `5a752567ccd5f6051806937ba337ae4a` |
| `surround51.flac` | 6 independent channels | `e54c24cf1982c7562e511f6e7eed00ff` |
| `mono_fixed_11025.flac` | fixed predictors, 1000-sample blocks (16-bit block size field), 11025 Hz (16-bit rate field) | `04faca744878e78674283fb588a0db80` |
| `wasted24.flac` | 24-bit container holding 16-bit audio: 8 wasted bits per subframe | `b43096b38a1f641a909bd39781c5a996` |

`testdata/codecs/tone.flac` (mono 16-bit, `02e51bdbaa961229fcc721869bb43385`) is used too.
Verbatim subframes, escaped Rice partitions, variable block size headers, LPC order 32 with a
negative shift (an error) and other rare cases are covered by hand-built frames in the unit
tests of `bindings/rust/mkff-flac/src/decoder.rs`.
