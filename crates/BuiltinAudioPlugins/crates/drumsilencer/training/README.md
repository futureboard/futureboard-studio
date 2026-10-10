# Drum Silencer training

The network in `../model/drumsilencer.dsil` is trained here. The Rust DSP
(`../src`) runs the same framing, features and arithmetic; `export.py` writes a
parity vector the crate's tests replay, so a mismatch fails `cargo test`.

| File | What |
|---|---|
| `dsil.py` | The contract: window pair, framing, features, the network (`DrumSilencerNet2`). |
| `train.py` | Training on MUSDB18-HQ with on-the-fly remixing. |
| `evaluate.py` | Per-song numbers on the 50 MUSDB18-HQ test songs, plus the ideal-mask ceiling. |
| `export.py` | Checkpoint → `.dsil` weights + `.parity` vector. |
| `run_remote.sh` | The training box's whole run (unpack, train with crash-resume, evaluate, export). |

## Data and licence

**MUSDB18-HQ** (Rafii et al., Zenodo record 3338373, 22.7 GB). Its licence is
for non-commercial research use. Weights trained on it inherit that question:
decide before shipping them in a commercial build.

## Running

Training runs on the training box only (an RX 6600 under ROCm), never on the
development PC:

```sh
# on the training box, with the venv in ~/drumsil/venv and the code in ~/drumsil/code
nohup sh run_remote.sh v2b --arch v2 --batch 32 > ~/drumsil/runs/v2b.log 2>&1 < /dev/null &
```

`HSA_OVERRIDE_GFX_VERSION=10.3.0` is needed for gfx1032; training uses float16
autocast for the network (the transforms and loss stay float32) and resumes
from its last checkpoint when ROCm faults.

Then copy `runs/<run>/drumsilencer.dsil` and its `.parity` here as
`../model/drumsilencer.dsil` and run `cargo test -p drumsilencer`.

## The parity model

`../src/testdata/random16.dsil` is an untrained 16-unit network:

```sh
python export.py --random --hidden 16 ../src/testdata/random16.dsil
```
