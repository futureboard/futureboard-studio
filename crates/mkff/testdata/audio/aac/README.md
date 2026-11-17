# AAC test fixtures (`mkff-aac`)

ADTS / LOAS streams made with FFmpeg's native `aac` encoder (FFmpeg git N-117098, 2024-09,
libavcodec 61), and
the first part of FFmpeg's decode of each as 16-bit little-endian interleaved PCM (`.s16`).
FFmpeg is only the fixture generator: the tests compare `mkff-aac`'s decoder against the
`.s16` references and never run FFmpeg.

`FF="ffmpeg -hide_banner -loglevel error -y"`

| File | Content | Command |
|------|---------|---------|
| `mono_44k.aac` | 44.1 kHz mono chirp + tone, 64 kb/s | `$FF -f lavfi -i "aevalsrc='0.5*sin(2*PI*(100+4000*t)*t)+0.1*sin(2*PI*7000*t)':s=44100:d=0.5" -c:a aac -b:a 64k -aac_pns 0 mono_44k.aac` |
| `stereo_48k.aac` | 48 kHz stereo tones + noise (M/S, intensity stereo) | `$FF -f lavfi -i "aevalsrc='0.3*sin(2*PI*440*t)+0.15*sin(2*PI*2500*t)+0.05*(random(0)-0.5)\|0.3*sin(2*PI*440*t)+0.1*sin(2*PI*1250*t)+0.05*(random(1)-0.5)':s=48000:d=0.4" -c:a aac -b:a 128k -aac_pns 0 stereo_48k.aac` |
| `transient_44k.aac` | clicks / noise bursts (short windows, start / stop) | `$FF -f lavfi -i "aevalsrc='if(lt(mod(t\,0.1)\,0.003)\,0.7*sin(2*PI*3000*t)\,0.05*sin(2*PI*300*t))\|if(lt(mod(t+0.05\,0.1)\,0.003)\,0.6*(random(0)-0.5)\,0.05*sin(2*PI*500*t))':s=44100:d=0.3" -c:a aac -b:a 128k -aac_pns 0 transient_44k.aac` |
| `surround51_48k.aac` | 5.1, one tone per channel (configuration 6) | `$FF -f lavfi -i "aevalsrc='0.3*sin(2*PI*300*t)\|0.3*sin(2*PI*500*t)\|0.3*sin(2*PI*700*t)\|0.3*sin(2*PI*60*t)\|0.3*sin(2*PI*1100*t)\|0.3*sin(2*PI*1300*t)':s=48000:d=0.15:c=5.1" -c:a aac -b:a 320k -aac_pns 0 surround51_48k.aac` |
| `pce51_48k.aac` | the same with a program config element (channel configuration 0) | as above with `d=0.1` and `-aac_pce 1`, output `pce51_48k.aac` |
| `pns_44k.aac` | noise + tone with PNS | `$FF -f lavfi -i "aevalsrc='0.2*(random(0)-0.5)+0.2*sin(2*PI*440*t)\|0.2*(random(1)-0.5)+0.2*sin(2*PI*440*t)':s=44100:d=0.3" -c:a aac -b:a 64k -aac_pns 1 pns_44k.aac` |
| `mono_8k.aac` | 8 kHz mono chirp (8 kHz band tables) | `$FF -f lavfi -i "aevalsrc='0.4*sin(2*PI*(100+1500*t)*t)':s=8000:d=0.5" -c:a aac -b:a 24k -aac_pns 0 mono_8k.aac` |
| `stereo_22k.aac` | 22.05 kHz stereo (24 kHz band tables) | `$FF -f lavfi -i "aevalsrc='0.4*sin(2*PI*(100+5000*t)*t)\|0.3*sin(2*PI*800*t)':s=22050:d=0.4" -c:a aac -b:a 48k -aac_pns 0 stereo_22k.aac` |
| `tns_44k.aac` | decaying noise bursts (TNS on long windows) | `$FF -f lavfi -i "aevalsrc='0.8*exp(-60*mod(t\,0.25))*(random(0)-0.5)':s=44100:d=0.5" -c:a aac -b:a 96k -aac_pns 0 tns_44k.aac` |
| `tns_short_48k.aac` | pulse trains (TNS on short windows) | `$FF -f lavfi -i "aevalsrc='0.6*pow(abs(sin(PI*110*t))\,40)*sin(2*PI*1800*t)\|0.5*pow(abs(sin(PI*90*t))\,30)*(random(0)-0.5)':s=48000:d=0.3" -c:a aac -b:a 128k -aac_pns 0 tns_short_48k.aac` |
| `stereo_48k.latm` | `stereo_48k.aac` in LOAS / LATM | `$FF -i stereo_48k.aac -c copy -f latm stereo_48k.latm` |

(In a shell the `\|` above is a plain `|` inside the quoted filter string.)

References, truncated to the sample counts the tests use:

```sh
ref() { $FF -i "$1" -f s16le - | head -c "$3" > "$2"; }
ref mono_44k.aac        mono_44k.s16        $((23552*2))
ref mono_8k.aac         mono_8k.s16         $((5120*2))
ref stereo_48k.aac      stereo_48k.s16      $((8192*4))
ref transient_44k.aac   transient_44k.s16   $((10240*4))
ref surround51_48k.aac  surround51_48k.s16  $((4096*12))
ref pce51_48k.aac       pce51_48k.s16       $((3072*12))
ref stereo_22k.aac      stereo_22k.s16      $((6144*4))
ref pns_44k.aac         pns_44k.s16         $((4096*4))
ref tns_44k.aac         tns_44k.s16         $((16384*2))
ref tns_short_48k.aac   tns_short_48k.s16   $((4096*4))
```

`-aac_pns 0` keeps the random PNS noise out of the sample-exact comparisons; `pns_44k` is
compared by frame energy. Against FFmpeg's float output (`-f f32le`) the decoder reaches
about 136 dB SNR on every fixture except `pns_44k`; the 16-bit references bound the
measurable SNR to roughly 70–95 dB depending on the signal level.

No HE-AAC (SBR / PS) fixture: this FFmpeg build has no HE-AAC encoder (no libfdk-aac; the
Media Foundation encoder is AAC-LC only). The tests cover SBR / PS signalling with
configurations written by the tests themselves.
