<#
.SYNOPSIS
Builds the LiveStage appliance image: Alpine Linux that boots on UEFI into
livestage-server. Needs Docker (Linux containers) and bun.

.EXAMPLE
pwsh packaging/livestage/build.ps1
pwsh packaging/livestage/build.ps1 -DataMB 2048 -Out D:\images
#>
param(
    # Where the image goes.
    [string]$Out = "out/livestage-alpine",
    # The data partition in the image. It grows to the end of the disk on the
    # first boot anyway; this is only the image's size.
    [int]$DataMB = 512,
    [string]$AlpineVersion = "3.24",
    # Use the web UI already in apps/native/livestage/webui/dist.
    [switch]$SkipWebUI
)

$ErrorActionPreference = "Stop"
$repo = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path

if (-not $SkipWebUI) {
    Write-Host "==> Building the web UI"
    bun run --cwd (Join-Path $repo "apps/native/livestage/webui") build
    if ($LASTEXITCODE -ne 0) { throw "web UI build failed (run 'bun install' at the repository root first?)" }
}

$tag = "futureboard/livestage-builder:alpine$AlpineVersion"
Write-Host "==> Builder image $tag"
docker build --build-arg "ALPINE_VERSION=$AlpineVersion" -t $tag $PSScriptRoot
if ($LASTEXITCODE -ne 0) { throw "docker build failed" }

$outDir = if ([IO.Path]::IsPathRooted($Out)) { $Out } else { Join-Path $repo $Out }
New-Item -ItemType Directory -Force $outDir | Out-Null
$outDir = (Resolve-Path $outDir).Path

# Cargo's registry and target directory live in volumes: the second build is
# quick, and the Windows target/ is never touched.
docker run --rm `
    -v "${repo}:/src:ro" `
    -v "${outDir}:/out" `
    -v livestage-cargo-registry:/root/.cargo/registry `
    -v livestage-cargo-git:/root/.cargo/git `
    -v "livestage-target-alpine${AlpineVersion}:/target" `
    -e "DATA_MB=$DataMB" `
    $tag sh /src/packaging/livestage/make-image.sh
if ($LASTEXITCODE -ne 0) { throw "image build failed" }

Write-Host ""
Write-Host "Write $outDir\livestage-alpine$AlpineVersion-x86_64.img to a USB stick or disk"
Write-Host "(Rufus in DD mode, balenaEtcher, or dd), then boot it in UEFI mode."
