# macOS Hardware MIDI Input — Diagnostic Note

## Reported behavior

A macOS hardware MIDI controller was said to appear but not respond in Futureboard Studio. The user later confirmed that no MIDI device is available to test right now.

## Evidence collected

- The native MIDI service’s existing CoreMIDI scan ran through a temporary probe that depended on the same `SphereMidiService` crate.
- CoreMIDI created a client but reported `sources=0 destinations=0`; the service enumerated zero MIDI endpoints. With no input endpoint, this test could not receive controller events.
- `cargo test -p SphereMidiService --lib`: 93 passed, 0 failed. This covers current service and packet/doorbell tests, not the native app’s full input-to-instrument path.
- A full native app build did not complete: the installed Xcode lacks the Metal Toolchain, and native host compilation also reported the CLAP header `external/clap/include/clap/clap.h` missing. The app was not launched, so no track-routing, audio, or recording behavior was verified.
- No product source files were changed.

## Conclusion and limits

The zero-endpoint result is consistent with the user having no MIDI hardware available at present. It does not establish why the original report occurred or prove a Futureboard code defect. There is no evidence yet to justify a source change.

## Resume test when hardware is available

1. Connect the controller and confirm it appears in macOS Audio MIDI Setup.
2. Confirm Futureboard Settings → MIDI Devices lists it as connected and enabled.
3. Run Futureboard with `FUTUREBOARD_MIDI_INPUT_DEBUG=1` and `FUTUREBOARD_MIDI_SETTINGS_DEBUG=1`.
4. Press and release keys on a selected/armed instrument track; record callback, event, and routed counter changes, then separately verify recording if needed.
5. Fix only the first failing layer demonstrated by those results.

## Build note

Native end-to-end verification also requires the Xcode Metal Toolchain and the pinned CLAP SDK checkout in the isolated worktree. These are environment prerequisites, not evidence of a MIDI implementation defect.
