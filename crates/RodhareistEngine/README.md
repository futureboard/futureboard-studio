# RodhareistEngine

A C ABI over the Rodhareist guitar multi-effect core (`rodharerist`), with a
header-only C++17 wrapper.

| File | What it is |
| --- | --- |
| `include/rodhareist_engine.h` | The C API, and the threading and error rules |
| `include/rodhareist_engine.hpp` | `rodhareist::Engine`: RAII, move-only; control calls throw `rodhareist::Error` |
| `src/lib.rs` | The exported functions |
| `tests/cpp/` | A C++ smoke test against the DLL and the static library |

## Build

```powershell
cargo build -p rodhareist_engine --release
```

`target/release` then holds:

- `rodhareist_engine.dll` with its import library `rodhareist_engine.dll.lib`;
- `rodhareist_engine.lib`, the static library. Define `RODHAREIST_STATIC` and
  also link the system libraries Rust needs. To list them, run
  `cargo rustc -p rodhareist_engine --release --lib --crate-type staticlib -- --print native-static-libs`.

On Linux and macOS the same command builds `librodhareist_engine.so` /
`.dylib` and `.a`.

## Use

```cpp
#include "rodhareist_engine.hpp"

rodhareist::Engine rig(48000.0);
rig.loadPreset(3);
rig.setParam("amp_gain", 6.5f);

// Audio thread, per block (mono guitar in, stereo out):
rig.process(guitar, nullptr, outLeft, outRight, frames);
// Report rig.latencySamples() to the host's delay compensation.

// Save and restore:
std::string saved = rig.state();
rig.setState(saved);  // the capture and IR it names must be loaded again
```

### Threads

`process` runs on one audio thread at a time; it never allocates, locks or
blocks. Parameters, state, presets, NAM captures and IRs can be changed from
any other thread while audio runs. Those changes reach the DSP at the start of
the next `process` call, in the order they were made. Tempo, latency, meters
and telemetry can be called from any thread.

### NAM captures and impulse responses

- Load a capture with `loadNam(json, name)`, then set `tone_engine` to 1 to
  play it.
- Load an IR with `loadIr(wavBytes, name)`, then set `cab_model` to
  `rodhareist::cabModelIr()` to play it.

The saved state records only the files' names, so a host restoring a state
loads the files itself.

## Test

```powershell
cargo test -p rodhareist_engine
pwsh crates/RodhareistEngine/tests/cpp/run.ps1
```

The Rust tests call the ABI the way a C host does. They check:

- no heap allocation inside `process`;
- control and audio running on two threads;
- that the header declares every export, and its constants match the library.

The script compiles the C++ smoke test with MSVC against both libraries and
runs it.

## Compatibility

`RODHAREIST_ABI_VERSION` changes on any incompatible change. Parameter indices
are the core's wire table, which only ever grows by appending, so an index
stays valid.
