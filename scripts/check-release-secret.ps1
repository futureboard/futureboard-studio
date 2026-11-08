<#
.SYNOPSIS
  Fails if a canary secret appears in built binaries (issue #241).

.DESCRIPTION
  Scans each file's raw bytes for the canary as ASCII/UTF-8 and as UTF-16LE.
  Exit codes: 0 = canary not found in any file, 1 = canary found,
  2 = usage error (missing file, empty or too-short canary).

  Release check: build release with a canary exported as
  FUTUREBOARD_TONE3000_API_KEY (and TONE3000_API_KEY), then point this at the
  engine library, the launcher and anything else that ships.

.EXAMPLE
  ./scripts/check-release-secret.ps1 -Canary $canary -Path target/release/FutureboardEngine_x64.dll, target/release/FutureboardNative.exe

  Run it in-process (as above) to pass several paths; `pwsh -File` takes one.
#>
[CmdletBinding(PositionalBinding = $false)]
param(
    [Parameter(Mandatory = $true)]
    [string]$Canary,

    [Parameter(Mandatory = $true, Position = 0, ValueFromRemainingArguments = $true)]
    [string[]]$Path
)

$ErrorActionPreference = 'Stop'

if ($Canary.Length -lt 8) {
    [Console]::Error.WriteLine('Canary must be at least 8 characters so it cannot match by chance.')
    exit 2
}

# Compiled byte search: a PowerShell loop is far too slow for a multi-hundred-MB
# debug library. Plain C# so it also loads in Windows PowerShell 5.1.
if (-not ('FutureboardCanaryScan' -as [type])) {
    Add-Type -TypeDefinition @"
public static class FutureboardCanaryScan
{
    public static long IndexOf(byte[] haystack, byte[] needle)
    {
        int last = haystack.Length - needle.Length;
        byte first = needle[0];
        for (int i = 0; i <= last; i++)
        {
            if (haystack[i] != first) continue;
            int j = 1;
            while (j < needle.Length && haystack[i + j] == needle[j]) j++;
            if (j == needle.Length) return i;
        }
        return -1;
    }
}
"@
}

$encodings = [ordered]@{
    'ASCII/UTF-8' = [System.Text.Encoding]::UTF8.GetBytes($Canary)
    'UTF-16LE'    = [System.Text.Encoding]::Unicode.GetBytes($Canary)
}

$found = $false
foreach ($entry in $Path) {
    if (-not (Test-Path -LiteralPath $entry -PathType Leaf)) {
        [Console]::Error.WriteLine("Not a file: $entry")
        exit 2
    }
    $file = (Resolve-Path -LiteralPath $entry).Path
    $bytes = [System.IO.File]::ReadAllBytes($file)
    $fileHit = $false
    foreach ($name in $encodings.Keys) {
        $offset = [FutureboardCanaryScan]::IndexOf($bytes, $encodings[$name])
        if ($offset -ge 0) {
            Write-Output ("FOUND   {0} canary at offset 0x{1:X} in {2}" -f $name, $offset, $file)
            $fileHit = $true
            $found = $true
        }
    }
    if (-not $fileHit) {
        Write-Output ("clean   {0} ({1:N0} bytes)" -f $file, $bytes.Length)
    }
}

if ($found) {
    Write-Output 'FAIL: canary secret is embedded in a scanned binary.'
    exit 1
}
Write-Output 'OK: canary not found.'
exit 0
