#!/bin/sh
# Builds the LiveStage appliance: Alpine Linux that boots on UEFI straight
# into livestage-server, the web UI on port 8730.
#
# Runs inside the builder container (Dockerfile), started by build.ps1 or
# build.sh:
#   /src     the repository, read-only
#   /out     where the image goes
#   /target  cargo's target directory (a cache volume)
#
# The disk (GPT):
#   1  LSBOOT  FAT32  EFI/BOOT/BOOTX64.EFI: one unified kernel image (stub,
#                     kernel, microcode, initramfs, command line). Any UEFI
#                     firmware boots it from the removable-media path, with no
#                     boot loader and nothing to configure.
#   2  lsroot  ext4   the system, mounted read-only
#   3  lsdata  ext4   /data: the session and recordings. Grown to the end of
#                     the disk on the first boot.
#
# Nothing is mounted and nothing needs privileges: the filesystems are written
# from directories (mke2fs -d, mtools) and placed into the disk image by offset.

set -eu

ALPINE_VERSION=$(cut -d. -f1,2 /etc/alpine-release)
ESP_MB=${ESP_MB:-128}
DATA_MB=${DATA_MB:-512}
IMAGE_NAME=${IMAGE_NAME:-livestage-alpine${ALPINE_VERSION}-x86_64}
HERE=/src/packaging/livestage
WORK=/work
ROOT=$WORK/rootfs
DATA=$WORK/datafs

# Disk drivers the initramfs needs to find the root partition: SATA, NVMe, USB
# sticks, SD/eMMC, and virtio for virtual machines.
MKINITFS_FEATURES="ata base ext4 mmc nvme scsi usb virtio"
# Kernel messages go to the serial port too; the screen (tty0, last) is the
# console proper, where the boot and the login banner show.
CMDLINE="root=LABEL=lsroot rootfstype=ext4 ro modules=sd-mod,usb-storage,ext4 quiet console=ttyS0,115200 console=tty0"

PACKAGES="
	alpine-base busybox-mdev-openrc busybox-openrc ifupdown-ng
	linux-lts linux-firmware-rtl_nic intel-ucode amd-ucode
	alsa-lib alsa-utils libgcc
	avahi avahi-openrc dbus dbus-openrc
	e2fsprogs e2fsprogs-extra sfdisk partx mount umount blkid
"

log() { printf '\n==> %s\n' "$*"; }

# ── 1. livestage-server ─────────────────────────────────────────────────────

log "Building livestage-server (musl, built-in effects only)"
if [ ! -f /src/apps/native/livestage/webui/dist/index.html ]; then
	echo "warning: the web UI is not built; the server will answer the browser with how to build it"
fi
export CARGO_TARGET_DIR=/target
cargo build --release --locked --manifest-path /src/Cargo.toml \
	-p livestage --no-default-features --bin livestage-server
mkdir -p "$WORK"
strip -o "$WORK/livestage-server" /target/release/livestage-server

# ── 2. The root filesystem ──────────────────────────────────────────────────

log "Installing Alpine $ALPINE_VERSION into the root filesystem"
rm -rf "$ROOT" "$DATA"
mkdir -p "$ROOT/etc/apk" "$ROOT/etc/mkinitfs"
cp /etc/apk/repositories "$ROOT/etc/apk/repositories"
# The initramfs is made below from this tree; stop the kernel package's
# trigger from making one of its own inside it.
printf 'features="%s"\ndisable_trigger="yes"\n' "$MKINITFS_FEATURES" >"$ROOT/etc/mkinitfs/mkinitfs.conf"
# shellcheck disable=SC2086
apk add --root "$ROOT" --initdb --no-cache --keys-dir /etc/apk/keys \
	--repositories-file /etc/apk/repositories $PACKAGES

log "Configuring the system"
# File modes do not survive a Windows checkout: set them here.
(cd "$HERE/rootfs" && find . -type f) | while read -r file; do
	case "$file" in
	./etc/init.d/* | ./etc/acpi/* | ./etc/local.d/*) mode=0755 ;;
	*) mode=0644 ;;
	esac
	install -D -m "$mode" "$HERE/rootfs/$file" "$ROOT/$file"
done
install -m 0755 "$WORK/livestage-server" "$ROOT/usr/bin/livestage-server"

# Written at boot on /run: the root filesystem is read-only.
ln -sf /run/resolv.conf "$ROOT/etc/resolv.conf"
ln -sf /run/issue "$ROOT/etc/issue"
ln -sf /run/machine-id "$ROOT/etc/machine-id"
mkdir -p "$ROOT/data" "$ROOT/boot/efi"
# avahi answers for livestage.local and announces the web UI.
sed -i 's/^#\{0,1\}host-name=.*/host-name=livestage/; s/^#\{0,1\}publish-workstation=.*/publish-workstation=no/' \
	"$ROOT/etc/avahi/avahi-daemon.conf"

# The service account: plays and records (audio), owns /data/livestage.
chroot "$ROOT" /usr/sbin/addgroup -S livestage 2>/dev/null || true
chroot "$ROOT" /usr/sbin/adduser -S -D -H -h /data/livestage -s /sbin/nologin \
	-G livestage livestage
chroot "$ROOT" /usr/sbin/addgroup livestage audio

rc() { chroot "$ROOT" /sbin/rc-update add "$1" "$2" >/dev/null; }
for s in devfs dmesg mdev hwdrivers; do rc "$s" sysinit; done
for s in modules sysctl hostname bootmisc syslog hwclock livestage-data; do rc "$s" boot; done
for s in networking acpid ntpd dbus avahi-daemon livestage local; do rc "$s" default; done
for s in killprocs mount-ro savecache; do rc "$s" shutdown; done

KVER=$(basename "$(find "$ROOT/lib/modules" -mindepth 1 -maxdepth 1 -type d | head -1)")
log "Kernel $KVER"

# ── 3. The unified kernel image ─────────────────────────────────────────────

log "Making the initramfs and the unified kernel image"
mkinitfs -b "$ROOT" -c "$ROOT/etc/mkinitfs/mkinitfs.conf" -o "$WORK/initramfs" "$KVER"
mkdir -p "$WORK/esp/EFI/BOOT"
efi-mkuki -c "$CMDLINE" -r "$ROOT/etc/os-release" -o "$WORK/esp/EFI/BOOT/BOOTX64.EFI" \
	"$ROOT/boot/vmlinuz-lts" "$ROOT/boot/intel-ucode.img" "$ROOT/boot/amd-ucode.img" \
	"$WORK/initramfs"
# The kernel lives in the UKI; the root filesystem does not need its copies.
rm -f "$ROOT"/boot/vmlinuz-* "$ROOT"/boot/initramfs-* "$ROOT"/boot/*-ucode.img \
	"$ROOT"/boot/System.map-* "$ROOT"/boot/config-*
echo "$CMDLINE" >"$WORK/esp/EFI/BOOT/cmdline.txt"

# ── 4. The filesystems ──────────────────────────────────────────────────────

log "Making the filesystems"
mib() { echo $(((($1) + 1048575) / 1048576)); }
root_used=$(du -s -B1 "$ROOT" | cut -f1)
# Room for ext4's own structures and a little growth.
ROOT_MB=$(($(mib "$root_used") * 5 / 4 + 64))

# The data partition starts with the service's folder in it, owned right.
uid=$(chroot "$ROOT" id -u livestage)
gid=$(chroot "$ROOT" id -g livestage)
mkdir -p "$DATA/livestage/recordings"
cat >"$DATA/livestage/livestage.conf" <<'EOF'
# LiveStage settings for this machine; see /etc/conf.d/livestage.
#LIVESTAGE_HTTP="0.0.0.0:8730"
#LIVESTAGE_ARGS="--output hw:CARD=USB --input hw:CARD=USB --rate 48000 --buffer 128"
EOF
chown -R "$uid:$gid" "$DATA/livestage"

rm -f "$WORK"/*.img
mkfs.vfat -C -F 32 -n LSBOOT "$WORK/esp.img" $((ESP_MB * 1024)) >/dev/null
mcopy -s -i "$WORK/esp.img" "$WORK/esp/EFI" ::/
mke2fs -q -t ext4 -L lsroot -d "$ROOT" "$WORK/root.img" "${ROOT_MB}M"
mke2fs -q -t ext4 -L lsdata -m 0 -d "$DATA" "$WORK/data.img" "${DATA_MB}M"

# ── 5. The disk ─────────────────────────────────────────────────────────────

log "Assembling the disk image"
DISK=$WORK/$IMAGE_NAME.img
TOTAL_MB=$((1 + ESP_MB + ROOT_MB + DATA_MB + 1))
truncate -s "${TOTAL_MB}M" "$DISK"
sfdisk -q "$DISK" <<EOF
label: gpt
unit: sectors
first-lba: 2048
start=2048, size=$((ESP_MB * 2048)), type=uefi, name=LSBOOT
size=$((ROOT_MB * 2048)), type=linux, name=lsroot
size=$((DATA_MB * 2048)), type=linux, name=lsdata
EOF
place() { dd if="$1" of="$DISK" bs=1M seek="$2" conv=notrunc,sparse status=none; }
place "$WORK/esp.img" 1
place "$WORK/root.img" $((1 + ESP_MB))
place "$WORK/data.img" $((1 + ESP_MB + ROOT_MB))

mkdir -p /out
cp --sparse=always "$DISK" "/out/$IMAGE_NAME.img"
cp "$WORK/esp/EFI/BOOT/BOOTX64.EFI" "/out/$IMAGE_NAME.efi"
(cd /out && sha256sum "$IMAGE_NAME.img" "$IMAGE_NAME.efi" >"$IMAGE_NAME.sha256")

log "Done"
printf '  %s  %s MiB (boot %s, system %s, data %s, grows on first boot)\n' \
	"/out/$IMAGE_NAME.img" "$TOTAL_MB" "$ESP_MB" "$ROOT_MB" "$DATA_MB"
printf '  kernel %s, livestage-server %s KiB\n' "$KVER" "$(($(stat -c %s "$WORK/livestage-server") / 1024))"
