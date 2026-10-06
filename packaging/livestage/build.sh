#!/bin/sh
# Builds the LiveStage appliance image: Alpine Linux that boots on UEFI into
# livestage-server. Needs Docker (or podman as docker) and bun.
#
#   packaging/livestage/build.sh [--out DIR] [--data-mb N] [--alpine 3.24] [--skip-webui]
set -eu

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
out="$repo/out/livestage-alpine"
data_mb=512
alpine=3.24
webui=1

while [ $# -gt 0 ]; do
	case "$1" in
	--out) out=$2; shift 2 ;;
	--data-mb) data_mb=$2; shift 2 ;;
	--alpine) alpine=$2; shift 2 ;;
	--skip-webui) webui=0; shift ;;
	-h | --help) sed -n '2,6p' "$0"; exit 0 ;;
	*) echo "unknown argument: $1" >&2; exit 2 ;;
	esac
done

if [ "$webui" = 1 ]; then
	echo "==> Building the web UI"
	bun run --cwd "$repo/apps/native/livestage/webui" build
fi

tag="futureboard/livestage-builder:alpine$alpine"
echo "==> Builder image $tag"
docker build --build-arg "ALPINE_VERSION=$alpine" -t "$tag" "$here"

mkdir -p "$out"
out=$(cd "$out" && pwd)
docker run --rm \
	-v "$repo:/src:ro" \
	-v "$out:/out" \
	-v livestage-cargo-registry:/root/.cargo/registry \
	-v livestage-cargo-git:/root/.cargo/git \
	-v "livestage-target-alpine$alpine:/target" \
	-e "DATA_MB=$data_mb" \
	"$tag" sh /src/packaging/livestage/make-image.sh

echo
echo "Write $out/livestage-alpine$alpine-x86_64.img to a USB stick or disk"
echo "(dd, balenaEtcher, Rufus in DD mode), then boot it in UEFI mode."
