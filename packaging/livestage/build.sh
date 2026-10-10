#!/bin/sh
# Builds the LiveStage appliance image: Alpine Linux that boots on UEFI into
# livestage-server. Needs Docker (or podman as docker) and bun.
#
#   packaging/livestage/build.sh [--out DIR] [--data-mb N] [--alpine 3.24] [--skip-webui]
#                                [--kernel linux-VERSION-livestage.tar.zst] [--installer]
#   --installer   also make the installer USB stick image (livestage-installer-....img),
#                 which puts the appliance image onto a computer's own disk
set -eu

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
out="$repo/out/livestage-alpine"
data_mb=512
alpine=3.24
webui=1
kernel=
installer=0

while [ $# -gt 0 ]; do
	case "$1" in
	--out) out=$2; shift 2 ;;
	--data-mb) data_mb=$2; shift 2 ;;
	--alpine) alpine=$2; shift 2 ;;
	--skip-webui) webui=0; shift ;;
	--kernel) kernel=$(cd "$(dirname "$2")" && pwd)/$(basename "$2"); shift 2 ;;
	--installer) installer=1; shift ;;
	-h | --help) sed -n '2,9p' "$0"; exit 0 ;;
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

# A kernel made by kernel/build-kernel.sh in place of Alpine's linux-lts.
set --
if [ -n "$kernel" ]; then
	echo "==> Kernel $kernel"
	set -- -v "$kernel:/kernel.tar.zst:ro" -e KERNEL_TARBALL=/kernel.tar.zst
fi

mkdir -p "$out"
out=$(cd "$out" && pwd)
docker run --rm \
	-v "$repo:/src:ro" \
	-v "$out:/out" \
	-v livestage-cargo-registry:/root/.cargo/registry \
	-v livestage-cargo-git:/root/.cargo/git \
	-v "livestage-target-alpine$alpine:/target" \
	-e "DATA_MB=$data_mb" -e "INSTALLER=$installer" "$@" \
	"$tag" sh /src/packaging/livestage/make-image.sh

echo
echo "Write $out/livestage-alpine$alpine-x86_64.img to a USB stick or disk"
echo "(dd, balenaEtcher, Rufus in DD mode), then boot it in UEFI mode."
if [ "$installer" = 1 ]; then
	echo "Or write $out/livestage-installer-alpine$alpine-x86_64.img to a USB stick and boot"
	echo "the computer from it: it installs LiveStage onto the computer's own disk."
fi
