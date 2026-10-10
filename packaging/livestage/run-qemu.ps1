<#
.SYNOPSIS
Boots the LiveStage appliance image in QEMU with UEFI firmware, on a copy of
the image. The web UI is forwarded to http://127.0.0.1:18730/.

With -Installer it boots the installer image instead, as a USB stick, with an
empty disk to install onto (target.img, -DiskGB, 8 by default); -BootTarget
then boots that disk alone, with the same firmware settings (so the boot
entry the installer added is there).

.EXAMPLE
pwsh packaging/livestage/run-qemu.ps1
pwsh packaging/livestage/run-qemu.ps1 -DiskGB 4 -Headless
pwsh packaging/livestage/run-qemu.ps1 -UsbDiskGB 8     # plus an empty USB drive
pwsh packaging/livestage/run-qemu.ps1 -Installer -DiskGB 8 [-TargetBus nvme]
pwsh packaging/livestage/run-qemu.ps1 -BootTarget [-TargetBus nvme]
#>
param(
    # The image to boot; by default the appliance image, or with -Installer
    # the installer image.
    [string]$Image = "",
    # Host port for the web UI.
    [int]$Port = 18730,
    [int]$MemoryMB = 1024,
    # Boot a copy grown to this size: exercises the first-boot growing of the
    # data partition. 0 keeps the image's size.
    [int]$DiskGB = 0,
    # Plug in an empty USB drive of this size (a sparse file next to the copy,
    # kept between runs). The monitor's `device_del usbstick` pulls it out.
    [int]$UsbDiskGB = 0,
    # No window: the serial console goes to serial.log next to the copy, and
    # the QEMU monitor listens on 127.0.0.1:4445.
    [switch]$Headless,
    # Give the guest's sound card the host's speakers (output only). Without
    # it the card is there but silent: the guest never hears the host's
    # microphone either way.
    [switch]$HostAudio,
    # Hyper-V acceleration (WHPX). Off by default: with it this kernel
    # stalled right after the firmware handed over; plain emulation boots in
    # about two minutes.
    [switch]$Accel,
    # Boot the installer image as a USB stick (xhci), with an empty disk of
    # -DiskGB to install onto: target.img in the work folder, made anew.
    [switch]$Installer,
    # Boot the disk the last -Installer run installed onto, alone.
    [switch]$BootTarget,
    # How the target disk is attached.
    [ValidateSet("virtio", "nvme")]
    [string]$TargetBus = "virtio"
)

$ErrorActionPreference = "Stop"
$repo = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
if ($Installer -and $BootTarget) { throw "-Installer or -BootTarget, not both" }
if (-not $Image) {
    $Image = if ($Installer) {
        "out/livestage-alpine/livestage-installer-alpine3.24-x86_64.img"
    } else {
        "out/livestage-alpine/livestage-alpine3.24-x86_64.img"
    }
}
$imagePath = if ([IO.Path]::IsPathRooted($Image)) { $Image } else { Join-Path $repo $Image }
if (-not $BootTarget -and -not (Test-Path $imagePath)) {
    throw "no image at ${imagePath}: build it with build.ps1$(if ($Installer) { ' -Installer' })"
}

$qemu = (Get-Command qemu-system-x86_64 -ErrorAction SilentlyContinue).Source
if (-not $qemu) { $qemu = "C:\Program Files\qemu\qemu-system-x86_64.exe" }
if (-not (Test-Path $qemu)) { throw "QEMU not found (install it from qemu.org)" }
$share = Join-Path (Split-Path $qemu) "share"
$code = Join-Path $share "edk2-x86_64-code.fd"
if (-not (Test-Path $code)) { throw "UEFI firmware not found: $code" }

# The test runs on copies; the image and QEMU's firmware stay as they are.
$runDir = Join-Path (Split-Path $imagePath) "qemu"
New-Item -ItemType Directory -Force $runDir | Out-Null
$target = Join-Path $runDir "target.img"
$targetVars = Join-Path $runDir "target-efivars.fd"
$targetDrive = @("-drive", "file=$target,format=raw,if=none,id=target") + $(
    if ($TargetBus -eq "nvme") { @("-device", "nvme,serial=livestage-target,drive=target") }
    else { @("-device", "virtio-blk-pci,drive=target") })
$drives = @()
if ($Installer) {
    # The stick is a copy of the installer image; the target an empty disk,
    # sparse so that only what is written takes room. The firmware settings
    # are kept for -BootTarget.
    $stick = Join-Path $runDir "installer-stick.img"
    Copy-Item $imagePath $stick -Force
    if (Test-Path $target) { Remove-Item -LiteralPath $target -Force }
    New-Item -ItemType File $target | Out-Null
    fsutil sparse setflag $target | Out-Null
    $size = if ($DiskGB -gt 0) { $DiskGB } else { 8 }
    $stream = [IO.File]::Open($target, "Open", "ReadWrite")
    try { $stream.SetLength([int64]$size * 1GB) } finally { $stream.Close() }
    $vars = $targetVars
    Copy-Item (Join-Path $share "edk2-i386-vars.fd") $vars -Force
    $drives = @("-device", "qemu-xhci,id=xhci",
        "-drive", "if=none,id=stick,format=raw,file=$stick",
        "-device", "usb-storage,bus=xhci.0,drive=stick,id=installer,bootindex=0") + $targetDrive
    Write-Host "Installer on a USB stick; target disk $target ($size GB, $TargetBus)"
} elseif ($BootTarget) {
    if (-not (Test-Path $target)) { throw "no target disk at ${target}: run with -Installer first" }
    $vars = $targetVars
    if (-not (Test-Path $vars)) { Copy-Item (Join-Path $share "edk2-i386-vars.fd") $vars }
    $drives = $targetDrive
    Write-Host "Booting the installed disk $target ($TargetBus)"
} else {
    $disk = Join-Path $runDir "disk.img"
    $vars = Join-Path $runDir "efivars.fd"
    Copy-Item $imagePath $disk -Force
    Copy-Item (Join-Path $share "edk2-i386-vars.fd") $vars -Force
    if ($DiskGB -gt 0) {
        $stream = [IO.File]::Open($disk, "Open", "ReadWrite")
        try { $stream.SetLength([int64]$DiskGB * 1GB) } finally { $stream.Close() }
    }
    $drives = @("-drive", "file=$disk,format=raw,if=virtio")
}

$audio = if ($HostAudio) {
    @("-audiodev", "dsound,id=snd", "-device", "intel-hda", "-device", "hda-output,audiodev=snd")
} else {
    @("-audiodev", "none,id=snd", "-device", "intel-hda", "-device", "hda-duplex,audiodev=snd")
}
$usb = @()
if ($UsbDiskGB -gt 0) {
    $stick = Join-Path $runDir "usb.img"
    if (-not (Test-Path $stick)) {
        $stream = [IO.File]::Open($stick, "CreateNew", "ReadWrite")
        try { $stream.SetLength([int64]$UsbDiskGB * 1GB) } finally { $stream.Close() }
    }
    $usb = @("-device", "qemu-xhci,id=xhci",
        "-drive", "if=none,id=stick,format=raw,file=$stick",
        "-device", "usb-storage,bus=xhci.0,drive=stick,id=usbstick")
}
$acceleration = if ($Accel) { @("-accel", "whpx,kernel-irqchip=off", "-accel", "tcg") } else { @("-accel", "tcg") }
$display = if ($Headless) {
    @("-display", "none", "-serial", "file:$(Join-Path $runDir 'serial.log')",
      "-monitor", "tcp:127.0.0.1:4445,server,nowait")
} else {
    @("-serial", "file:$(Join-Path $runDir 'serial.log')")
}

$arguments = @(
    "-machine", "q35", "-m", "$MemoryMB", "-smp", "2",

    "-drive", "if=pflash,format=raw,unit=0,readonly=on,file=$code",
    "-drive", "if=pflash,format=raw,unit=1,file=$vars"
) + $drives + @(
    "-nic", "user,model=virtio-net-pci,hostfwd=tcp:127.0.0.1:${Port}-:8730"
) + $usb + $acceleration + $audio + $display

Write-Host "Web UI: http://127.0.0.1:$Port/  (once it has booted)"
& $qemu @arguments
