# Builds RodhareistEngine, compiles tests/cpp/smoke.cpp with MSVC against the
# DLL and against the static library, and runs both. Exit code 0 when both
# print "ok".
#
#   pwsh crates/RodhareistEngine/tests/cpp/run.ps1            # debug
#   pwsh crates/RodhareistEngine/tests/cpp/run.ps1 -Release
param([switch]$Release)

$ErrorActionPreference = 'Stop'
$crate = Resolve-Path (Join-Path $PSScriptRoot '..\..')
$root = Resolve-Path (Join-Path $crate '..\..')
$profileDir = if ($Release) { 'release' } else { 'debug' }
$cargoFlags = if ($Release) { @('--release') } else { @() }

Push-Location $root
try {
    & cargo build -p rodhareist_engine @cargoFlags
    if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }
    # The system libraries a static Rust library needs, as rustc reports them.
    $note = & cargo rustc -p rodhareist_engine @cargoFlags --lib --crate-type staticlib -- --print native-static-libs 2>&1 |
        Select-String 'native-static-libs:' | Select-Object -Last 1
    if (-not $note) { throw 'rustc did not report the native static libraries' }
    $staticLibs = ($note.Line -split 'native-static-libs:')[1].Trim()
} finally {
    Pop-Location
}

$target = Join-Path $root "target\$profileDir"
$out = Join-Path $root "target\rodhareist_engine_cpp\$profileDir"
New-Item -ItemType Directory -Force $out | Out-Null

$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
$vs = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if (-not $vs) { throw 'MSVC (Visual Studio C++ tools) was not found' }
$vcvars = Join-Path $vs 'VC\Auxiliary\Build\vcvars64.bat'

$include = Join-Path $crate 'include'
$source = Join-Path $PSScriptRoot 'smoke.cpp'
$runtime = if ($Release) { '/MD' } else { '/MDd' }

# Against the DLL: link its import library, run it beside the DLL.
$dynamicExe = Join-Path $out 'smoke_dll.exe'
$cmd = "set `"PATH=$(Split-Path $vswhere);%PATH%`" && `"$vcvars`" >nul && cl /nologo /std:c++17 /EHsc /W4 /WX $runtime /I`"$include`" `"$source`" /Fo`"$out\\`" /Fe`"$dynamicExe`" /link `"$target\rodhareist_engine.dll.lib`""
cmd /c $cmd
if ($LASTEXITCODE -ne 0) { throw 'compiling against the DLL failed' }
Copy-Item (Join-Path $target 'rodhareist_engine.dll') $out -Force

# Against the static library. Rust's static library is built against the
# release C runtime, so this one always uses /MD.
$staticExe = Join-Path $out 'smoke_static.exe'
$cmd = "set `"PATH=$(Split-Path $vswhere);%PATH%`" && `"$vcvars`" >nul && cl /nologo /std:c++17 /EHsc /W4 /WX /MD /DRODHAREIST_STATIC /I`"$include`" `"$source`" /Fo`"$out\\`" /Fe`"$staticExe`" /link `"$target\rodhareist_engine.lib`" $staticLibs"
cmd /c $cmd
if ($LASTEXITCODE -ne 0) { throw 'compiling against the static library failed' }

$failed = $false
foreach ($exe in @($dynamicExe, $staticExe)) {
    Write-Host "== $(Split-Path $exe -Leaf)"
    & $exe
    if ($LASTEXITCODE -ne 0) { $failed = $true }
}
if ($failed) { exit 1 }
