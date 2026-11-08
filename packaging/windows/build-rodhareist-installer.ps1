# Build the Rodhareist Windows installer.
#
# Steps:
#   1. cargo rodhareist-vst3 bundle rodhareist_vst3 --release   (skip: -SkipBundle)
#   2. ISCC rodhareist-installer.iss -> target\installer\Rodhareist_<ver>-windows.exe
#
# The version is read from vstplugins\rodhareist_vst3\Cargo.toml, the same
# value the plug-in reports to the host, so the installer cannot disagree with
# the binary it carries. Windows file versions take numbers only, so the
# installer's file version is that value up to its pre-release tag.
#
# Installs the uninstaller in Program Files\Futureboard\Rodhareist and the
# plug-ins in the plug-in folders (VST3 and CLAP under Common Files, VST2 under
# Program Files\VstPlugins); see rodhareist-installer.iss.
#
# Examples:
#   .\build-rodhareist-installer.ps1
#   .\build-rodhareist-installer.ps1 -SkipBundle
#   .\build-rodhareist-installer.ps1 -Iscc "C:\Program Files (x86)\Inno Setup 6\ISCC.exe"

param(
    [string]$Root = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path,
    [string]$Iscc = "",
    [string]$AppVersion = "",
    [switch]$SkipBundle
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$pluginDir = Join-Path $Root "vstplugins\rodhareist_vst3"
$bundleDir = Join-Path $pluginDir "target\bundled"

# ---------------------------------------------------------------------------
# Resolve version
# ---------------------------------------------------------------------------

if (-not $AppVersion) {
    $manifest = Join-Path $pluginDir "Cargo.toml"
    # The first `version =` is [package]'s; [workspace] comes before it and
    # has none.
    $line = Select-String -LiteralPath $manifest -Pattern '^version\s*=\s*"([^"]+)"' |
        Select-Object -First 1
    if (-not $line) {
        throw "No package version found in $manifest"
    }
    $AppVersion = $line.Matches[0].Groups[1].Value
}

# "2026.11.1-beta2.0.0" -> "2026.11.1": the numbers before any tag, at most four.
$fileVersion = (($AppVersion -split '[^0-9.]', 2)[0].Trim('.') -split '\.' |
    Select-Object -First 4) -join '.'
if (-not $fileVersion) {
    throw "Version '$AppVersion' does not start with a number."
}

# ---------------------------------------------------------------------------
# Bundle
# ---------------------------------------------------------------------------

if (-not $SkipBundle) {
    Write-Host "==> cargo rodhareist-vst3 bundle rodhareist_vst3 --release" -ForegroundColor Cyan
    Push-Location $Root
    try {
        cargo rodhareist-vst3 bundle rodhareist_vst3 --release
        if ($LASTEXITCODE -ne 0) {
            throw "Plug-in bundle failed with exit code $LASTEXITCODE."
        }
    }
    finally {
        Pop-Location
    }
}

foreach ($rel in @(
        "Rodhareist.vst3\Contents\x86_64-win\Rodhareist.vst3",
        "Rodhareist.clap",
        "Rodhareist.dll"
    )) {
    if (-not (Test-Path -LiteralPath (Join-Path $bundleDir $rel) -PathType Leaf)) {
        throw "Missing bundled plug-in: $bundleDir\$rel`nRun: cargo rodhareist-vst3 bundle rodhareist_vst3 --release"
    }
}

# ---------------------------------------------------------------------------
# Compile installer
# ---------------------------------------------------------------------------

if (-not $Iscc) {
    $candidates = @(
        $env:INNO_SETUP_PATH,
        (Get-Command "iscc.exe" -ErrorAction SilentlyContinue | ForEach-Object Source),
        "$env:ProgramFiles\Inno Setup 6\ISCC.exe",
        "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
        "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe"
    ) | Where-Object { $_ }
    foreach ($candidate in $candidates) {
        if (Test-Path -LiteralPath $candidate -PathType Container) {
            $candidate = Join-Path $candidate "ISCC.exe"
        }
        if (Test-Path -LiteralPath $candidate -PathType Leaf) {
            $Iscc = $candidate
            break
        }
    }
}
if (-not $Iscc) {
    throw "Inno Setup compiler was not found. Install Inno Setup 6, set INNO_SETUP_PATH, or pass -Iscc."
}

$iss = Join-Path $PSScriptRoot "rodhareist-installer.iss"
$outputName = "Rodhareist_$AppVersion-windows.exe"
$outputExe = Join-Path $Root "target\installer\$outputName"

Write-Host ""
Write-Host "Rodhareist Installer" -ForegroundColor Cyan
Write-Host "Version : $AppVersion (file version $fileVersion)"
Write-Host "Source  : $bundleDir"
Write-Host "Compiler: $Iscc"
Write-Host ""

& $Iscc "/Q" "/DMyAppVersion=$AppVersion" "/DMyFileVersion=$fileVersion" "/DMySourceDir=$bundleDir" $iss
if ($LASTEXITCODE -ne 0) {
    throw "Inno Setup compilation failed with exit code $LASTEXITCODE."
}
if (-not (Test-Path -LiteralPath $outputExe -PathType Leaf)) {
    throw "Inno Setup completed, but the installer was not found: $outputExe"
}

$outputFile = Get-Item -LiteralPath $outputExe
Write-Host "Installer: $outputExe ($([Math]::Round($outputFile.Length / 1MB, 2)) MB)" -ForegroundColor Green
