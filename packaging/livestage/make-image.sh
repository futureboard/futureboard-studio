#!/bin/sh
# Builds the LiveStage appliance: Alpine Linux that boots on UEFI straight
# into livestage-server, the web UI on port 8730.
#
# Runs inside the builder container (Dockerfile), started by build.ps1 or
# build.sh:
#   /src     the repository, read-only
#   /out     where the image goes
#   /target  cargo's target directory (a cache volume)
# and, with KERNEL_TARBALL set, the kernel to use in place of Alpine's
# linux-lts: a tar.zst made by kernel/build-kernel.sh. With INSTALLER=1 it
# then makes the installer USB stick image too (section 6, below).
#
# The disk (GPT):
#   1  LSBOOT  FAT32  EFI/BOOT/BOOTX64.EFI: one unified kernel image (stub,
#                     kernel, microcode, initramfs, command line). Any UEFI
#                     firmware boots it from the removable-media path, with no
#                     boot loader and nothing to configure.
#   2  lsroot  ext4   the system, mounted read-only
#   3  lssys   ext4   /var/lib/livestage: the setup's answers and settings,
#                     root's alone (the service account cannot write them)
#   4  lsdata  exFAT  /data: the session and recordings, readable on any
#                     Windows or Mac computer the disk is plugged into. Made
#                     anew to fill the disk on the first boot (exFAT cannot
#                     grow).
#
# Nothing is mounted and nothing needs privileges: the filesystems are written
# from directories (mke2fs -d, mtools) or made empty (mkfs.exfat), and placed
# into the disk image by offset.
#
# The installer (INSTALLER=1), livestage-installer-....img, for a USB stick
# that puts the image above onto a computer's own disk:
#   1  LSINSTALL  FAT32  its own unified kernel image, root=LABEL=lsinstall
#   2  lsinstall  ext4   the same Alpine system, read-only, where tty1 runs
#                        livestage-installer instead of the setup and no
#                        LiveStage service runs; the image above, compressed,
#                        is /usr/share/livestage/installer/livestage.img.zst
#                        with its size and sha256 in payload.conf
# Its labels are not the appliance's, so the two never mix up on one machine.

set -eu

ALPINE_VERSION=$(cut -d. -f1,2 /etc/alpine-release)
ESP_MB=${ESP_MB:-128}
DATA_MB=${DATA_MB:-512}
SYS_MB=32
IMAGE_NAME=${IMAGE_NAME:-livestage-alpine${ALPINE_VERSION}-x86_64}
KERNEL_TARBALL=${KERNEL_TARBALL:-}
INSTALLER=${INSTALLER:-0}
INSTALLER_NAME=livestage-installer-alpine${ALPINE_VERSION}-x86_64
HERE=/src/packaging/livestage
WORK=/work
ROOT=$WORK/rootfs
SYS=$WORK/sysfs
IROOT=$WORK/installer-rootfs

# Disk drivers the initramfs needs to find the root partition: SATA, NVMe, USB
# sticks, SD/eMMC, and virtio for virtual machines.
MKINITFS_FEATURES="ata base ext4 mmc nvme scsi usb virtio"
# Kernel messages go to the serial port too; the screen (tty0, last) is the
# console proper, where the boot and the login banner show.
CMDLINE="root=LABEL=lsroot rootfstype=ext4 ro modules=sd-mod,usb-storage,ext4 quiet console=ttyS0,115200 console=tty0"
INSTALLER_CMDLINE="root=LABEL=lsinstall rootfstype=ext4 ro modules=sd-mod,usb-storage,ext4 quiet console=ttyS0,115200 console=tty0"

# Firmware for every Wi-Fi driver in the kernel: the packages holding what
# drivers/net/wireless asks for (modinfo -F firmware), plus cypress (brcm's
# links point there). What is left out upstream does not ship either (b43,
# zd1211, ipw2x00, wil6210).
WIFI_FIRMWARE="
	linux-firmware-ath6k linux-firmware-ath9k_htc linux-firmware-ath10k
	linux-firmware-ath11k linux-firmware-ath12k linux-firmware-atmel
	linux-firmware-brcm linux-firmware-cypress linux-firmware-intel
	linux-firmware-libertas linux-firmware-mediatek linux-firmware-mrvl
	linux-firmware-mwl8k linux-firmware-other linux-firmware-rsi
	linux-firmware-rtlwifi linux-firmware-rtw88 linux-firmware-rtw89
	linux-firmware-ti-connectivity linux-firmware-wfx
"

PACKAGES="
	alpine-base busybox-mdev-openrc busybox-openrc ifupdown-ng
	mkinitfs linux-firmware-rtl_nic intel-ucode amd-ucode
	alsa-lib alsa-utils libgcc tzdata
	wpa_supplicant iw wireless-regdb
	$WIFI_FIRMWARE
	avahi avahi-openrc dbus dbus-openrc
	e2fsprogs e2fsprogs-extra exfatprogs sfdisk partx mount umount blkid
"

# Alpine's kernel, unless one built by kernel/build-kernel.sh is given.
if [ -z "$KERNEL_TARBALL" ]; then
	PACKAGES="$PACKAGES linux-lts"
fi

log() { printf '\n==> %s\n' "$*"; }

# ── 1. livestage-server ─────────────────────────────────────────────────────

log "Building livestage-server (musl, built-in effects only) and livestage-setup"
if [ ! -f /src/apps/native/livestage/webui/dist/index.html ]; then
	echo "warning: the web UI is not built; the server will answer the browser with how to build it"
fi
export CARGO_TARGET_DIR=/target
BINS="--bin livestage-server --bin livestage-setup"
if [ "$INSTALLER" = 1 ]; then
	BINS="$BINS --bin livestage-installer"
fi
# shellcheck disable=SC2086
cargo build --release --locked --manifest-path /src/Cargo.toml \
	-p livestage --no-default-features --features appliance $BINS
mkdir -p "$WORK"
strip -o "$WORK/livestage-server" /target/release/livestage-server
strip -o "$WORK/livestage-setup" /target/release/livestage-setup
if [ "$INSTALLER" = 1 ]; then
	strip -o "$WORK/livestage-installer" /target/release/livestage-installer
fi

# ── 2. The root filesystem ──────────────────────────────────────────────────

log "Installing Alpine $ALPINE_VERSION into the root filesystem"
rm -rf "$ROOT" "$SYS" "$IROOT" "$WORK/esp" "$WORK/iesp"
mkdir -p "$ROOT/etc/apk" "$ROOT/etc/mkinitfs"
cp /etc/apk/repositories "$ROOT/etc/apk/repositories"
# The initramfs is made below from this tree; stop the kernel package's
# trigger from making one of its own inside it.
printf 'features="%s"\ndisable_trigger="yes"\n' "$MKINITFS_FEATURES" >"$ROOT/etc/mkinitfs/mkinitfs.conf"
# shellcheck disable=SC2086
apk add --root "$ROOT" --initdb --no-cache --keys-dir /etc/apk/keys \
	--repositories-file /etc/apk/repositories $PACKAGES
if [ -n "$KERNEL_TARBALL" ]; then
	log "Installing the kernel from $(basename "$KERNEL_TARBALL")"
	zstd -dc "$KERNEL_TARBALL" | tar -C "$ROOT" -xf - boot lib
	kver=$(basename "$(find "$ROOT/lib/modules" -mindepth 1 -maxdepth 1 -type d | head -1)")
	depmod -b "$ROOT" "$kver"
	# Where the UKI step below takes the kernel from.
	ln -sf "vmlinuz-$kver" "$ROOT/boot/vmlinuz-lts"
fi

log "Configuring the system"
# Wi-Fi firmware: Intel's package is everything Intel (Bluetooth, cameras,
# audio DSPs, ...); only its Wi-Fi part stays. Marvell's is mostly network
# switch chips (prestera, 73 MB); its Wi-Fi files stay.
find "$ROOT/lib/firmware/intel" -mindepth 1 -maxdepth 1 ! -name iwlwifi -exec rm -rf {} +
rm -rf "$ROOT/lib/firmware/mrvl/prestera"
# File modes do not survive a Windows checkout: set them here.
(cd "$HERE/rootfs" && find . -type f) | while read -r file; do
	case "$file" in
	./etc/init.d/* | ./etc/acpi/* | ./etc/local.d/*) mode=0755 ;;
	*) mode=0644 ;;
	esac
	install -D -m "$mode" "$HERE/rootfs/$file" "$ROOT/$file"
done
install -m 0755 "$WORK/livestage-server" "$ROOT/usr/bin/livestage-server"
install -m 0755 "$WORK/livestage-setup" "$ROOT/usr/bin/livestage-setup"

# Written at boot on /run: the root filesystem is read-only. The setup's
# settings (/data/system/setup.conf) make the name, network and time zone
# (livestage-config); DHCP writes resolv.conf.
ln -sf /run/resolv.conf "$ROOT/etc/resolv.conf"
ln -sf /run/issue "$ROOT/etc/issue"
ln -sf /run/machine-id "$ROOT/etc/machine-id"
ln -sf /run/hostname "$ROOT/etc/hostname"
ln -sf /run/hosts "$ROOT/etc/hosts"
ln -sf /run/localtime "$ROOT/etc/localtime"
mkdir -p "$ROOT/etc/network"
ln -sf /run/network/interfaces "$ROOT/etc/network/interfaces"
mkdir -p "$ROOT/data" "$ROOT/boot/efi" "$ROOT/var/lib/livestage" "$ROOT/media"
# The data partition is exFAT; the kernel's driver is a module. USB sticks
# come FAT32 too.
printf 'exfat\nvfat\n' >>"$ROOT/etc/modules"
# The MIDI remote (midir) talks to USB MIDI devices through the ALSA
# sequencer; the service account is in audio, which owns /dev/snd/seq.
printf 'snd-seq\nsnd-seq-midi\n' >>"$ROOT/etc/modules"
# avahi answers for NAME.local (the host name, livestage until the setup
# changes it) and announces the web UI.
sed -i 's/^#\{0,1\}publish-workstation=.*/publish-workstation=no/' \
	"$ROOT/etc/avahi/avahi-daemon.conf"

# The service account: plays and records (audio), owns /data/livestage.
chroot "$ROOT" /usr/sbin/addgroup -S livestage 2>/dev/null || true
chroot "$ROOT" /usr/sbin/adduser -S -D -H -h /data/livestage -s /sbin/nologin \
	-G livestage livestage
chroot "$ROOT" /usr/sbin/addgroup livestage audio

rc() { chroot "$ROOT" /sbin/rc-update add "$1" "$2" >/dev/null; }
for s in devfs dmesg mdev hwdrivers; do rc "$s" sysinit; done
for s in modules sysctl hostname bootmisc syslog hwclock livestage-data livestage-config; do rc "$s" boot; done
for s in networking acpid ntpd dbus avahi-daemon livestage-storage livestage local; do rc "$s" default; done
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
if [ "$INSTALLER" = 1 ]; then
	# The same kernel and initramfs; the installer's root by its own label.
	mkdir -p "$WORK/iesp/EFI/BOOT"
	efi-mkuki -c "$INSTALLER_CMDLINE" -r "$ROOT/etc/os-release" \
		-o "$WORK/iesp/EFI/BOOT/BOOTX64.EFI" \
		"$ROOT/boot/vmlinuz-lts" "$ROOT/boot/intel-ucode.img" "$ROOT/boot/amd-ucode.img" \
		"$WORK/initramfs"
	echo "$INSTALLER_CMDLINE" >"$WORK/iesp/EFI/BOOT/cmdline.txt"
fi
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

# exFAT has no owners: the data partition is mounted as the service's, with
# its uid and gid in the mount options.
uid=$(chroot "$ROOT" id -u livestage)
gid=$(chroot "$ROOT" id -g livestage)
sed -i "s/@LIVESTAGE_UID@/$uid/; s/@LIVESTAGE_GID@/$gid/" "$ROOT/etc/fstab"
grep -q "uid=$uid,gid=$gid" "$ROOT/etc/fstab"

# The settings partition: root's, read at boot. The setup on tty1 writes
# setup.conf; livestage.conf is for changes by hand.
mkdir -p "$SYS"
cat >"$SYS/livestage.conf" <<'EOF'
# LiveStage settings for this machine; see /etc/conf.d/livestage.
#LIVESTAGE_HTTP="0.0.0.0:8730"
#LIVESTAGE_ARGS="--output hw:CARD=USB --input hw:CARD=USB --rate 48000 --buffer 128"
EOF
chmod 0644 "$SYS/livestage.conf"

rm -f "$WORK"/*.img
mkfs.vfat -C -F 32 -n LSBOOT "$WORK/esp.img" $((ESP_MB * 1024)) >/dev/null
mcopy -s -i "$WORK/esp.img" "$WORK/esp/EFI" ::/
mke2fs -q -t ext4 -L lsroot -d "$ROOT" "$WORK/root.img" "${ROOT_MB}M"
mke2fs -q -t ext4 -L lssys -m 0 -E root_owner=0:0 -d "$SYS" "$WORK/sys.img" "${SYS_MB}M"
# Empty: livestage's start makes its folders.
truncate -s "${DATA_MB}M" "$WORK/data.img"
mkfs.exfat -L lsdata "$WORK/data.img" >/dev/null

# ── 5. The disk ─────────────────────────────────────────────────────────────

log "Assembling the disk image"
DISK=$WORK/$IMAGE_NAME.img
TOTAL_MB=$((1 + ESP_MB + ROOT_MB + SYS_MB + DATA_MB + 1))
truncate -s "${TOTAL_MB}M" "$DISK"
sfdisk -q "$DISK" <<EOF
label: gpt
unit: sectors
first-lba: 2048
start=2048, size=$((ESP_MB * 2048)), type=uefi, name=LSBOOT
size=$((ROOT_MB * 2048)), type=linux, name=lsroot
size=$((SYS_MB * 2048)), type=linux, name=lssys
size=$((DATA_MB * 2048)), type=EBD0A0A2-B9E5-4433-87C0-68B6B72699C7, name=lsdata
EOF
place() { dd if="$1" of="$DISK" bs=1M seek="$2" conv=notrunc,sparse status=none; }
place "$WORK/esp.img" 1
place "$WORK/root.img" $((1 + ESP_MB))
place "$WORK/sys.img" $((1 + ESP_MB + ROOT_MB))
place "$WORK/data.img" $((1 + ESP_MB + ROOT_MB + SYS_MB))

mkdir -p /out
cp --sparse=always "$DISK" "/out/$IMAGE_NAME.img"
cp "$WORK/esp/EFI/BOOT/BOOTX64.EFI" "/out/$IMAGE_NAME.efi"
(cd /out && sha256sum "$IMAGE_NAME.img" "$IMAGE_NAME.efi" >"$IMAGE_NAME.sha256")

log "Done: the appliance image"
printf '  %s  %s MiB (boot %s, system %s, settings %s, data %s exFAT, fills the disk on first boot)\n' \
	"/out/$IMAGE_NAME.img" "$TOTAL_MB" "$ESP_MB" "$ROOT_MB" "$SYS_MB" "$DATA_MB"
printf '  kernel %s, livestage-server %s KiB\n' "$KVER" "$(($(stat -c %s "$WORK/livestage-server") / 1024))"

[ "$INSTALLER" = 1 ] || exit 0

# ── 6. The installer ────────────────────────────────────────────────────────

log "The installer's system: the same one, with livestage-installer on tty1"
cp -a "$ROOT" "$IROOT"
# efibootmgr adds the new disk's boot entry; zstd unpacks the image.
apk add --root "$IROOT" --no-cache --keys-dir /etc/apk/keys \
	--repositories-file /etc/apk/repositories efibootmgr zstd >/dev/null
# No LiveStage here: no mixer, no settings or data partitions, no storage
# service, and no network services (a Wi-Fi scan in the setup needs none).
for s in livestage livestage-storage avahi-daemon dbus ntpd networking local; do
	chroot "$IROOT" /sbin/rc-update del "$s" default >/dev/null 2>&1 || true
done
for s in livestage-data livestage-config; do
	chroot "$IROOT" /sbin/rc-update del "$s" boot >/dev/null 2>&1 || true
done
rm -f "$IROOT"/etc/init.d/livestage* "$IROOT/etc/conf.d/livestage" \
	"$IROOT/etc/avahi/services/livestage.service" "$IROOT/etc/local.d/livestage-issue.start" \
	"$IROOT/usr/bin/livestage-server" "$IROOT/usr/bin/livestage-setup"
# What /etc linked to on /run (made at boot by livestage-config) is fixed
# here instead.
rm -f "$IROOT/etc/hostname" "$IROOT/etc/hosts" "$IROOT/etc/network/interfaces" \
	"$IROOT/etc/localtime" "$IROOT/etc/issue"
ln -s /usr/share/zoneinfo/UTC "$IROOT/etc/localtime"
(cd "$HERE/installer-rootfs" && find . -type f) | while read -r file; do
	rm -f "$IROOT/$file"
	install -D -m 0644 "$HERE/installer-rootfs/$file" "$IROOT/$file"
done
install -m 0755 "$WORK/livestage-installer" "$IROOT/usr/bin/livestage-installer"

log "The image to install, compressed"
PAYLOAD=$IROOT/usr/share/livestage/installer
mkdir -p "$PAYLOAD"
image_bytes=$(stat -c %s "$DISK")
image_sha=$(sha256sum "$DISK" | cut -d' ' -f1)
zstd -q -T0 -10 -o "$PAYLOAD/livestage.img.zst" "$DISK"
cat >"$PAYLOAD/payload.conf" <<EOF
# What livestage.img.zst holds, uncompressed: made by make-image.sh, checked
# by livestage-installer as it writes the image and again when it reads it
# back from the disk.
IMAGE_NAME="$IMAGE_NAME"
IMAGE_BYTES="$image_bytes"
IMAGE_SHA256="$image_sha"
EOF

log "The installer's filesystems and disk"
iroot_used=$(du -s -B1 "$IROOT" | cut -f1)
IROOT_MB=$(($(mib "$iroot_used") * 9 / 8 + 64))
mkfs.vfat -C -F 32 -n LSINSTALL "$WORK/iesp.img" $((ESP_MB * 1024)) >/dev/null
mcopy -s -i "$WORK/iesp.img" "$WORK/iesp/EFI" ::/
mke2fs -q -t ext4 -L lsinstall -d "$IROOT" "$WORK/iroot.img" "${IROOT_MB}M"
IDISK=$WORK/$INSTALLER_NAME.img
ITOTAL_MB=$((1 + ESP_MB + IROOT_MB + 1))
truncate -s "${ITOTAL_MB}M" "$IDISK"
sfdisk -q "$IDISK" <<EOF
label: gpt
unit: sectors
first-lba: 2048
start=2048, size=$((ESP_MB * 2048)), type=uefi, name=LSINSTALL
size=$((IROOT_MB * 2048)), type=linux, name=lsinstall
EOF
dd if="$WORK/iesp.img" of="$IDISK" bs=1M seek=1 conv=notrunc,sparse status=none
dd if="$WORK/iroot.img" of="$IDISK" bs=1M seek=$((1 + ESP_MB)) conv=notrunc,sparse status=none
cp --sparse=always "$IDISK" "/out/$INSTALLER_NAME.img"
(cd /out && sha256sum "$INSTALLER_NAME.img" >"$INSTALLER_NAME.sha256")

log "Done: the installer"
printf '  %s  %s MiB (boot %s, system %s with the image, %s MiB of it compressed)\n' \
	"/out/$INSTALLER_NAME.img" "$ITOTAL_MB" "$ESP_MB" "$IROOT_MB" \
	"$(($(stat -c %s "$PAYLOAD/livestage.img.zst") / 1048576))"
