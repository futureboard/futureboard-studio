# Build the Futureboard Studio Windows installer from the complete
# xtask-staged runtime tree.
#
# Default package source:
#   out\release\windows-x64
#
# Expected package command:
#   cargo package-release
#
# Examples:
#   .\build-installer.ps1
#   .\build-installer.ps1 -AppVersion "2026.8.1-alpha1"
#   .\build-installer.ps1 -SourceDir "D:\Builds\Futureboard\windows-x64"
#   .\build-installer.ps1 -Iscc "C:\Program Files (x86)\Inno Setup 6\ISCC.exe"

param(
    [string]$Root = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path,
    [string]$Iscc = "",
    [string]$AppVersion = "",
    [string]$SourceDir = ""
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

function Find-InnoSetupCompiler {
    param(
        [string]$PreferredPath = ""
    )

    $candidates = [System.Collections.Generic.List[string]]::new()

    if ($PreferredPath) {
        $candidates.Add($PreferredPath)
    }

    if ($env:INNO_SETUP_PATH) {
        # Support either:
        #   INNO_SETUP_PATH=C:\...\ISCC.exe
        # or:
        #   INNO_SETUP_PATH=C:\...\Inno Setup 6
        if (Test-Path -LiteralPath $env:INNO_SETUP_PATH -PathType Container) {
            $candidates.Add(
                (Join-Path $env:INNO_SETUP_PATH "ISCC.exe")
            )
        }
        else {
            $candidates.Add($env:INNO_SETUP_PATH)
        }
    }

    $isccCommand = Get-Command "iscc.exe" -ErrorAction SilentlyContinue
    if (-not $isccCommand) {
        $isccCommand = Get-Command "iscc" -ErrorAction SilentlyContinue
    }

    if ($isccCommand) {
        $candidates.Add($isccCommand.Source)
    }

    foreach ($version in 7, 6) {
        if ($env:ProgramFiles) {
            $candidates.Add(
                (Join-Path $env:ProgramFiles "Inno Setup $version\ISCC.exe")
            )
        }

        if (${env:ProgramFiles(x86)}) {
            $candidates.Add(
                (Join-Path ${env:ProgramFiles(x86)} "Inno Setup $version\ISCC.exe")
            )
        }

        if ($env:LOCALAPPDATA) {
            $candidates.Add(
                (Join-Path $env:LOCALAPPDATA "Programs\Inno Setup $version\ISCC.exe")
            )
        }
    }

    foreach ($candidate in $candidates) {
        if (
            $candidate -and
            (Test-Path -LiteralPath $candidate -PathType Leaf)
        ) {
            return (Resolve-Path -LiteralPath $candidate).Path
        }
    }

    return $null
}

# ---------------------------------------------------------------------------
# Resolve repository root
# ---------------------------------------------------------------------------

if (-not (Test-Path -LiteralPath $Root -PathType Container)) {
    throw "Repository root not found: $Root"
}

$Root = (Resolve-Path -LiteralPath $Root).Path

# ---------------------------------------------------------------------------
# Resolve application version
# ---------------------------------------------------------------------------

if (-not $AppVersion) {
    $versionJson = Join-Path $Root "version.json"

    if (-not (Test-Path -LiteralPath $versionJson -PathType Leaf)) {
        throw "version.json not found: $versionJson"
    }

    try {
        $versionData = Get-Content -LiteralPath $versionJson -Raw |
            ConvertFrom-Json

        $AppVersion = [string]$versionData.version
    }
    catch {
        throw "Unable to read version from '$versionJson': $($_.Exception.Message)"
    }

    if ([string]::IsNullOrWhiteSpace($AppVersion)) {
        throw "The 'version' field is missing or empty in: $versionJson"
    }
}

# ---------------------------------------------------------------------------
# Resolve staged package source
# ---------------------------------------------------------------------------

if (-not $SourceDir) {
    $SourceDir = Join-Path $Root "out\release\windows-x64"
}
elseif (-not [System.IO.Path]::IsPathRooted($SourceDir)) {
    $SourceDir = Join-Path $Root $SourceDir
}

if (-not (Test-Path -LiteralPath $SourceDir -PathType Container)) {
    throw @"
Packaged runtime directory not found:

  $SourceDir

Create it first with:

  cargo package-release
"@
}

$SourceDir = (Resolve-Path -LiteralPath $SourceDir).Path

# ---------------------------------------------------------------------------
# Validate required package files
# ---------------------------------------------------------------------------

$nativeExe = Join-Path $SourceDir "FutureboardNative.exe"

if (-not (Test-Path -LiteralPath $nativeExe -PathType Leaf)) {
    throw @"
FutureboardNative.exe was not found in the packaged runtime tree:

  $nativeExe

Run:

  cargo package-release
"@
}

$packageFiles = @(
    Get-ChildItem -LiteralPath $SourceDir -File -Recurse -Force
)

if ($packageFiles.Count -eq 0) {
    throw "The packaged runtime directory is empty: $SourceDir"
}

# ---------------------------------------------------------------------------
# Resolve installer script
# ---------------------------------------------------------------------------

$iss = Join-Path $PSScriptRoot "installer.iss"

if (-not (Test-Path -LiteralPath $iss -PathType Leaf)) {
    throw "Inno Setup script not found: $iss"
}

$iss = (Resolve-Path -LiteralPath $iss).Path

# ---------------------------------------------------------------------------
# Locate Inno Setup
# ---------------------------------------------------------------------------

$Iscc = Find-InnoSetupCompiler -PreferredPath $Iscc

if (-not $Iscc) {
    throw @"
Inno Setup compiler was not found.

Install Inno Setup 6 or 7, add ISCC.exe to PATH, set INNO_SETUP_PATH,
or pass it explicitly:

  .\build-installer.ps1 -Iscc "C:\Program Files (x86)\Inno Setup 6\ISCC.exe"
"@
}

# ---------------------------------------------------------------------------
# Build installer
# ---------------------------------------------------------------------------

$outputDir = Join-Path $Root "target\installer"
$outputExe = Join-Path $outputDir "FutureboardStudioSetup.exe"

New-Item -ItemType Directory -Path $outputDir -Force | Out-Null

Write-Host ""
Write-Host "Futureboard Studio Installer" -ForegroundColor Cyan
Write-Host "Version : $AppVersion"
Write-Host "Source  : $SourceDir"
Write-Host "Files   : $($packageFiles.Count)"
Write-Host "Compiler: $Iscc"
Write-Host ""

& $Iscc `
    "/DMyAppVersion=$AppVersion" `
    "/DMySourceDir=$SourceDir" `
    $iss

$compilerExitCode = $LASTEXITCODE

if ($compilerExitCode -ne 0) {
    throw "Inno Setup compilation failed with exit code $compilerExitCode."
}

if (-not (Test-Path -LiteralPath $outputExe -PathType Leaf)) {
    throw "Inno Setup completed, but the installer was not found: $outputExe"
}

$outputFile = Get-Item -LiteralPath $outputExe
$outputSizeMb = [Math]::Round($outputFile.Length / 1MB, 2)

Write-Host ""
Write-Host "Installer built successfully!" -ForegroundColor Green
Write-Host "Output : $outputExe"
Write-Host "Size   : $outputSizeMb MB"
