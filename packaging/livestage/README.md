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

## First setup

On the first boot the screen (tty1) shows the setup, `livestage-setup`, a
text UI. It asks for:

- the machine's name (`NAME.local` on the network);
- the network: DHCP on every wired port, or one port (wired or Wi-Fi) by
  DHCP or with a fixed address, gateway and DNS servers. For Wi-Fi, **Scan for
  networks** lists those in range, strongest first; only the WPA key made
  from the password is saved (`/data/system/setup.conf`, root only), and the
  radio's country comes from the time zone;
- the audio interface (from the sound cards found), sample rate and buffer;
- whether the web UI is open to the network or to this machine only, and
  its port;
- the time zone;
- a console password (the console otherwise logs in as root with none).

Every question has a default, and **Use the defaults** skips them all. The
machine works without anyone touching it: until the setup is done it uses
the defaults (DHCP on every wired port, `livestage`, UTC).

Afterwards tty1 shows the machine's status (the web UI's addresses,
LiveStage, the network, free space) with **Setup** to change the answers,
restart LiveStage, read its log, reboot or power off. A login shell is on
tty2 (Alt+F2), where `livestage-setup` runs the same screens.

The answers are saved in `/data/system/setup.conf`. At boot,
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
| 3 | `lsdata` | ext4       | `/data/livestage`: the session (`show.json`), recordings, settings; `/data/system`: the first setup's answers. Grown to the end of the disk on the first boot. |

The system is mounted read-only, so a power cut cannot damage it; logs and
temporary files are in RAM. The session is saved when the service stops
(power button, `poweroff`, web UI Save), not continuously: save from the web
UI after changing the mix.

## On the machine

- Name, network, time zone, interface, password: the setup on tty1, or
  `livestage-setup` from a shell.
- Other settings: `/data/livestage/livestage.conf` (variables in
  `/etc/conf.d/livestage`; it wins over the setup's), for example
  `LIVESTAGE_ARGS="--no-autosave"`. Then `rc-service livestage restart`.
- Log: `/var/log/livestage.log`. Cards: `aplay -l`,
  `livestage-server --list-devices`.
- The console logs in as `root`, with no password until the setup sets one.
  There is no SSH.
- The power button shuts down cleanly (the take being recorded is closed and
  the session saved).

## Security

The web UI has no login: anyone who can reach port 8730 controls the mixer.
Keep the machine on the show's own network, or close the web UI to this
machine in the setup ("Open to: This machine only").
