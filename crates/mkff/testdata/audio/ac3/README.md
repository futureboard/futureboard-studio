# AC-3 / E-AC-3 test fixtures (`mkff-ac3`)

Raw AC-3 (`.ac3`) and E-AC-3 (`.eac3`) elementary streams, about 0.3 s each, with reference
decodes (`.ref`) made by FFmpeg (N-117098, 2024-09-22). The tests in
`bindings/rust/mkff-ac3/tests/decode.rs` decode the streams and report / check the SNR against
the references; they never run FFmpeg.

`make_fixtures.sh` (run from the repository root, with `ffmpeg` and `python3` on the `PATH`)
regenerates everything; it is reproduced in full below.

## Files

| Stream | Content | Coding tools | Reference |
|---|---|---|---|
| `ac3_mono_48k.ac3` | 1/0, 48 kHz, 96 kbit/s, tones + chirp | D15/D25/D45 exponents, grouped mantissas | `ac3_mono_48k.ref` (step 4) |
| `ac3_mono_44k.ac3` | 1/0, 44.1 kHz, 96 kbit/s | 44.1 kHz frame sizes / bit allocation tables | `ac3_mono_44k.ref` (step 4) |
| `ac3_stereo_48k.ac3` | 2/0, 48 kHz, 192 kbit/s | coupling, rematrixing | `ac3_stereo_48k.ref` (step 8) |
| `ac3_stereo_44k.ac3` | 2/0, 44.1 kHz, 160 kbit/s | coupling, rematrixing | `ac3_stereo_44k.ref` (step 8) |
| `ac3_51_48k.ac3` | 3/2 + LFE, 48 kHz, 384 kbit/s | coupling (4 channels), LFE | `ac3_51_48k.ref` (step 16) |
| `ac3_51_44k.ac3` | 3/2 + LFE, 44.1 kHz, 384 kbit/s | coupling, LFE | `ac3_51_44k.ref` (step 16) |
| `ac3_blksw_48k.ac3` | 2/0, clicks, 192 kbit/s | block switching (forced, see below), coupling | `ac3_blksw_48k.ref` (step 8) |
| `ac3_stereo_48k_dither.ac3` | `ac3_stereo_48k.ac3` as FFmpeg encoded it | dither flags on | `ac3_stereo_48k.ref` |
| `eac3_stereo_48k.eac3` | 2/0, 48 kHz, 192 kbit/s, 6 blocks | E-AC-3 frame exponent strategies, coupling | `eac3_stereo_48k.ref` (step 8) |
| `eac3_51_48k.eac3` | 3/2 + LFE, 384 kbit/s | E-AC-3 5.1 | `eac3_51_48k.ref` (step 16) |
| `eac3_71_48k.eac3` | 7.1: 3/2 + LFE independent substream + dependent substream (2/0, `chanmap` = Lrs/Rrs) | dependent substream, custom channel map | `eac3_71_48k.ref` (step 16) |

`.ref` format (see `make_ref.py`): `b"MKFFREF1"`, u32 channel count, u32 step, u32 frame
count (little endian), then every `step`-th sample frame of FFmpeg's `-f f32le` decode as
24-bit signed integers (value × 2^23), channels interleaved in WAV order. Decimating keeps the
files small; any decoding error shows up at the sampled positions as well.

## Why the streams are post-processed

- **Dither.** FFmpeg's encoder sets `dithflag` for every channel, and decoders fill zero-bit
  mantissas with their own random noise, so two correct decoders differ by about 60-70 dB SNR.
  The streams were rewritten to clear the dither flags (AC-3: the `dithflag` bits in place;
  E-AC-3: `dithflage` = 1 plus explicit zero `dithflag` bits in every block, paid for with unused
  auxiliary bits), CRCs recomputed. This is done by the ignored test
  `devtools::make_nodither_fixture` of `mkff-ac3` (it uses the decoder's own parser to locate
  the fields). `ac3_stereo_48k_dither.ac3` keeps FFmpeg's original flags.
- **Block switching.** FFmpeg's AC-3 encoder never switches to short blocks. In AC-3 the
  `blksw` bits do not affect the rest of the syntax, so `ac3_blksw_48k.ac3` was made from a
  normal encode by also setting `blksw` in a fixed pattern over blocks and channels
  (`MKFF_AC3_BLKSW=1`). The audio is then not what the encoder intended, but every decoder
  must reconstruct the same signal. The pattern makes the two channels' transforms differ in
  the very first block: FFmpeg copies the left delay line into the right one the first time the
  channels' transforms differ (an "upmix" of its initial state), which only leaves its output
  correct while the delay lines are still silent.
- **7.1.** FFmpeg cannot encode dependent substreams. `eac3_71_48k.eac3` interleaves a 5.1
  encode with the frames of a stereo encode turned into dependent substream frames
  (`strmtyp` 1, `chanmape` 1, `chanmap` 0x0200 = Lrs/Rrs, converter fields removed;
  ignored test `devtools::make_71_fixture`). FFmpeg decodes it as 7.1 with correct L R C LFE
  Lrs Rrs but loses the side pair (it outputs silence and a copy of Lrs), so the reference's
  last two channels (Ls, Rs) come from FFmpeg's decode of the independent substream alone.

All references use `-drc_scale 0` (FFmpeg applies dynamic range control by default; the
streams carry no `dynrng` words, so it makes no difference here).

## Not covered by these fixtures

FFmpeg's encoders never produce: E-AC-3 frames of 1, 2 or 3 blocks, AHT, spectral extension,
enhanced coupling, transient pre-noise processing, phase flags, delta bit allocation, dual
mono (1+1), Annex D `xbsi` fields, `bsid` 9 / 10 (half / quarter rate), or dynamic range words.
Those paths are only exercised by unit tests and by fuzzing in the crate.

## make_fixtures.sh

```sh
#!/bin/bash
# Regenerates testdata/audio/ac3 (see README.md there). Run from the repository root.
set -e
FF="ffmpeg -hide_banner -loglevel error -y"
OUT=$(pwd -W 2>/dev/null || pwd)/testdata/audio/ac3
TMP=${TMP_DIR:-/tmp/mkff-ac3-fixtures}
mkdir -p $TMP
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-target}
patch() { # in out [blksw]
  if [ -n "$3" ]; then export MKFF_AC3_BLKSW=1; else unset MKFF_AC3_BLKSW; fi
  MKFF_AC3_IN=$1 MKFF_AC3_OUT=$2 cargo test -q --release -p mkff-ac3 --lib make_nodither -- --ignored
}
ref() { # stream channels step ref
  $FF -drc_scale 0 -i $1 -f f32le $TMP/ref.f32
  python3 $OUT/make_ref.py $TMP/ref.f32 $2 $3 $4 "${@:5}"
}

M="0.4*sin(2*PI*440*t)+0.2*sin(2*PI*3000*t)+0.05*sin(2*PI*(2000+8000*t)*t)"
L="0.3*sin(2*PI*440*t)+0.1*sin(2*PI*5000*t)+0.03*sin(2*PI*(1000+8000*t)*t)"
R="0.25*sin(2*PI*660*t)+0.12*sin(2*PI*7000*t)+0.04*sin(2*PI*12000*t)"
C="0.3*sin(2*PI*330*t)*sin(2*PI*2*t)"
LFE="0.4*sin(2*PI*60*t)"
LS="0.2*sin(2*PI*1500*t)+0.05*sin(2*PI*9500*t)"
RS="0.2*sin(2*PI*2500*t)"
LRS="0.2*sin(2*PI*880*t)+0.07*sin(2*PI*11000*t)"
RRS="0.15*sin(2*PI*1320*t)*sin(2*PI*3*t)"
K="if(lt(mod(t\,0.1)\,0.003)\,0.9*sin(2*PI*2500*t)*exp(-300*mod(t\,0.1))\,0.0)+0.05*sin(2*PI*500*t)"
D=0.3

$FF -f lavfi -i "aevalsrc=$M:s=48000:d=$D" -c:a ac3 -b:a 96k -f ac3 $TMP/mono48.ac3
$FF -f lavfi -i "aevalsrc=$M:s=44100:d=$D" -c:a ac3 -b:a 96k -f ac3 $TMP/mono44.ac3
$FF -f lavfi -i "aevalsrc=$L|$R:s=48000:d=$D" -c:a ac3 -b:a 192k -f ac3 $OUT/ac3_stereo_48k_dither.ac3
$FF -f lavfi -i "aevalsrc=$L|$R:s=44100:d=$D" -c:a ac3 -b:a 160k -f ac3 $TMP/stereo44.ac3
$FF -f lavfi -i "aevalsrc=$L|$R|$C|$LFE|$LS|$RS:s=48000:d=$D:c=5.1" -c:a ac3 -b:a 384k -f ac3 $TMP/s51_48.ac3
$FF -f lavfi -i "aevalsrc=$L|$R|$C|$LFE|$LS|$RS:s=44100:d=$D:c=5.1" -c:a ac3 -b:a 384k -f ac3 $TMP/s51_44.ac3
$FF -f lavfi -i "aevalsrc=$K|$K*0.7+0.1*sin(2*PI*7000*t):s=48000:d=$D" -c:a ac3 -b:a 192k -f ac3 $TMP/clicks.ac3
$FF -f lavfi -i "aevalsrc=$L|$R:s=48000:d=$D" -c:a eac3 -b:a 192k -f eac3 $TMP/estereo48.eac3
$FF -f lavfi -i "aevalsrc=$L|$R|$C|$LFE|$LS|$RS:s=48000:d=$D:c=5.1" -c:a eac3 -b:a 384k -f eac3 $TMP/e51_48.eac3
$FF -f lavfi -i "aevalsrc=$LRS|$RRS:s=48000:d=$D" -c:a eac3 -b:a 192k -f eac3 $TMP/erear.eac3

patch $TMP/mono48.ac3 $OUT/ac3_mono_48k.ac3
patch $TMP/mono44.ac3 $OUT/ac3_mono_44k.ac3
patch $OUT/ac3_stereo_48k_dither.ac3 $OUT/ac3_stereo_48k.ac3
patch $TMP/stereo44.ac3 $OUT/ac3_stereo_44k.ac3
patch $TMP/s51_48.ac3 $OUT/ac3_51_48k.ac3
patch $TMP/s51_44.ac3 $OUT/ac3_51_44k.ac3
patch $TMP/clicks.ac3 $OUT/ac3_blksw_48k.ac3 blksw
patch $TMP/estereo48.eac3 $OUT/eac3_stereo_48k.eac3
patch $TMP/e51_48.eac3 $OUT/eac3_51_48k.eac3
MKFF_AC3_IN=$TMP/e51_48.eac3 MKFF_AC3_IN2=$TMP/erear.eac3 MKFF_AC3_OUT=$TMP/e71.eac3 \
  cargo test -q --release -p mkff-ac3 --lib make_71 -- --ignored
patch $TMP/e71.eac3 $OUT/eac3_71_48k.eac3

ref $OUT/ac3_mono_48k.ac3 1 4 $OUT/ac3_mono_48k.ref
ref $OUT/ac3_mono_44k.ac3 1 4 $OUT/ac3_mono_44k.ref
ref $OUT/ac3_stereo_48k.ac3 2 8 $OUT/ac3_stereo_48k.ref
ref $OUT/ac3_stereo_44k.ac3 2 8 $OUT/ac3_stereo_44k.ref
ref $OUT/ac3_51_48k.ac3 6 16 $OUT/ac3_51_48k.ref
ref $OUT/ac3_51_44k.ac3 6 16 $OUT/ac3_51_44k.ref
ref $OUT/ac3_blksw_48k.ac3 2 8 $OUT/ac3_blksw_48k.ref
ref $OUT/eac3_stereo_48k.eac3 2 8 $OUT/eac3_stereo_48k.ref
ref $OUT/eac3_51_48k.eac3 6 16 $OUT/eac3_51_48k.ref
# 7.1: FFmpeg places the rear pair correctly but loses the side pair (Ls/Rs) of the
# independent substream, so those two come from decoding the 5.1 part on its own.
python3 - $OUT/eac3_71_48k.eac3 $TMP/e71_indep.eac3 <<'PY'
import sys
d = open(sys.argv[1], 'rb').read(); out = bytearray(); p = 0
while p + 8 < len(d):
    size = ((d[p + 2] & 7) << 8 | d[p + 3]) * 2 + 2
    if (d[p + 2] >> 6) == 0:
        out += d[p:p + size]
    p += size
open(sys.argv[2], 'wb').write(out)
PY
$FF -drc_scale 0 -i $TMP/e71_indep.eac3 -f f32le $TMP/e71_indep.f32
ref $OUT/eac3_71_48k.eac3 8 16 $OUT/eac3_71_48k.ref --take 0,1,2,3,4,5 --from $TMP/e71_indep.f32:6:4,5
ls -la $OUT
```
