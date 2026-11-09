Futureboard includes vendored and patched third-party code.

GPUI is vendored under `crates/gpui` from the Zed project and is licensed
under Apache-2.0. Futureboard carries local patches on top of that upstream
library for native DAW integration.

Current Futureboard GPUI patches include a macOS traffic-light safety guard for
sheet and non-minimizable windows.

See `crates/gpui/PATCHED.md` for the GPUI-specific patch record.

## Opus export (libopus)

Audio export to Ogg Opus links libopus 1.6.1 statically. libopus is built from
the source bundled in the `opusic-sys` crate and is distributed under the
BSD-3-Clause license below. The Rust crates around it are `opus` (Tad
Hardesty, MIT OR Apache-2.0), `opusic-sys` (BSD-3-Clause) and `ogg` (est31 and
contributors, Xiph.org Foundation, BSD-3-Clause). The same text is reproduced
in `packages/shared/OSS-LICENSES.txt`.

```
Copyright 2001-2023 Xiph.Org, Skype Limited, Octasic,
                    Jean-Marc Valin, Timothy B. Terriberry,
                    CSIRO, Gregory Maxwell, Mark Borgerding,
                    Erik de Castro Lopo, Mozilla, Amazon

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions
are met:

- Redistributions of source code must retain the above copyright
notice, this list of conditions and the following disclaimer.

- Redistributions in binary form must reproduce the above copyright
notice, this list of conditions and the following disclaimer in the
documentation and/or other materials provided with the distribution.

- Neither the name of Internet Society, IETF or IETF Trust, nor the
names of specific contributors, may be used to endorse or promote
products derived from this software without specific prior written
permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
``AS IS'' AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT OWNER
OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL,
EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO,
PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR
PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF
LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING
NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

Opus is subject to the royalty-free patent licenses which are
specified at:

Xiph.Org Foundation:
https://datatracker.ietf.org/ipr/1524/

Microsoft Corporation:
https://datatracker.ietf.org/ipr/1914/

Broadcom Corporation:
https://datatracker.ietf.org/ipr/1526/
```

MP3 export, which statically linked the LGPL LAME encoder, was removed in
favour of Opus (#284). MP3 files are still imported and decoded through
Symphonia (MPL-2.0); no MP3 encoder is built.
