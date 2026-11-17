# MPEG audio fixtures (`mkff-mp3`)

Small streams for the `mkff-mp3` decoder tests (about 350 KB in total). FFmpeg / libmp3lame /
libtwolame are only the fixture *encoders* and the *reference decoder*; the tests never run
them.

Every `<name>.ref` is the reference: FFmpeg's **float** decoder output (`mp1float`,
`mp2float`, `mp3float`) for 1536 sample frames starting at `0.16 * sample_rate`, raw
interleaved `f32le`. For `l3_44k_vbr_xing.mp3` FFmpeg applies the LAME-tag gapless trimming,
so the window corresponds to the untrimmed decode shifted by `576 + 529 = 1105` samples.
`l3_22k_js_32_is.mp3` has no reference (see below).

| File | Layer / version | Notes |
|------|-----------------|-------|
| `l1_48k_js_384.mp1` | I, MPEG-1 48 kHz | joint stereo (bound 8), written by the crate's test-only Layer I encoder |
| `l1_32k_mono_192.mp1` | I, MPEG-1 32 kHz | mono, same encoder |
| `l2_44k_js_192_crc.mp2` | II, 44.1 kHz | libtwolame joint stereo **with CRC** (table B.2b) |
| `l2_48k_stereo_256.mp2` | II, 48 kHz | FFmpeg `mp2`, table B.2a |
| `l2_48k_stereo_64.mp2` | II, 48 kHz | 32 kbit/s per channel: table B.2c |
| `l2_32k_mono_48.mp2` | II, 32 kHz | table B.2d |
| `l2_22k_mono_64.mp2` | II, MPEG-2 LSF 22.05 kHz | LSF allocation table |
| `l3_44k_js_128.mp3` | III, 44.1 kHz | LAME joint stereo (M/S frames, short blocks, reservoir) |
| `l3_44k_stereo_320.mp3` | III, 44.1 kHz | 320 kbit/s: escape tables / linbits |
| `l3_44k_js_64_ms.mp3` | III, 44.1 kHz | low-rate M/S |
| `l3_44k_js_64_is.mp3` | III, 44.1 kHz | the previous file with the **intensity stereo** flag set in every frame |
| `l3_44k_vbr_xing.mp3` | III, 44.1 kHz | VBR, ID3v2 tag + Xing header + LAME extension (FFmpeg writes `Lavc…`) |
| `l3_48k_stereo_160.mp3` | III, 48 kHz | plain stereo |
| `l3_32k_mono_56.mp3` | III, 32 kHz | mono |
| `l3_24k_mono_32.mp3` | III, MPEG-2 24 kHz | |
| `l3_22k_js_48.mp3` | III, MPEG-2 22.05 kHz | joint stereo |
| `l3_22k_js_32_ms.mp3` | III, MPEG-2 22.05 kHz | joint stereo, source of the LSF IS file |
| `l3_22k_js_32_is.mp3` | III, MPEG-2 22.05 kHz | intensity flag set: **LSF intensity stereo**. No reference: FFmpeg ignores the ISO/IEC 13818-3 illegal intensity position (`2^slen - 1`), `mkff-mp3` follows the standard |
| `l3_16k_stereo_32.mp3` | III, MPEG-2 16 kHz | plain stereo |
| `l3_12k_js_24.mp3` | III, MPEG-2.5 12 kHz | joint stereo |
| `l3_11k_mono_16.mp3` | III, MPEG-2.5 11.025 kHz | |
| `l3_8k_mono_8.mp3` | III, MPEG-2.5 8 kHz | |

## Regenerating

Run from the repository root with FFmpeg (with libmp3lame and libtwolame) on the `PATH`
(`FF="ffmpeg -hide_banner -loglevel error -y"`). The source signal, stereo, is

```sh
L='0.25*sin(2*PI*440*t)+0.12*sin(2*PI*(300*t+3000*t*t))+0.35*exp(-30*(t-0.17*floor(t/0.17)))*(random(0)-0.5)+0.08*sin(2*PI*5000*t)'
R='0.25*sin(2*PI*660*t)+0.12*sin(2*PI*(300*t+3000*t*t))+0.35*exp(-30*(t-0.17*floor(t/0.17)))*(random(1)-0.5)+0.05*sin(2*PI*7000*t)'
src() { $FF -f lavfi -i "aevalsrc=exprs=$L|$R:s=$1:d=$2" -c:a pcm_f32le $T/src_$1.wav; }  # rate seconds
lame() { src $1 $2; $FF -i $T/src_$1.wav -c:a libmp3lame "${@:4}" testdata/audio/mp3/$3; }
mp2()  { src $1 $2; $FF -i $T/src_$1.wav "${@:4}" -id3v2_version 0 -f mp2 testdata/audio/mp3/$3; }
```

(a tone, a chirp and decaying noise bursts every 0.17 s, which make LAME switch to short
blocks). Then:

```sh
N="-write_xing 0 -id3v2_version 0"
lame 44100 0.6 l3_44k_js_128.mp3     -b:a 128k -joint_stereo 1 $N
lame 48000 0.4 l3_48k_stereo_160.mp3 -b:a 160k -joint_stereo 0 $N
lame 32000 0.5 l3_32k_mono_56.mp3    -ac 1 -b:a 56k $N
lame 44100 0.3 l3_44k_stereo_320.mp3 -b:a 320k -joint_stereo 0 $N
lame 44100 0.4 l3_44k_vbr_xing.mp3   -q:a 2
lame 44100 0.5 l3_44k_js_64_ms.mp3   -b:a 64k -joint_stereo 1 $N
lame 22050 0.5 l3_22k_js_48.mp3      -b:a 48k -joint_stereo 1 $N
lame 24000 0.5 l3_24k_mono_32.mp3    -ac 1 -b:a 32k $N
lame 16000 0.5 l3_16k_stereo_32.mp3  -b:a 32k -joint_stereo 0 $N
lame 22050 0.5 l3_22k_js_32_ms.mp3   -b:a 32k -joint_stereo 1 $N
lame 12000 0.5 l3_12k_js_24.mp3      -b:a 24k -joint_stereo 1 $N
lame 11025 0.5 l3_11k_mono_16.mp3    -ac 1 -b:a 16k $N
lame 8000  0.5 l3_8k_mono_8.mp3      -ac 1 -b:a 8k $N
src 44100 0.5
$FF -i $T/src_44100.wav -c:a libtwolame -b:a 192k -mode joint_stereo -error_protection 1 \
    -id3v2_version 0 -f mp2 testdata/audio/mp3/l2_44k_js_192_crc.mp2
mp2 48000 0.3 l2_48k_stereo_256.mp2 -c:a mp2 -b:a 256k
mp2 22050 0.5 l2_22k_mono_64.mp2    -c:a mp2 -ac 1 -b:a 64k
mp2 32000 0.5 l2_32k_mono_48.mp2    -c:a mp2 -ac 1 -b:a 48k
mp2 48000 0.5 l2_48k_stereo_64.mp2  -c:a mp2 -b:a 64k

# intensity stereo: set mode_extension bit 0 in every joint stereo frame
cargo run --release -p mkff-mp3 --example mp3tool -- setis testdata/audio/mp3/l3_44k_js_64_ms.mp3 testdata/audio/mp3/l3_44k_js_64_is.mp3
cargo run --release -p mkff-mp3 --example mp3tool -- setis testdata/audio/mp3/l3_22k_js_32_ms.mp3 testdata/audio/mp3/l3_22k_js_32_is.mp3

# Layer I (FFmpeg has no Layer I encoder): the crate's test-only encoder, same source signal
src 48000 0.3; src 32000 0.4
MKFF_MP3_L1_SRC=$T cargo test -p mkff-mp3 --release make_layer1_fixtures -- --ignored

# references
for f in testdata/audio/mp3/*.mp[123]; do
  case $f in *.mp1) dec=mp1float;; *.mp2) dec=mp2float;; *) dec=mp3float;; esac
  rate=$(ffprobe -v error -show_entries stream=sample_rate -of csv=p=0 $f)
  start=$((rate * 16 / 100))
  $FF -c:a $dec -i $f -af atrim=start_sample=$start:end_sample=$((start + 1536)) -f f32le ${f%.*}.ref
done
rm testdata/audio/mp3/l3_22k_js_32_is.ref
```

Note: FFmpeg's default `mp2` / `mp1` decoders are fixed point (16-bit output); use the
`*float` decoders above for references.
