# LiveStage appliance image

A bootable disk image for a dedicated LiveStage machine: Alpine Linux that
boots on any x86-64 UEFI PC straight into `livestage-server`, with the web UI
on port 8730. Plug in the audio interface and a network cable, power it on,
and open `http://livestage.local:8730/` (or the address the screen shows).
With a screen and keyboard attached, the first boot also offers a short setup
(below).

The server here is the headless Linux build: ALSA, built-in effects only.

## Build

Needs Docker (Linux containers) and bun. Nothing runs as root on the host.

```bash
pwsh packaging/livestage/build.ps1      # Windows
packaging/livestage/build.sh            # Linux, macOS
```

Out comes `out/livestage-alpine/livestage-alpine3.24-x86_64.img`, plus the
bare boot file (`.efi`) and checksums. The first build compiles the server
with full LTO and takes a while; later ones reuse Docker volumes
(`livestage-cargo-*`, `livestage-target-*`).

Options: `-DataMB` / `--data-mb` (the data partition in the image, 512 MiB),
`-AlpineVersion` / `--alpine` (3.24), `-SkipWebUI` / `--skip-webui`,
`-Kernel FILE` / `--kernel FILE` (below).

### A newer kernel

The image uses Alpine's `linux-lts` unless given a kernel built from
kernel.org's sources by `kernel/build-kernel.sh`. On any x86-64 Linux with
a kernel toolchain (gcc, make, bc, flex, bison, perl, openssl, libelf, cpio,
xz, zstd, curl):

```bash
packaging/livestage/kernel/build-kernel.sh 7.2.9 ~/livestage-kernel
```

It checks the download against kernel.org's sha256sums, configures it as
Alpine's linux-lts (`kernel/config-alpine-*`) with LiveStage's changes
(`kernel/livestage.config`: full preemption, no debug info, a per-build
module signing key), and packs `linux-7.2.9-livestage.tar.zst`. Then:

```bash
pwsh packaging/livestage/build.ps1 -Kernel out/livestage-kernel/linux-7.2.9-livestage.tar.zst
```

For a new Alpine release, refresh `config-alpine-*` from its `linux-lts`
package (`boot/config-*`).

## Install

Write the `.img` to a USB stick, SSD or SD card: balenaEtcher, Rufus (DD
mode), or `dd if=livestage-….img of=/dev/sdX bs=4M conv=fsync`. Boot it with
UEFI (Secure Boot off: the image is not signed).

## Install onto a computer

To put LiveStage onto a computer's own disk (an SSD inside it, say) rather
than run it from a stick, build the installer too:

```bash
pwsh packaging/livestage/build.ps1 -Installer      # Windows
packaging/livestage/build.sh --installer           # Linux, macOS
```

Out comes `out/livestage-alpine/livestage-installer-alpine3.24-x86_64.img`
as well. Write it to a USB stick (as above) and boot the computer from it
with UEFI. Its first screen, `livestage-installer`, is a text UI like the
setup's:

1. **Disk**: every disk of the computer, with its size, how it is attached
   (NVMe, SATA, USB, SD/eMMC, virtio), its model and what is on it now
   (Windows, Linux, LiveStage, empty). The installer's own stick, a
   write-protected disk, a disk with a mounted partition and one too small
   (the image plus 1 GiB) are listed with the reason and cannot be chosen.
2. **Settings**: *Set up now* asks the first setup's questions (name,
   network with a Wi-Fi scan, audio interface from this computer's sound
   cards, web UI, time zone, console password) and writes the answers onto
   the new disk, so its first boot goes straight to LiveStage; *On first
   boot* leaves them to the setup on the new system's screen.
3. **Confirm**: the disk, everything on it that is erased, the settings;
   type the disk's name (`nvme0n1`) to go on. Esc goes back a step anywhere
   until here.
4. **Install**: the image is written to the whole disk (MB/s and time left
   shown), read back past the cache and checked against its sha256, then
   the settings go onto the new settings partition (`lssys`, found by its
   partition number on that disk), the console password into the new
   system, and a UEFI boot entry "LiveStage" is added with `efibootmgr`
   (when that fails the disk still boots on most firmware; the screen says
   so). The partition table goes on last: a disk left half-written has
   none rather than a broken system. On an error the screen shows it as it
   came and what is left on the disk.
5. **Done**: remove the USB stick, then Reboot (or Power off, or a Shell).
   The first boot gives the data partition the rest of the disk, as it does
   for an image written by hand.

A shell is on Alt+F2 (root, no password) and behind **Shell**. From a
shell, `livestage-installer --list` lists the disks, and
`livestage-installer --disk /dev/sdX --yes [--setup-conf FILE]` installs
with no questions (a `setup.conf` as the setup writes it), printing its
progress. The installer cannot run LiveStage itself: to try LiveStage
without installing it, write the appliance image to a stick instead.

The installer's partitions are `LSINSTALL` (boot) and `lsinstall` (its
read-only system, with the image as
`/usr/share/livestage/installer/livestage.img.zst` and its size and sha256
in `payload.conf`), so they never mix with an installed LiveStage's.

## Try it in a virtual machine

```bash
pwsh packaging/livestage/run-qemu.ps1              # window
pwsh packaging/livestage/run-qemu.ps1 -Headless -DiskGB 4
```

Boots a copy of the image with QEMU's UEFI firmware; the web UI is at
`http://127.0.0.1:18730/` after about two minutes (plain emulation: with
Hyper-V acceleration, `-Accel`, this kernel stalled right after the
firmware). `-DiskGB` boots it on a bigger disk, which the first boot gives
the data partition. `-UsbDiskGB 8` plugs in an empty USB drive of that size
(a file in the work folder), to try Storage with. The guest gets a sound card that plays nowhere
(`-HostAudio` sends it to the host's speakers). Headless, the QEMU monitor
listens on `127.0.0.1:4445` (`system_powerdown` presses the power button).

The installer: `run-qemu.ps1 -Installer -DiskGB 8` boots the installer
image as a USB stick with an empty 8 GB disk to install onto (`target.img`
in the work folder; `-TargetBus nvme` for an NVMe disk instead of virtio).
Afterwards `run-qemu.ps1 -BootTarget` boots that disk alone, with the same
firmware settings, so the boot entry the installer added is used.

## First setup

On the first boot the screen (tty1) shows the setup, `livestage-setup`, a
text UI. It asks for:

- the machine's name (`NAME.local` on the network);
- the network: DHCP on every wired port, or one port (wired or Wi-Fi) by
  DHCP or with a fixed address, gateway and DNS servers. For Wi-Fi, **Scan for
  networks** lists those in range, strongest first; only the WPA key made
  from the password is saved (`/var/lib/livestage/setup.conf`, root only), and the
  radio's country comes from the time zone;
- the audio interface (from the sound cards found), sample rate and buffer;
- whether the web UI is open to the network or to this machine only, and
  its port;
- where recordings go: the data partition or a USB drive (see Storage);
- the time zone;
- a console password (the console otherwise logs in as root with none).

Every question has a default, and **Use the defaults** skips them all. The
machine works without anyone touching it: until the setup is done it uses
the defaults (DHCP on every wired port, `livestage`, UTC).

Afterwards tty1 shows the machine's status (the web UI's addresses,
LiveStage, the network, free space) with **Setup** to change the answers,
restart LiveStage, read its log, reboot or power off. A login shell is on
tty2 (Alt+F2), where `livestage-setup` runs the same screens.

The answers are saved in `/var/lib/livestage/setup.conf`. At boot,
`/etc/init.d/livestage-config` (`livestage-setup --boot`) turns them into
`/run/hostname`, `/run/hosts`, `/run/network/interfaces`, `/run/localtime`
(which `/etc` links to) and, for Wi-Fi, `/run/wpa_supplicant/PORT.conf`,
which the port's `pre-up` hands to wpa_supplicant.

Wi-Fi firmware is in the image for every Wi-Fi driver in the kernel (PCIe,
M.2 and USB: Intel, MediaTek, Realtek, Ralink, Qualcomm Atheros,
Broadcom/Cypress, Marvell, TI, Redpine, Silicon Labs, Atmel), except what
Linux's firmware collection does not ship (old Broadcom b43, ZyDAS zd1211,
Intel ipw2x00, Qualcomm wil6210). WPA2/WPA3-transition networks
work; WPA3-only (SAE) and enterprise (802.1X) networks do not. The audio interface goes into the
session once it exists, the same place the web UI's Setup page changes it;
before the first start the service passes it on the command line.

## What is on the disk

| # | Label    | Filesystem | Contents |
|---|----------|------------|----------|
| 1 | `LSBOOT` | FAT32      | `EFI/BOOT/BOOTX64.EFI`: one unified kernel image (systemd-stub, `linux-lts`, CPU microcode, initramfs, command line). The firmware's removable-media path, so no boot loader and no boot entries. |
| 2 | `lsroot` | ext4, **read-only** | Alpine, OpenRC, ALSA, `livestage-server`. |
| 3 | `lssys`  | ext4, 32 MiB | `/var/lib/livestage`: the setup's answers (`setup.conf`) and `livestage.conf`. Root's alone. |
| 4 | `lsdata` | **exFAT**  | `/data/livestage`: the session (`show.json`) and recordings. Made anew to fill the disk on the first boot, while it is still empty. |

The data partition is exFAT, typed as a Windows data partition: plug the
disk into a Windows or Mac computer and it shows as a drive called `lsdata`
with the session and the recordings on it (the other partitions stay
hidden). exFAT has no owners or permissions, which is why the settings —
which root reads at boot, and which hold the Wi-Fi key — live on their own
ext4 partition instead. exFAT has no journal either: a power cut in the
middle of a take can leave it needing a repair, which the next boot runs
(`fsck.exfat`); the take being written may be cut short.

The system is mounted read-only, so a power cut cannot damage it; logs and
temporary files are in RAM. The session is saved when the service stops
(power button, `poweroff`, web UI Save), not continuously: save from the web
UI after changing the mix.

## On the machine

- Name, network, time zone, interface, password: the setup on tty1, or
  `livestage-setup` from a shell.
- Other settings: `/var/lib/livestage/livestage.conf` (variables in
  `/etc/conf.d/livestage`; it wins over the setup's), for example
  `LIVESTAGE_ARGS="--no-autosave"`. Then `rc-service livestage restart`.
- Log: `/var/log/livestage.log`. Cards: `aplay -l`,
  `livestage-server --list-devices`.
- The console logs in as `root`, with no password until the setup sets one.
  There is no SSH.
- The power button shuts down cleanly (the take being recorded is closed and
  the session saved).

## Storage

Recordings go to the data partition, or to a USB drive. The web UI's
**Setup → Storage** and the console's setup both show every drive (size,
free space, filesystem), the recording time left, and:

- **Record here**: takes go to the drive's `LiveStage Recordings` folder.
  The choice is saved; when the drive is not plugged in, recordings go to the
  data partition and the page says so, and they go back to the drive when it
  returns (never in the middle of a take).
- **Eject**: unmounts a drive so it can be pulled out.
- **Format**: erases a whole drive into one exFAT partition. Never the
  system's own disk.

Drives are mounted read-only until chosen; exFAT, FAT32, NTFS and ext4 can
be recorded to (FAT32 cannot hold a file over 4 GB). None of this works
while recording. A root service, `livestage-storage`
(`livestage-setup --storage-service`, log `/var/log/livestage-storage.log`),
does the mounting; the mixer asks it on `/run/livestage/storage.sock`.

## Users and lock

Until a user exists the web UI has no login: anyone who can reach port 8730
controls the mixer. **Setup → Users** adds users, each with a name, a PIN
(4–32 characters) and a role:

| Role | May |
|------|-----|
| admin | everything, including users, storage, the audio interface and the remote settings |
| engineer | the whole mix (scenes, undo, recording, playback…), not those |
| musician | their own monitor mixes only (the aux buses or matrices given to them): their send levels and pans, the mix's fader, mute and pan |
| viewer | look only |

The first user must be an admin; from then on every page asks for a name and
PIN. Five wrong tries from one address, or for one name, within five minutes
lock further tries out for 30 s. A log-in lasts until it is unused for 12
hours or the mixer restarts. The users are kept, PINs hashed, in
`/data/livestage/users.json` (next to the session; `--users FILE` puts them
elsewhere). The last admin cannot be removed; to go back to no users, stop
LiveStage and delete the file.

**Lock** (top bar) keeps a page showing everything but changing nothing until
the logged-in user's PIN, or any admin's, is entered (with no users: a PIN
chosen when locking).

## Remote control (OSC and MIDI)

**Setup → Remote** (admins). Both act as an engineer.

- **OSC**, off by default: UDP on the web UI's address and the port chosen
  (8000 unless changed). 127.0.0.1 keeps it to this machine; otherwise anyone
  on the network can control the mixer with it. Addresses, 1-based in the
  order the mixer shows: `/ch/N/fader` (0–1, the fader's travel), `/db`,
  `/mute`, `/pan`, `/solo`, `/name`; the same under `/bus/N`, `/mtx/N`,
  `/master` and `/dca/N` (fader and mute); `/ch/N/send/B/fader|db|pan`;
  `/mtx/N/src/master/…` and `/mtx/N/src/bus/B/…`; `/scene/recall N`,
  `/scene/next`, `/scene/previous`, `/talk`, `/oscillator/on`,
  `/playback/play`, `/playback/stop`, `/vsc`. Send `/livestage/subscribe`
  (again within 60 s to keep it) to be sent every value, then each change; at
  most eight subscribers. A leaf sent with no value is answered with its
  value.
- **MIDI**: USB MIDI devices through the ALSA sequencer (`snd-seq`,
  `snd-seq-midi`, loaded at boot). Pick inputs and outputs; a device
  unplugged is looked for again every 2 s. Map a CC, note or program change to
  an address (or **Learn**: pick the address, move the control); program
  changes can recall scenes (program 0 = scene 1). With feedback on, mapped
  values go back out to the outputs (motor faders, button lights).

The settings are kept in `/data/livestage/remote.json` (`--remote FILE`).

## Security

With no users, the web UI has no login: anyone who can reach port 8730
controls the mixer. Add users (above), keep the machine on the show's own
network, or close the web UI to this machine in the setup ("Open to: This
machine only"). The web UI and OSC are plain, unencrypted HTTP and UDP: PINs
cross the network in the clear.
