# Publish an internal alpha build of Futureboard Studio to the internal S3
# storage, where devportal.futureboard.studio lists it for staff to download.
#
# The input is the staged release tree (`cargo package-release`), not the Cargo
# target dir: the staged tree is what a user actually runs, helper binaries and
# CEF included. There is one package; Studio picks Community or Professional
# features at runtime from the licence.
#
# The Windows installer is built from that tree first
# (`packaging/windows/build-installer.ps1`), and it is what staff download.
#
# Bucket layout (all under S3_BUCKET):
#
#   alpha/builds/<buildId>/<setup>.exe     the installer; the portal's download
#   alpha/builds/<buildId>/<archive>.zip   the staged tree, zipped (-IncludeArchive)
#   alpha/builds/<buildId>/build.json      metadata; uploaded LAST
#
# The portal lists `alpha/builds/*/build.json` and offers its first asset, so a
# build becomes visible only once its files are fully uploaded. Builds never share a key, so two people
# publishing at once cannot overwrite each other and there is no index file to
# race on.
#
# Configuration comes from the repository `.env`, with real environment
# variables taking precedence:
#
#   S3_ENDPOINT            https://r2-storage.futureboard.studio
#   S3_REGION              signing region (th-bkk)
#   S3_BUCKET              bucket holding internal builds
#   S3_ACCESS_KEY_ID       S3 access key id
#   S3_SECRET_ACCESS_KEY   S3 secret access key
#
# Examples:
#   .\scripts\publish-alpha.ps1 -Build -Notes "Track header redesign"
#   .\scripts\publish-alpha.ps1 -IncludeArchive
#   .\scripts\publish-alpha.ps1 -WhatIfOnly

[CmdletBinding()]
param(
    # Staged platform folder under out/release/.
    [string]$Platform = "windows-x64",

    # Run `cargo package-release` first. Staging fails while Futureboard is
    # running from that folder; close it first.
    [switch]$Build,

    # Also upload the staged tree as a zip, after the installer.
    [switch]$IncludeArchive,

    # Short release note shown on the portal.
    [string]$Notes = "",

    # Read the release note from a file (Markdown is shown as plain text).
    [string]$NotesFile = "",

    # Publish even when build-info.json says the tree was built from a dirty
    # working copy. The portal shows the flag either way.
    [switch]$AllowDirty,

    # Path to the repository `.env`.
    [string]$EnvFile = "",

    # Resolve everything and build the archive, upload nothing.
    [switch]$WhatIfOnly
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

function Read-DotEnv {
    param([string]$Path)

    $values = @{}
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        return $values
    }
    foreach ($line in Get-Content -LiteralPath $Path) {
        $trimmed = $line.Trim()
        if (-not $trimmed -or $trimmed.StartsWith("#")) { continue }
        $separator = $trimmed.IndexOf("=")
        if ($separator -lt 1) { continue }
        $key = $trimmed.Substring(0, $separator).Trim()
        $value = $trimmed.Substring($separator + 1).Trim()
        if ($value.Length -ge 2 -and (
                ($value.StartsWith('"') -and $value.EndsWith('"')) -or
                ($value.StartsWith("'") -and $value.EndsWith("'")))) {
            $value = $value.Substring(1, $value.Length - 2)
        }
        $values[$key] = $value
    }
    return $values
}

if (-not $EnvFile) {
    $EnvFile = Join-Path $repoRoot ".env"
}
$dotenv = Read-DotEnv -Path $EnvFile

function Get-Config {
    param(
        [string]$Name,
        [switch]$Required
    )

    $value = [Environment]::GetEnvironmentVariable($Name)
    if ([string]::IsNullOrWhiteSpace($value) -and $dotenv.ContainsKey($Name)) {
        $value = $dotenv[$Name]
    }
    if ([string]::IsNullOrWhiteSpace($value)) {
        if ($Required) {
            throw "$Name is not set. Add it to '$EnvFile' or export it before running this script."
        }
        return ""
    }
    return $value.Trim()
}

$endpoint = (Get-Config -Name "S3_ENDPOINT" -Required).TrimEnd("/")
if (-not $endpoint.StartsWith("https://")) {
    throw "S3_ENDPOINT must be an https:// endpoint, got '$endpoint'."
}
$region = Get-Config -Name "S3_REGION"
if (-not $region) { $region = "us-east-1" }

$bucket = Get-Config -Name "S3_BUCKET" -Required:(-not $WhatIfOnly)
$accessKeyId = Get-Config -Name "S3_ACCESS_KEY_ID" -Required:(-not $WhatIfOnly)
$secretAccessKey = Get-Config -Name "S3_SECRET_ACCESS_KEY" -Required:(-not $WhatIfOnly)

if (-not $WhatIfOnly -and -not (Get-Command "aws" -ErrorAction SilentlyContinue)) {
    throw "The AWS CLI was not found on PATH; it drives the multipart upload. Install it from https://aws.amazon.com/cli/."
}

# ---------------------------------------------------------------------------
# Build and read the staged tree
# ---------------------------------------------------------------------------

if ($Build) {
    Write-Host "cargo package-release ..." -ForegroundColor Cyan
    Push-Location $repoRoot
    try {
        & cargo package-release
        if ($LASTEXITCODE -ne 0) { throw "cargo package-release failed with exit code $LASTEXITCODE." }
    }
    finally {
        Pop-Location
    }
}

$stageDir = Join-Path $repoRoot "out\release\$Platform"
$buildInfoPath = Join-Path $stageDir "build-info.json"
if (-not (Test-Path -LiteralPath $buildInfoPath -PathType Leaf)) {
    throw "No staged build at '$stageDir'. Run 'cargo package-release' (or pass -Build) first."
}
$buildInfo = Get-Content -LiteralPath $buildInfoPath -Raw | ConvertFrom-Json

if ($buildInfo.profile -ne "release") {
    throw "'$stageDir' holds a '$($buildInfo.profile)' build; alpha builds are release builds."
}
if (-not ($buildInfo.PSObject.Properties.Name -contains "professional")) {
    throw "'$stageDir' was staged by the old two-edition packager. Re-stage it with 'cargo package-release'."
}
$professional = [bool]$buildInfo.professional
if (-not $Platform.StartsWith("windows-")) {
    throw "Only Windows packages have an installer; '$Platform' cannot be published yet."
}
if ($buildInfo.gitDirty -and -not $AllowDirty) {
    throw @"
The staged build was made from a working copy with uncommitted changes
(commit $($buildInfo.gitCommit) + local edits), so nobody could rebuild it from
the commit it names. Commit and re-stage, or pass -AllowDirty to publish it
marked as dirty.
"@
}

if ($NotesFile) {
    $Notes = (Get-Content -LiteralPath $NotesFile -Raw).Trim()
}
if ($Notes.Length -gt 4000) {
    throw "Release note is $($Notes.Length) characters; keep it under 4000."
}

$commit = [string]$buildInfo.gitCommit
$shortCommit = if ($commit.Length -ge 8) { $commit.Substring(0, 8) } else { "nocommit" }
$branch = (& git -C $repoRoot rev-parse --abbrev-ref HEAD 2>$null)
if ($LASTEXITCODE -ne 0 -or -not $branch) { $branch = "" }
$publisher = (& git -C $repoRoot config user.name 2>$null)
if ($LASTEXITCODE -ne 0 -or -not $publisher) { $publisher = $env:USERNAME }

$publishedAt = (Get-Date).ToUniversalTime()
# Sorts by time as a plain string, and stays unique per publish.
$buildId = "{0}-{1}-{2}" -f $publishedAt.ToString("yyyyMMdd-HHmmss"), $shortCommit, $Platform
$archiveName = "Futureboard-{0}-{1}-alpha-{2}.zip" -f $buildInfo.version, $Platform, $shortCommit
# The in-app updater only takes a Windows asset whose name contains "setup"
# and ends in ".exe"; keep that shape so the same file can be served later.
$installerName = "FutureboardStudioSetup-{0}-alpha-{1}.exe" -f $buildInfo.version, $shortCommit
$prefix = "alpha/builds/$buildId"

# ---------------------------------------------------------------------------
# Archive
# ---------------------------------------------------------------------------

$workDir = Join-Path ([System.IO.Path]::GetTempPath()) "futureboard-alpha-$buildId"
New-Item -ItemType Directory -Force -Path $workDir | Out-Null
$archivePath = Join-Path $workDir $archiveName

try {
    # The installer first: it is what staff download, and it is built from the
    # exact tree being published.
    $installerScript = Join-Path $repoRoot "packaging\windows\build-installer.ps1"
    Write-Host "Building the installer from $stageDir ..." -ForegroundColor Cyan
    & $installerScript -Root $repoRoot -SourceDir $stageDir -AppVersion ([string]$buildInfo.version)
    if (-not $?) { throw "Building the installer failed." }
    $builtInstaller = Join-Path $repoRoot "target\installer\FutureboardStudioSetup.exe"
    $installerPath = Join-Path $workDir $installerName
    Copy-Item -LiteralPath $builtInstaller -Destination $installerPath
    $installer = Get-Item -LiteralPath $installerPath
    $installerDigest = (Get-FileHash -LiteralPath $installerPath -Algorithm SHA256).Hash.ToLowerInvariant()

    if ($IncludeArchive) {
        Write-Host "Archiving $stageDir ..." -ForegroundColor Cyan
        Add-Type -AssemblyName System.IO.Compression
        Add-Type -AssemblyName System.IO.Compression.FileSystem

        # One top-level folder, so extracting next to an older build never mixes
        # the two. `logs/` is runtime output from launching the staged tree.
        $rootName = [System.IO.Path]::GetFileNameWithoutExtension($archiveName)
        $stageFull = (Resolve-Path -LiteralPath $stageDir).Path.TrimEnd("\")
        $files = Get-ChildItem -LiteralPath $stageFull -Recurse -File | Where-Object {
            $relative = $_.FullName.Substring($stageFull.Length + 1)
            -not ($relative -eq "logs" -or $relative.StartsWith("logs\"))
        }

        $stream = [System.IO.File]::Open($archivePath, [System.IO.FileMode]::CreateNew)
        try {
            $zip = [System.IO.Compression.ZipArchive]::new($stream, [System.IO.Compression.ZipArchiveMode]::Create)
            try {
                foreach ($file in $files) {
                    $relative = $file.FullName.Substring($stageFull.Length + 1).Replace("\", "/")
                    [System.IO.Compression.ZipFileExtensions]::CreateEntryFromFile(
                        $zip, $file.FullName, "$rootName/$relative",
                        [System.IO.Compression.CompressionLevel]::Optimal) | Out-Null
                }
            }
            finally {
                $zip.Dispose()
            }
        }
        finally {
            $stream.Dispose()
        }

        $archive = Get-Item -LiteralPath $archivePath
        $digest = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash.ToLowerInvariant()
    }

    $manifest = [ordered]@{
        schemaVersion = 1
        id            = $buildId
        channel       = "alpha"
        version       = [string]$buildInfo.version
        # One package: the licence picks Community or Professional at runtime.
        # `professional` says whether the Professional code is compiled in.
        edition       = "studio"
        professional  = $professional
        platform      = $Platform
        target        = [string]$buildInfo.target
        commit        = $commit
        branch        = $branch
        dirty         = [bool]$buildInfo.gitDirty
        builtAt       = [string]$buildInfo.builtAtUtc
        publishedAt   = $publishedAt.ToString("yyyy-MM-ddTHH:mm:ssZ")
        publishedBy   = $publisher
        notes         = $Notes
        assets        = @(
            [ordered]@{
                name   = $installerName
                key    = "$prefix/$installerName"
                size   = $installer.Length
                sha256 = $installerDigest
            }
        )
    }
    if ($IncludeArchive) {
        $manifest.assets += [ordered]@{
            name   = $archiveName
            key    = "$prefix/$archiveName"
            size   = $archive.Length
            sha256 = $digest
        }
    }
    $manifestJson = $manifest | ConvertTo-Json -Depth 6

    Write-Host ""
    Write-Host "Futureboard alpha -> internal storage" -ForegroundColor Cyan
    Write-Host "Endpoint : $endpoint"
    Write-Host "Bucket   : $(if ($bucket) { $bucket } else { '(not set)' })"
    Write-Host "Build    : $buildId"
    Write-Host "Version  : $($buildInfo.version) ($Platform, Professional code $(if ($professional) { 'included' } else { 'not included' }))"
    Write-Host "Commit   : $commit$(if ($buildInfo.gitDirty) { ' (dirty)' })"
    Write-Host ("Installer: {0} ({1:N1} MB)" -f $installerName, ($installer.Length / 1MB))
    Write-Host "SHA-256  : $installerDigest"
    if ($IncludeArchive) {
        Write-Host ("Archive  : {0} ({1:N1} MB, {2} files)" -f $archiveName, ($archive.Length / 1MB), @($files).Count)
        Write-Host "SHA-256  : $digest"
    }
    Write-Host ""

    if ($WhatIfOnly) {
        Write-Host "-WhatIfOnly set; nothing was uploaded." -ForegroundColor Yellow
        Write-Host $manifestJson
        return
    }

    # Scoped to this process; never written to disk and never echoed.
    $env:AWS_ACCESS_KEY_ID = $accessKeyId
    $env:AWS_SECRET_ACCESS_KEY = $secretAccessKey
    $env:AWS_DEFAULT_REGION = $region
    # Keep the CLI to plain SigV4 payloads; S3-compatible servers vary in their
    # support for the newer streaming checksum trailers.
    $env:AWS_REQUEST_CHECKSUM_CALCULATION = "when_required"
    $env:AWS_RESPONSE_CHECKSUM_VALIDATION = "when_required"
    # The endpoint sits behind Cloudflare, which refuses request bodies over
    # 100 MB and gives up on an origin that takes over 100 s to answer (524).
    # The link behind it is slow enough that the CLI's default ten parallel
    # 8 MB parts hit that limit and the upload is aborted (once while the CLI
    # still exited 0), so parts go one at a time and small. The settings live
    # in a config of this run's own, leaving the user's AWS config alone.
    $awsConfig = Join-Path $workDir "aws-config"
    Set-Content -LiteralPath $awsConfig -Encoding utf8NoBOM -Value @"
[default]
s3 =
    max_concurrent_requests = 1
    multipart_threshold = 5MB
    multipart_chunksize = 5MB
"@
    $env:AWS_CONFIG_FILE = $awsConfig

    function Invoke-Aws {
        param([string[]]$Arguments)

        & aws @Arguments
        if ($LASTEXITCODE -ne 0) {
            throw "aws $($Arguments -join ' ') failed with exit code $LASTEXITCODE."
        }
    }

    # `aws s3 cp` has reported success for an installer that never appeared
    # in the bucket (build 20261120-150838-1aabebfe: build.json listed, the
    # download 404). Read every upload back before build.json makes the build
    # visible. Listed rather than HEAD-requested: the endpoint answers
    # HeadObject with 403 even for objects that are there.
    function Assert-Uploaded {
        param([string]$Key, [long]$Length)

        $listing = & aws s3api list-objects-v2 --bucket $bucket --prefix $Key --endpoint-url $endpoint --output json 2>&1
        if ($LASTEXITCODE -ne 0) {
            throw "Could not list '$Key' after uploading it: $listing"
        }
        # An empty listing has no Contents at all (StrictMode would throw).
        $parsed = ($listing | Out-String) | ConvertFrom-Json
        $contents = if ($parsed.PSObject.Properties["Contents"]) { $parsed.Contents } else { @() }
        $found = @($contents | Where-Object { $_.Key -eq $Key })
        if ($found.Count -eq 0) {
            throw "Uploaded '$Key' but the bucket does not have it; build.json was not published."
        }
        if ([long]$found[0].Size -ne $Length) {
            throw "Uploaded '$Key' ($Length bytes) but the bucket holds $($found[0].Size) bytes; build.json was not published."
        }
    }

    Write-Host "Uploading $installerName ..." -ForegroundColor Cyan
    Invoke-Aws @(
        "s3", "cp", $installerPath, "s3://$bucket/$prefix/$installerName",
        "--endpoint-url", $endpoint,
        "--content-type", "application/vnd.microsoft.portable-executable",
        "--only-show-errors"
    )
    Assert-Uploaded -Key "$prefix/$installerName" -Length $installer.Length

    if ($IncludeArchive) {
        Write-Host "Uploading $archiveName ..." -ForegroundColor Cyan
        Invoke-Aws @(
            "s3", "cp", $archivePath, "s3://$bucket/$prefix/$archiveName",
            "--endpoint-url", $endpoint,
            "--content-type", "application/zip",
            "--only-show-errors"
        )
        Assert-Uploaded -Key "$prefix/$archiveName" -Length $archive.Length
    }

    $manifestFile = Join-Path $workDir "build.json"
    Set-Content -LiteralPath $manifestFile -Value $manifestJson -Encoding utf8NoBOM
    Write-Host "Publishing $prefix/build.json ..." -ForegroundColor Cyan
    Invoke-Aws @(
        "s3", "cp", $manifestFile, "s3://$bucket/$prefix/build.json",
        "--endpoint-url", $endpoint,
        "--content-type", "application/json",
        "--cache-control", "no-cache",
        "--only-show-errors"
    )

    Write-Host ""
    Write-Host "Published alpha $buildId." -ForegroundColor Green
    Write-Host "https://devportal.futureboard.studio/builds/$buildId"
}
finally {
    Remove-Item -LiteralPath $workDir -Recurse -Force -ErrorAction SilentlyContinue
}
