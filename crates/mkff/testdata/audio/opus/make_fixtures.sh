#!/bin/sh
# Regenerates the mkff-opus fixtures with ffmpeg + libopus (see README.md). Run from this directory.
set -e
FF="${FFMPEG:-ffmpeg} -hide_banner -loglevel error -y"
T=_work; mkdir -p "$T"
F0='(130+25*sin(2*PI*2.7*t))'
SPEECH="0.25*sin(2*PI*$F0*t)*(0.55+0.45*sin(2*PI*3.1*t))+0.15*sin(2*PI*2*$F0*t)*(0.5+0.5*sin(2*PI*3.1*t))+0.08*sin(2*PI*3*$F0*t)+0.05*sin(2*PI*5*$F0*t)+0.06*(random(0)-0.5)*gt(sin(2*PI*1.3*t),0.6)"
MUSIC_L="0.2*sin(2*PI*220*t)+0.12*sin(2*PI*277.2*t+0.5)+0.1*sin(2*PI*329.6*t)*sin(2*PI*1.5*t)+0.03*(random(0)-0.5)+0.4*lt(mod(t,0.25),0.004)*(random(1)-0.5)+0.05*sin(2*PI*3520*t)*sin(2*PI*0.7*t)"
MUSIC_R="0.18*sin(2*PI*246.9*t)+0.12*sin(2*PI*369.9*t+1.0)+0.08*sin(2*PI*440*t)*sin(2*PI*1.1*t)+0.03*(random(2)-0.5)+0.3*lt(mod(t+0.1,0.25),0.004)*(random(3)-0.5)+0.04*sin(2*PI*7040*t)*sin(2*PI*0.9*t)"
$FF -f lavfi -i "aevalsrc='$SPEECH':s=48000:d=2" "$T/speech.wav"
$FF -f lavfi -i "aevalsrc='$MUSIC_L|$MUSIC_R':s=48000:d=2" "$T/music.wav"
$FF -i "$T/speech.wav" -i "$T/music.wav" -filter_complex \
  "[0:a]pan=stereo|c0=c0|c1=0.7*c0[s];[s][1:a]amix=inputs=2:weights='1 0.4'[o]" -map "[o]" "$T/speechmusic.wav"

enc() { name=$1; shift; $FF "$@" -c:a libopus $ENC "$name.opus"; }
ENC="-application lowdelay -b:a 128k -frame_duration 20"; enc celt_stereo_20ms -t 0.16 -i "$T/music.wav"
ENC="-application lowdelay -b:a 32k -frame_duration 60"; enc celt_stereo_60ms -t 0.18 -i "$T/music.wav"
ENC="-application lowdelay -b:a 64k -frame_duration 2.5"; enc celt_mono_2_5ms -t 0.1 -i "$T/music.wav" -ac 1
ENC="-application lowdelay -b:a 48k -frame_duration 5"; enc celt_mono_5ms -t 0.1 -i "$T/music.wav" -ac 1
ENC="-application lowdelay -b:a 40k -frame_duration 10"; enc celt_mono_10ms -t 0.1 -i "$T/music.wav" -ac 1
ENC="-application voip -b:a 8k -cutoff 4000 -frame_duration 20"; enc silk_nb_mono -t 0.3 -i "$T/speech.wav"
ENC="-application voip -b:a 12k -cutoff 6000 -frame_duration 60"; enc silk_mb_60ms -t 0.24 -i "$T/speech.wav"
ENC="-application voip -b:a 20k -cutoff 8000 -frame_duration 40"; enc silk_wb_stereo_40ms -t 0.16 -i "$T/speechmusic.wav"
ENC="-application voip -b:a 32k -frame_duration 20"; enc hybrid_fb_stereo -t 0.12 -i "$T/speechmusic.wav"
ENC="-application voip -b:a 24k -cutoff 12000 -frame_duration 10"; enc hybrid_swb_10ms -t 0.2 -i "$T/speech.wav"
ENC="-application audio -b:a 160k -mapping_family 1"; enc surround51 -t 0.04 -i "$T/music.wav" -filter_complex \
  "[0:a]pan=5.1|c0=c0|c1=c1|c2=0.5*c0+0.5*c1|c3=0.3*c0|c4=0.8*c0-0.2*c1|c5=0.8*c1-0.2*c0[o]" -map "[o]"
# A hybrid -> CELT switch chosen by the encoder (with a redundancy frame), sliced out of a
# longer encode: packets 68..84 of a 2 s mono stream.
ENC="-application audio -b:a 24k"; enc "$T/transitions" -t 2 -i "$T/speechmusic.wav" -ac 1 \
  -af "volume=enable='gt(t,1)':volume=0.3"
python oggcat.py hybrid_to_celt.opus "$T/transitions.opus[68:84]"
# Streams spliced together (SILK -> CELT -> hybrid -> SILK -> CELT without redundancy): the
# decoder conceals the switches with PLC + cross-fades (RFC 6716 §4.5).
python oggcat.py mode_switch.opus "silk_nb_mono.opus[0:10]" celt_mono_10ms.opus \
  "hybrid_swb_10ms.opus[0:10]" "silk_mb_60ms.opus[0:2]" "celt_mono_2_5ms.opus[0:20]"

for f in *.opus; do
  $FF -c:a libopus -i "$f" -f s16le -ar 48000 "${f%.opus}.s16"
done
rm -rf "$T"
