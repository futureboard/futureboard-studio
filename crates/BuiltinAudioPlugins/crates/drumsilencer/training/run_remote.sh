#!/bin/sh
# The training box's whole run: wait for MUSDB18-HQ, unpack it, train, then
# measure on the test songs and export the weights. Logs to ~/drumsil/runs/.
#
#   nohup sh run_remote.sh v2 [extra train.py args] > ~/drumsil/runs/v2.log 2>&1 < /dev/null &
#
# ROCm on the RX 6600 (gfx1032, run as gfx1030) now and then dies with a GPU
# memory fault; training resumes from its last checkpoint (every 500 steps).
set -e
RUN=${1:-v2}
shift || true
ROOT=$HOME/drumsil
PY=$ROOT/venv/bin/python
export HSA_OVERRIDE_GFX_VERSION=10.3.0
export HSA_ENABLE_SDMA=0
cd "$ROOT/code"

while pgrep -x aria2c > /dev/null; do sleep 30; done
if [ -z "$(find "$ROOT/data/musdb18hq" -maxdepth 3 -type d -name test 2>/dev/null)" ]; then
    echo "unpacking"
    $PY -c "import zipfile; zipfile.ZipFile('$ROOT/data/musdb18hq.zip').extractall('$ROOT/data/musdb18hq')"
fi
DATA=$(dirname "$(find "$ROOT/data/musdb18hq" -maxdepth 3 -type d -name test | head -1)")
echo "training $RUN on $DATA"
tries=0
until $PY train.py --data "$DATA" --out "$ROOT/runs/$RUN" --resume "$@"; do
    tries=$((tries + 1))
    if [ $tries -ge 30 ]; then
        echo "training failed $tries times; giving up"
        exit 1
    fi
    pkill -f "forkserver.import" || true
    echo "training crashed ($tries); resuming"
    sleep 10
done
$PY evaluate.py "$ROOT/runs/$RUN/best.pt" --data "$DATA" --wav "$ROOT/runs/$RUN/wav"
$PY export.py "$ROOT/runs/$RUN/best.pt" "$ROOT/runs/$RUN/drumsilencer.dsil"
echo "finished $RUN"
