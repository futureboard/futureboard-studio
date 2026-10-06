# LiveStage appliance image

A bootable disk image for a dedicated LiveStage machine: Alpine Linux that
boots on any x86-64 UEFI PC straight into `livestage-server`, with the web UI
on port 8730. Plug in the audio interface and a network cable, power it on,
and open `http://livestage.local:8730/` (or the address the screen shows).

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
`-AlpineVersion` / `--alpine` (3.24), `-SkipWebUI` / `--skip-webui`.

## Install

Write the `.img` to a USB stick, SSD or SD card: balenaEtcher, Rufus (DD
mode), or `dd if=livestage-….img of=/dev/sdX bs=4M conv=fsync`. Boot it with
UEFI (Secure Boot off: the image is not signed).

## Try it in a virtual machine

```bash
pwsh packaging/livestage/run-qemu.ps1              # window
pwsh packaging/livestage/run-qemu.ps1 -Headless -DiskGB 4
```

Boots a copy of the image with QEMU's UEFI firmware; the web UI is at
`http://127.0.0.1:18730/` after about two minutes (plain emulation: with
Hyper-V acceleration, `-Accel`, this kernel stalled right after the
firmware). `-DiskGB` boots it on a bigger disk, which the first boot grows
the data partition into. The guest gets a sound card that plays nowhere
(`-HostAudio` sends it to the host's speakers). Headless, the QEMU monitor
listens on `127.0.0.1:4445` (`system_powerdown` presses the power button).

## What is on the disk

| # | Label    | Filesystem | Contents |
|---|----------|------------|----------|
| 1 | `LSBOOT` | FAT32      | `EFI/BOOT/BOOTX64.EFI`: one unified kernel image (systemd-stub, `linux-lts`, CPU microcode, initramfs, command line). The firmware's removable-media path, so no boot loader and no boot entries. |
| 2 | `lsroot` | ext4, **read-only** | Alpine, OpenRC, ALSA, `livestage-server`. |
| 3 | `lsdata` | ext4       | `/data/livestage`: the session (`show.json`), recordings, settings. Grown to the end of the disk on the first boot. |

The system is mounted read-only, so a power cut cannot damage it; logs and
temporary files are in RAM. The session is saved when the service stops
(power button, `poweroff`, web UI Save), not continuously: save from the web
UI after changing the mix.

## On the machine

- Settings: `/data/livestage/livestage.conf` (variables in
  `/etc/conf.d/livestage`), for example a fixed interface:
  `LIVESTAGE_ARGS="--output hw:CARD=USB --input hw:CARD=USB --rate 48000"`.
  Then `rc-service livestage restart`.
- Log: `/var/log/livestage.log`. Cards: `aplay -l`,
  `livestage-server --list-devices`.
- The console logs in as `root` with no password and there is no SSH. Set a
  password: `mount -o remount,rw / && passwd && mount -o remount,ro /`.
- The power button shuts down cleanly (the take being recorded is closed and
  the session saved).

## Security

The web UI has no login: anyone who can reach port 8730 controls the mixer.
Keep the machine on the show's own network. `LIVESTAGE_HTTP="127.0.0.1:8730"`
closes it to the network.
