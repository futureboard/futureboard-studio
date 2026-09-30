# MIT KEMAR HRTF data

`mit_kemar.hrir` is derived from the MIT Media Lab KEMAR HRTF measurements
("compact" set), <https://sound.media.mit.edu/resources/KEMAR.html>:

> Bill Gardner and Keith Martin, "HRTF Measurements of a KEMAR Dummy-Head
> Microphone", MIT Media Lab Perceptual Computing Technical Report #280, 1994.

The data are Copyright 1994 by the MIT Media Laboratory and are provided free
with no restrictions on use, provided the authors are cited when the data are
used in any research or commercial application.

`tools/kemar_to_hrir.py` made this file from `compact.zip`: minimum-phase
filters with the interaural time difference kept apart, diffuse-field
equalised, flat below 250 Hz, the spectral difference between each
direction and its front/back mirror image scaled by 2.5 above 1 kHz (at most
+3 / -9 dB), 96 taps at 44.1 kHz, quantised to 16 bits.
