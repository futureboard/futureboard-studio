# Build the WhiteSharp Windows installer and, with -Upload, publish it to R2.
#
# Steps:
#   1. cargo whitesharp-vst3 bundle whitesharp_vst3 --release   (skip: -SkipBundle)
#   2. ISCC whitesharp-installer.iss -> target\installer\WhiteSharp_<ver>-windows.exe
#   3. -Upload: aws s3 cp -> r2://<Bucket>/whitesharp-release/WhiteSharp_<ver>-windows.exe
#
# The version is read from vstplugins\whitesharp_vst3\Cargo.toml, the same
# value the plug-in reports to the host, so the installer cannot disagree with
# the binary it carries.
#
# The upload name is the contract with the website: its /releases/plugins/
# whitesharp endpoint lists whitesharp-release/ and serves the highest
# WhiteSharp_<version>-windows.<ext> it finds. Nothing else needs publishing.
#
# R2 credentials come from the repository .env (CF_AWSS3_API,
# R2_ACCESS_KEY_ID, R2_SECRET_ACCESS_KEY) and are only passed to the aws CLI
# through its environment.
#
# Examples:
#   .\build-whitesharp-installer.ps1
#   .\build-whitesharp-installer.ps1 -SkipBundle -Upload
#   .\build-whitesharp-installer.ps1 -Iscc "C:\Program Files (x86)\Inno Setup 6\ISCC.exe"

param(
    [string]$Root = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path,
    [string]$Iscc = "",
    [string]$AppVersion = "",
    [switch]$SkipBundle,
    [switch]$Upload,
    [string]$Bucket = "futureboard",
    [string]$Prefix = "whitesharp-release/"
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$pluginDir = Join-Path $Root "vstplugins\whitesharp_vst3"
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

# ---------------------------------------------------------------------------
# Bundle
# ---------------------------------------------------------------------------

if (-not $SkipBundle) {
    Write-Host "==> cargo whitesharp-vst3 bundle whitesharp_vst3 --release" -ForegroundColor Cyan
    Push-Location $Root
    try {
        cargo whitesharp-vst3 bundle whitesharp_vst3 --release
        if ($LASTEXITCODE -ne 0) {
            throw "Plug-in bundle failed with exit code $LASTEXITCODE."
        }
    }
    finally {
        Pop-Location
    }
}

foreach ($rel in @(
        "WhiteSharp.vst3\Contents\x86_64-win\WhiteSharp.vst3",
        "WhiteSharp.clap",
        "WhiteSharp.dll"
    )) {
    if (-not (Test-Path -LiteralPath (Join-Path $bundleDir $rel) -PathType Leaf)) {
        throw "Missing bundled plug-in: $bundleDir\$rel`nRun: cargo whitesharp-vst3 bundle whitesharp_vst3 --release"
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

$iss = Join-Path $PSScriptRoot "whitesharp-installer.iss"
$outputName = "WhiteSharp_$AppVersion-windows.exe"
$outputExe = Join-Path $Root "target\installer\$outputName"

Write-Host ""
Write-Host "WhiteSharp Installer" -ForegroundColor Cyan
Write-Host "Version : $AppVersion"
Write-Host "Source  : $bundleDir"
Write-Host "Compiler: $Iscc"
Write-Host ""

& $Iscc "/Q" "/DMyAppVersion=$AppVersion" "/DMySourceDir=$bundleDir" $iss
if ($LASTEXITCODE -ne 0) {
    throw "Inno Setup compilation failed with exit code $LASTEXITCODE."
}
if (-not (Test-Path -LiteralPath $outputExe -PathType Leaf)) {
    throw "Inno Setup completed, but the installer was not found: $outputExe"
}

$outputFile = Get-Item -LiteralPath $outputExe
Write-Host "Installer: $outputExe ($([Math]::Round($outputFile.Length / 1MB, 2)) MB)" -ForegroundColor Green

if (-not $Upload) {
    exit 0
}

# ---------------------------------------------------------------------------
# Upload to R2
# ---------------------------------------------------------------------------

if (-not (Get-Command "aws" -ErrorAction SilentlyContinue)) {
    throw "The aws CLI is required for -Upload."
}

$envFile = Join-Path $Root ".env"
$settings = @{}
if (Test-Path -LiteralPath $envFile) {
    foreach ($raw in Get-Content -LiteralPath $envFile) {
        $t = $raw.Trim()
        if ($t -eq "" -or $t.StartsWith("#")) { continue }
        $i = $t.IndexOf("=")
        if ($i -lt 1) { continue }
        $settings[$t.Substring(0, $i).Trim()] = $t.Substring($i + 1).Trim().Trim('"').Trim("'")
    }
}
foreach ($key in "CF_AWSS3_API", "R2_ACCESS_KEY_ID", "R2_SECRET_ACCESS_KEY") {
    if (-not $settings[$key]) {
        throw "$key is missing from $envFile"
    }
}

$objectKey = "$Prefix$outputName"
$target = "s3://$Bucket/$objectKey"
Write-Host "==> upload -> $target" -ForegroundColor Cyan

$saved = @{
    AWS_ACCESS_KEY_ID     = $env:AWS_ACCESS_KEY_ID
    AWS_SECRET_ACCESS_KEY = $env:AWS_SECRET_ACCESS_KEY
    AWS_DEFAULT_REGION    = $env:AWS_DEFAULT_REGION
}
try {
    $env:AWS_ACCESS_KEY_ID = $settings["R2_ACCESS_KEY_ID"]
    $env:AWS_SECRET_ACCESS_KEY = $settings["R2_SECRET_ACCESS_KEY"]
    $env:AWS_DEFAULT_REGION = "auto"
    aws s3 cp $outputExe $target `
        --endpoint-url $settings["CF_AWSS3_API"] `
        --content-type "application/vnd.microsoft.portable-executable" `
        --only-show-errors
    if ($LASTEXITCODE -ne 0) {
        throw "Upload failed with exit code $LASTEXITCODE."
    }
}
finally {
    foreach ($name in $saved.Keys) {
        if ($null -eq $saved[$name]) {
            Remove-Item "env:$name" -ErrorAction SilentlyContinue
        }
        else {
            Set-Item -Path "env:$name" -Value $saved[$name]
        }
    }
}

Write-Host "Uploaded: $target" -ForegroundColor Green
Write-Host "The site serves it from /releases/plugins/whitesharp/download?platform=windows within ~2 minutes."
