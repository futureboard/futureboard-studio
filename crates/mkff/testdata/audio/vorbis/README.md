# Vorbis fixtures (`mkff-vorbis` tests)

Short synthetic clips (tones, a chirp, noise bursts that force short blocks) encoded with
ffmpeg, plus reference decodes. ffmpeg is only used to make these files; the tests never run it.

| File | Encoder | Channels | Rate | Block sizes | Notes |
|------|---------|----------|------|-------------|-------|
| `mono_44k_q3.ogg` | libvorbis q3 | 1 | 44100 | 256 / 2048 | block switching |
| `mono_8k_qm1.ogg` | libvorbis q-1 | 1 | 8000 | 512 / 512 | lowest quality |
| `stereo_22k_q0.ogg` | libvorbis q0 | 2 | 22050 | 1024 / 1024 | coupled, residue 2 |
| `stereo_48k_q8.ogg` | libvorbis q8 | 2 | 48000 | 256 / 2048 | high quality |
| `surround51_48k_q2.ogg` | libvorbis q2 | 6 | 48000 | 256 / 2048 | 5.1, channel reordering |
| `stereo_44k_ffenc.ogg` | ffmpeg `vorbis` | 2 | 44100 | 2048 / 2048 | ffmpeg's native encoder |

`<name>.ref.f32` holds every 11th frame (interleaved little-endian f32, all channels of frames
0, 11, 22, …) of the reference decode, trimmed to the Ogg end granule.
`vp8_vorbis_webm.ref.f32` is the same for the audio of `../../codecs/vp8_vorbis.webm`.

The references come from ffmpeg's native Vorbis decoder, except `stereo_44k_ffenc`: on that
stream (residue type 2 with `end` = 1600, less than `channels × blocksize / 2` = 2048) ffmpeg's
native decoder disagrees with libvorbis on the second channel, while `mkff-vorbis` agrees with
libvorbis (the specification applies `begin` / `end` to the interleaved vector). Its reference is
therefore libvorbis' decode, which ffmpeg's wrapper delivers as 16-bit samples (≈ 81 dB SNR
ceiling).

## Commands

ffmpeg N-117098 (2024-09-22) with libvorbis; `SCRATCH` is any temporary directory.

```sh
SIG='0.35*sin(2*PI*440*t)+0.25*sin(2*PI*(150+1500*t)*t)+0.3*(random(0)-0.5)*lt(mod(t\,0.3)\,0.02)+0.02*(random(1)-0.5)'
SIG2='0.3*sin(2*PI*330*t+0.5*sin(2*PI*3*t))+0.2*sin(2*PI*(2000-900*t)*t)+0.3*(random(2)-0.5)*lt(mod(t+0.1\,0.37)\,0.015)+0.02*(random(3)-0.5)'
ffmpeg -f lavfi -i "aevalsrc=${SIG}:s=44100:d=1.5" -c:a libvorbis -q:a 3 mono_44k_q3.ogg
ffmpeg -f lavfi -i "aevalsrc=${SIG}|${SIG2}:s=48000:d=1.0" -c:a libvorbis -q:a 8 stereo_48k_q8.ogg
ffmpeg -f lavfi -i "aevalsrc=${SIG}|${SIG2}:s=22050:d=1.5" -c:a libvorbis -q:a 0 stereo_22k_q0.ogg
ffmpeg -f lavfi -i "aevalsrc=${SIG}|${SIG2}|0.5*sin(2*PI*220*t)|0.3*sin(2*PI*50*t)|${SIG2}*0.7|${SIG}*0.6:s=48000:d=0.6:c=5.1" \
       -c:a libvorbis -q:a 2 surround51_48k_q2.ogg
ffmpeg -f lavfi -i "aevalsrc=${SIG}|${SIG2}:s=44100:d=1.0" -c:a vorbis -strict -2 stereo_44k_ffenc.ogg
ffmpeg -f lavfi -i "aevalsrc=${SIG}:s=8000:d=2" -c:a libvorbis -q:a -1 mono_8k_qm1.ogg

# full reference decodes (ffmpeg native decoder; libvorbis for the ffmpeg-encoded stream)
for f in mono_44k_q3 mono_8k_qm1 stereo_22k_q0 stereo_48k_q8 surround51_48k_q2; do
  ffmpeg -c:a vorbis -i $f.ogg -f f32le $SCRATCH/$f.f32
done
ffmpeg -c:a libvorbis -i stereo_44k_ffenc.ogg -f f32le $SCRATCH/stereo_44k_ffenc.f32
ffmpeg -c:a vorbis -i ../../codecs/vp8_vorbis.webm -vn -f f32le $SCRATCH/vp8_vorbis_webm.f32

# keep every 11th frame: <channels> = 1, 2 or 6
python - $SCRATCH/$f.f32 $f.ref.f32 <channels> 11 <<'PY'
import sys
src, dst, ch, stride = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4])
d = open(src, 'rb').read(); fb = 4 * ch
open(dst, 'wb').write(b''.join(d[i * fb:(i + 1) * fb] for i in range(0, len(d) // fb, stride)))
PY
```

`bindings/rust/mkff-vorbis/tests/devref.rs` compares against the full (unstrided) decodes when
`MKFF_VORBIS_FULLREF` names the directory holding them.
