# Builds the Google Play upload bundle for Solfege Voxtune:
#   1. libcanto_dsp.so (rust/) for arm64-v8a and x86_64, into jniLibs
#   2. build/app/outputs/bundle/release/app-release.aab, signed with the
#      upload key named in android/key.properties
#
# Usage (from anywhere):
#   pwsh apps/mobile/solfege_voxtune/scripts/build-play.ps1
#   pwsh apps/mobile/solfege_voxtune/scripts/build-play.ps1 -SkipRust
#
# Needs: the Android NDK the linkers in rust/.cargo/config.toml point at,
# rustup targets aarch64-linux-android and x86_64-linux-android, and the
# Flutter SDK named by flutter.sdk in android/local.properties (or on PATH).
param(
    [switch]$SkipRust
)

$ErrorActionPreference = 'Stop'
$app = Split-Path -Parent $PSScriptRoot

if (-not $SkipRust) {
    $abis = @{
        'aarch64-linux-android' = 'arm64-v8a'
        'x86_64-linux-android'  = 'x86_64'
    }
    foreach ($target in $abis.Keys) {
        cargo build --release --target $target --manifest-path (Join-Path $app 'rust/Cargo.toml')
        if ($LASTEXITCODE -ne 0) { throw "cargo build for $target failed" }
        $dest = Join-Path $app "android/app/src/main/jniLibs/$($abis[$target])"
        New-Item -ItemType Directory -Force $dest | Out-Null
        Copy-Item (Join-Path $app "rust/target/$target/release/libcanto_dsp.so") $dest -Force
    }
}

$keyProperties = Join-Path $app 'android/key.properties'
if (-not (Test-Path $keyProperties)) {
    throw "android/key.properties is missing: Play refuses a bundle signed with the debug key (see README)"
}

$flutter = 'flutter'
$localProperties = Join-Path $app 'android/local.properties'
if (Test-Path $localProperties) {
    $sdk = (Get-Content $localProperties | Where-Object { $_ -match '^flutter\.sdk=' }) -replace '^flutter\.sdk=', '' -replace '\\\\', '\'
    if ($sdk) { $flutter = Join-Path $sdk 'bin/flutter.bat' }
}

Push-Location $app
try {
    # The same two ABIs as libcanto_dsp.so (android/app/build.gradle.kts).
    & $flutter build appbundle --release --target-platform android-arm64,android-x64
    if ($LASTEXITCODE -ne 0) { throw 'flutter build appbundle failed' }
} finally {
    Pop-Location
}

$aab = Join-Path $app 'build/app/outputs/bundle/release/app-release.aab'
Write-Host "Upload bundle: $aab"
