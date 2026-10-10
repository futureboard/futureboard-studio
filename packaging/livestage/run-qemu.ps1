<#
.SYNOPSIS
Boots the LiveStage appliance image in QEMU with UEFI firmware, on a copy of
the image. The web UI is forwarded to http://127.0.0.1:18730/.

.EXAMPLE
pwsh packaging/livestage/run-qemu.ps1
pwsh packaging/livestage/run-qemu.ps1 -DiskGB 4 -Headless
pwsh packaging/livestage/run-qemu.ps1 -UsbDiskGB 8     # plus an empty USB drive
#>
param(
    [string]$Image = "out/livestage-alpine/livestage-alpine3.24-x86_64.img",
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
    [switch]$Accel
)

$ErrorActionPreference = "Stop"
$repo = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
$imagePath = if ([IO.Path]::IsPathRooted($Image)) { $Image } else { Join-Path $repo $Image }
if (-not (Test-Path $imagePath)) { throw "no image at ${imagePath}: build it with build.ps1" }

$qemu = (Get-Command qemu-system-x86_64 -ErrorAction SilentlyContinue).Source
if (-not $qemu) { $qemu = "C:\Program Files\qemu\qemu-system-x86_64.exe" }
if (-not (Test-Path $qemu)) { throw "QEMU not found (install it from qemu.org)" }
$share = Join-Path (Split-Path $qemu) "share"
$code = Join-Path $share "edk2-x86_64-code.fd"
if (-not (Test-Path $code)) { throw "UEFI firmware not found: $code" }

# The test runs on copies; the image and QEMU's firmware stay as they are.
$runDir = Join-Path (Split-Path $imagePath) "qemu"
New-Item -ItemType Directory -Force $runDir | Out-Null
$disk = Join-Path $runDir "disk.img"
$vars = Join-Path $runDir "efivars.fd"
Copy-Item $imagePath $disk -Force
Copy-Item (Join-Path $share "edk2-i386-vars.fd") $vars -Force
if ($DiskGB -gt 0) {
    $stream = [IO.File]::Open($disk, "Open", "ReadWrite")
    try { $stream.SetLength([int64]$DiskGB * 1GB) } finally { $stream.Close() }
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
    "-drive", "if=pflash,format=raw,unit=1,file=$vars",
    "-drive", "file=$disk,format=raw,if=virtio",
    "-nic", "user,model=virtio-net-pci,hostfwd=tcp:127.0.0.1:${Port}-:8730"
) + $usb + $acceleration + $audio + $display

Write-Host "Web UI: http://127.0.0.1:$Port/  (once it has booted)"
& $qemu @arguments
