# Futureboard rustysynth patch

Vendored from crates.io `rustysynth` 1.3.6.

Local changes vs upstream:

1. **`sanitize_regions`** — drop invalid instrument regions instead of failing
   the entire SoundFont load. Many real SF2 banks leave inverted/empty loop
   points on NoLoop regions; rejecting the whole bank caused
   `SanityCheckFailed` for fonts that play fine in FluidSynth / other DAWs.
2. **`read_wave_data`** — decode `smpl` as explicit little-endian i16 so
   big-endian hosts match LE hosts.
3. **`Synthesizer::set_percussion_channel` / `is_percussion_channel`** — let
   any channel address the drum banks, not only channel 10, so the
   multitimbral Soundfont Player can put a kit on whichever channel a part
   plays on.
4. **Exclusive class (choke groups)** — `Synthesizer::note_on` chokes every
   earlier voice of the region's exclusive class on the channel before the
   note's own voices start, and `Voice::choke` fades a choked voice out over
   10 ms. Upstream instead reused the first voice of the class it found in
   `VoiceCollection::request_new`: a note with two layers of one class (a
   stereo hi-hat's left and right zones) overwrote its own first layer, and
   an earlier note's remaining layers kept ringing through the choke.
