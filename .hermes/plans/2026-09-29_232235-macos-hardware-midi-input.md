# macOS Hardware MIDI Input Implementation Plan

> For agentic workers: REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox syntax for tracking.

Goal: Identify and fix the first failing stage in Futureboard Studio’s macOS hardware-MIDI input path so an enabled physical controller reliably produces live instrument playback and/or recording.

Architecture: Preserve the existing native CoreMIDI → `HardwareMidiInput` queue/doorbell → GPUI control-thread drain → track/instance routing → engine/plugin path. First collect evidence at each already-instrumented boundary; then make the smallest root-cause fix at the failing boundary, with focused service/router regression tests and a native smoke test. Do not replace CoreMIDI, add dependencies, or redesign MIDI routing unless evidence proves the current architecture cannot meet the acceptance criteria.

Tech Stack: Rust 2024 workspace, macOS CoreMIDI FFI (`crates/SphereMidiService`), GPUI native Studio (`crates/SphereUIComponents`), built-in/external instrument routing.

Spec: User report only: “ใน macos ตัว hardware MIDI มันมีนะแต่กดแล้วไม่ทำงานใน App” (macOS detects the hardware MIDI device, but pressing its keys does nothing in the app). Root cause and whether the failure affects live monitoring, recording, or both are not established.

## Global Constraints

- Product is the native Rust/GPUI app; do not modify retired `apps/web` code.
- Keep MIDI callbacks off the audio thread and keep callback work real-time safe; do not add UI work, logging, blocking operations, or unbounded buffering to CoreMIDI callbacks.
- Preserve CoreMIDI-specific macOS implementation; this repo intentionally avoids `midir`/CoreFoundation version conflicts on macOS.
- Keep device/track/plugin stable IDs and route to the exact destination instrument instance.
- Preserve unrelated working-tree changes. Initial status showed `? external/vst3sdk` and three untracked `proxmox-jobs.sqlite3*` files; do not stage or alter them.
- Do not assume a macOS MIDI privacy prompt, permission API, or system setting is the cause. Establish evidence and confirm the applicable CoreMIDI behavior before adding permission-handling code.
- macOS checks use `SDKROOT=/Applications/Xcode.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX26.5.sdk` when that SDK exists on the implementation host.

## Current context / assumptions

- `crates/SphereMidiService/src/macos_coremidi.rs` implements CoreMIDI enumeration, input-port creation/connection, and packet callback decoding. `open_inputs` emits opt-in connection diagnostics and returns live connections.
- `crates/SphereMidiService/src/lib.rs` owns `HardwareMidiInput`, its event queue/doorbell, and shared cross-platform decoding/diagnostic counters.
- `crates/SphereUIComponents/src/layout/midi_input_router.rs` syncs enabled devices, drains events, resolves track targets, and dispatches MIDI to the engine/hosted plugin.
- `crates/SphereUIComponents/src/layout/audio_transport.rs` starts the doorbell-driven control task and periodically syncs configured devices.
- A diagnostic switch already exists: `FUTUREBOARD_MIDI_INPUT_DEBUG=1`. The router logs counters and routed/unrouted outcomes off the CoreMIDI callback. `FUTUREBOARD_MIDI_SETTINGS_DEBUG=1` provides device and CoreMIDI scan/connection diagnostics (confirm the exact environment-variable implementation before relying on spelling; the helper is named `midi_settings_debug_enabled`).
- Hardware MIDI routing accepts tracks that are armed or selected, subject to track input assignment/channel filter and a resolvable instrument (or a MIDI recording path). A detected device alone does not establish that input is enabled or has a valid target.
- No live reproduction, MIDI hardware, macOS permission state, or runtime logs have been inspected in this planning turn. Treat the implementation diagnosis below as a hypothesis tree, not a claim about the root cause.

## Review Focus

- Device enumerates but is not enabled/connected in persisted MIDI settings: UI and runtime must agree about enabled state and resolved device identity.
- CoreMIDI input port/source connection fails or callback never fires: expose the actual failure stage without callback-thread logging and retry policy must remain safe.
- Callback fires but packet parser yields no events: preserve running-status and multi-message packet behavior, and test malformed/fragmented input boundaries.
- Events are decoded but route to no target: distinguish track assignment, selected/armed state, channel filter, track type, and plugin-instance resolution.
- Event reaches a target but no sound is heard: verify the real engine/plugin preview dispatch and distinguish MIDI routing from audio-output/plugin readiness.

## Step-by-step tasks

### Task 1: Reproduce and identify the first failing boundary

**Files:**
- Read only: `crates/SphereMidiService/src/lib.rs`
- Read only: `crates/SphereMidiService/src/macos_coremidi.rs`
- Read only: `crates/SphereUIComponents/src/layout/midi_input_router.rs`
- Read only: `crates/SphereUIComponents/src/layout/audio_transport.rs`
- No source edits in this task.

- [ ] **Step 1: Record host and repository state**

Run from the repository root:

```bash
git status --short
git branch --show-current
sw_vers
```

Expected: branch `dev`; preserve the pre-existing untracked paths listed above. `sw_vers` must identify macOS; if not, mark native runtime reproduction blocked and do not claim the macOS path was tested.

- [ ] **Step 2: Launch the native app with existing diagnostics enabled**

Build/launch using the repository’s established native run workflow (inspect `README.md` and `apps/native/studio/Cargo.toml`; do not invent a run target). Set these variables only for the app process:

```bash
FUTUREBOARD_MIDI_INPUT_DEBUG=1 FUTUREBOARD_MIDI_SETTINGS_DEBUG=1 <repository-native-launch-command>
```

Expected evidence: device scan identifies the controller; input settings show whether it is enabled/connected; CoreMIDI logs whether a source and input port were connected; pressing a key should change diagnostics counters. If the repository’s launch workflow cannot pass environment variables, launch the built native binary from a shell with those variables.

- [ ] **Step 3: Exercise controlled app states and classify the failure**

With a known instrument track and audio output working, test the same key press in these states: (a) controller enabled, instrument track selected but not armed; (b) selected instrument track armed; (c) track input explicitly assigned to the detected controller; (d) transport stopped, then recording armed and transport running. Record log deltas and whether sound/recorded notes occur.

Classify using evidence:

1. `callbacks=0`: investigate CoreMIDI source resolution, input-port connect status, permissions/system routing, and connection lifetime.
2. callbacks/messages increase but decoded `events=0` or malformed/ignored rises: investigate packet/running-status decoding.
3. decoded events increase but `routed=0`/unrouted increases or router says no target: investigate assignment/selection/arming/channel filter/target resolution.
4. routed increases but no sound: inspect preview dispatch, engine availability, plugin sink readiness, and audio output; do not change CoreMIDI speculatively.
5. Sound works but recording does not: trace capture-take lifecycle and recording destination independently.

Expected: one first failing stage is supported by observed output, or the task is marked blocked with exact missing evidence (for example, no macOS machine/controller available). Do not proceed with a guessed fix.

### Task 2: Pin the proven failure with the smallest regression test

**Files:**
- Test placement depends on the Task 1 evidence. Prefer existing `#[cfg(test)]` modules in `crates/SphereMidiService/src/lib.rs` for parser/input-service behavior and `crates/SphereUIComponents/src/layout/midi_input_router.rs` for track-target resolution. Modify no unrelated tests.
- If the failure is solely a runtime CoreMIDI/engine integration issue with no deterministic seam, add a small pure helper at the actual faulty boundary and test that helper; do not fabricate CoreMIDI or audio-device availability.

- [ ] **Step 1: Write one failing test before production code changes**

Test the specific observed regression, not generic MIDI functionality. Examples of valid assertions based on evidence:

- Device identity resolution selects the same CoreMIDI source as enumeration for duplicate names/stable-ID suffixes.
- Packet decoder emits the expected NoteOn/NoteOff for the actual byte sequence that failed.
- Router resolves the tested enabled device and selected/armed track to the expected stable track and instrument-instance IDs.
- Doorbell/drain test proves a queued message is observable after a ring without periodic polling.

Use the repository’s existing test utilities/types. Test code must be derived from the actual API after reading its definition; do not copy a hypothetical signature into the repository.

- [ ] **Step 2: Run the focused test and verify it fails for the intended assertion**

For service tests:

```bash
export SDKROOT="/Applications/Xcode.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX26.5.sdk"
cargo test -p SphereMidiService <focused_test_name> --lib
```

For router tests:

```bash
export SDKROOT="/Applications/Xcode.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX26.5.sdk"
cargo test -p sphere_ui_components <focused_test_name> --lib
```

Expected: test compiles and fails at the new behavior assertion (not due to unrelated build errors). If the failure is runtime-only and the behavior cannot be deterministically modeled, document the observed test limitation and do not create a misleading mock-only test.

### Task 3: Apply the minimal root-cause fix at the proven boundary

**Files:** select only from the observed failing path:
- `crates/SphereMidiService/src/macos_coremidi.rs` — only if enumeration/source lookup/CoreMIDI port creation/connection/callback parsing is proven faulty.
- `crates/SphereMidiService/src/lib.rs` — only if shared connection synchronization, event queue/doorbell, decoding, or service lifetime is proven faulty.
- `crates/SphereUIComponents/src/layout/midi_input_router.rs` — only if device-to-track target resolution or engine/plugin dispatch is proven faulty.
- `crates/SphereUIComponents/src/layout/audio_transport.rs` — only if control-task startup/wakeup/draining is proven faulty.
- Other files are allowed only when Task 1 proves the first failure lives there; identify the exact file and caller before touching it.

- [ ] **Step 1: Write down the single root-cause hypothesis and supporting observation**

Before editing, state one concrete hypothesis in the implementation handoff, for example: “The enabled preference resolves to a different duplicate-name CoreMIDI endpoint than `find_source_by_name_or_id`; scan and open disagree because the resolver ignores the collision ordinal.” Tie it to the actual observed logs/test, not this illustrative example.

- [ ] **Step 2: Make the smallest implementation change**

Fix only the faulty stage. Preserve callback real-time constraints, exact device/track/plugin IDs, connection teardown ordering, and existing retry behavior. Do not add a new settings toggle or permission dialog unless the evidence and macOS API documentation establish that it is required.

- [ ] **Step 3: Run focused test; verify RED→GREEN**

Re-run the same focused `cargo test` command from Task 2. Expected: focused test passes. If it still fails, stop and return to diagnosis; do not stack speculative fixes.

- [ ] **Step 4: Commit the isolated code/test change**

Inspect and stage only the intended source/test files. Confirm a usable configured GPG signing key before committing; every commit must be signed. Example after adapting exact paths/message:

```bash
git diff --check
git diff -- <exact-source-and-test-paths>
git add <exact-source-path> <exact-test-path>
git commit -S -m "fix: restore macOS hardware MIDI input"
git verify-commit HEAD
```

Expected: commit succeeds and `git verify-commit HEAD` validates it. If signing is unavailable or fails, stop and report the blocker; do not create an unsigned commit. Do not include the pre-existing unrelated untracked files.

### Task 4: Validate package boundaries and native behavior

**Files:** no additional edits unless a focused check reveals a directly related regression; any further edit returns to Task 2’s failing-test-first cycle.

- [ ] **Step 1: Run formatting check**

```bash
export SDKROOT="/Applications/Xcode.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX26.5.sdk"
cargo fmt --all -- --check
```

Expected: exit status 0 and no formatting diff.

- [ ] **Step 2: Run the narrow package checks matching changed crates**

If `SphereMidiService` changed:

```bash
cargo test -p SphereMidiService --lib
```

If `SphereUIComponents` changed:

```bash
cargo test -p sphere_ui_components --lib
cargo check -p sphere_ui_components
```

Expected: all selected commands exit 0. Run only the relevant subset first; report any pre-existing unrelated failures with exact output rather than broadening scope.

- [ ] **Step 3: Re-run the exact hardware/app scenario with diagnostics**

Repeat the key presses from Task 1, with diagnostics enabled. Expected: callback/message/decode counters advance; routed count advances when a valid destination is configured; the selected/armed instrument audibly responds; and, when recording is armed, the take contains the played notes after stop/commit. If runtime hardware or a plugin is unavailable, state which layer was not verified.

- [ ] **Step 4: Verify final diff and commit state**

```bash
git status --short
git show --stat --oneline HEAD
git verify-commit HEAD
```

Expected: only the scoped signed fix/test commit is attributed to this work, unrelated initial untracked changes remain untouched, and signature verification passes.

## Tests / validation

- Diagnosis is evidence-first and uses existing opt-in counters/logs before any fix.
- TDD: one focused regression test must fail for the observed behavior before the implementation change, then pass after it.
- macOS validation: `cargo fmt --all -- --check`; `cargo test -p SphereMidiService --lib` and/or `cargo test -p sphere_ui_components --lib`; `cargo check -p sphere_ui_components` when the UI crate changes.
- Runtime validation is distinct from compilation: exercise a real hardware key press with a selected/armed instrument and verify audio and recording independently.
- Use the known-good SDKROOT only if that SDK path exists on the implementation machine; otherwise follow the host’s valid macOS SDK and report the difference. Do not claim macOS runtime proof from a non-macOS build.

## Risks, tradeoffs, and open questions

- Root cause is unknown until the diagnostic reproduction distinguishes discovery, enabled-state resolution, CoreMIDI connection, callback decode, doorbell/drain, track routing, plugin/engine dispatch, and audio output.
- “Device exists” may mean it appears in macOS Audio MIDI Setup, appears in Futureboard’s scan, or is enabled in Futureboard preferences; those states are not equivalent. Task 1 records each without assuming which the user meant.
- Live monitoring and recording can fail independently; validate them separately.
- CoreMIDI FFI is unsafe and callback timing is sensitive; any connection/callback change needs explicit ownership and teardown review plus a physical-device smoke test.
- Duplicate port names and hot-plug/connection retries are plausible edge cases, but they are not assigned as fixes absent reproduction evidence.
- If no compatible Mac/controller is available, complete only deterministic tests and report live MIDI behavior as unverified; ask for the diagnostic output rather than shipping a speculative workaround.
