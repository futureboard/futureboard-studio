#!/bin/sh
# Builds a kernel.org kernel for the LiveStage appliance, configured as
# Alpine's linux-lts (config-alpine-*) plus livestage.config, and packs it
# for make-image.sh (build.ps1 -Kernel / build.sh --kernel).
#
#   packaging/livestage/kernel/build-kernel.sh 7.2.9 [WORKDIR]
#
# Runs on any x86-64 Linux with a kernel toolchain: gcc, make, bc, flex,
# bison, perl, openssl, libelf, cpio, xz, zstd, curl. Out comes
# WORKDIR/linux-VERSION-livestage.tar.zst:
#   boot/vmlinuz-VERSION-livestage, boot/config-…, boot/System.map-…
#   lib/modules/VERSION-livestage/   (stripped, gzip-compressed, signed)
set -eu

version=${1:?usage: build-kernel.sh VERSION [WORKDIR]}
work=${2:-$HOME/livestage-kernel}
here=$(cd "$(dirname "$0")" && pwd)
base=$(ls "$here"/config-alpine-* | tail -1)
major=${version%%.*}
jobs=$(nproc)

mkdir -p "$work"
cd "$work"

tarball=linux-$version.tar.xz
if [ ! -f "$tarball" ]; then
	echo "==> Downloading $tarball"
	curl -fL --retry 3 -o "$tarball.part" "https://cdn.kernel.org/pub/linux/kernel/v$major.x/$tarball"
	mv "$tarball.part" "$tarball"
fi
echo "==> Checking it against kernel.org's sha256sums"
curl -fsSL "https://cdn.kernel.org/pub/linux/kernel/v$major.x/sha256sums.asc" |
	grep " $tarball\$" | sha256sum -c -

rm -rf "linux-$version"
echo "==> Unpacking"
tar -xf "$tarball"
cd "linux-$version"

echo "==> Configuring from $(basename "$base") + livestage.config"
cp "$base" .config
scripts/kconfig/merge_config.sh -m .config "$here/livestage.config" >/dev/null
make olddefconfig >/dev/null
# merge_config only warns when a value did not stick; fail instead.
grep -q '^CONFIG_LOCALVERSION="-livestage"$' .config
grep -q '^CONFIG_PREEMPT=y$' .config
grep -q '^CONFIG_DEBUG_INFO_NONE=y$' .config
kver=$(make -s kernelrelease)

echo "==> Building $kver with $jobs jobs"
make -j"$jobs" bzImage modules

echo "==> Packing"
stage=$work/stage-$kver
rm -rf "$stage"
mkdir -p "$stage/boot"
make -s INSTALL_MOD_PATH="$stage" INSTALL_MOD_STRIP=1 modules_install
rm -f "$stage/lib/modules/$kver/build" "$stage/lib/modules/$kver/source"
cp arch/x86/boot/bzImage "$stage/boot/vmlinuz-$kver"
cp .config "$stage/boot/config-$kver"
cp System.map "$stage/boot/System.map-$kver"
out=$work/linux-$kver.tar.zst
tar -C "$stage" --owner=0 --group=0 --numeric-owner -cf - boot lib | zstd -q -19 -T0 -o "$out" -f
sha256sum "$out" >"$out.sha256"
echo "==> $out ($(du -h "$out" | cut -f1))"
