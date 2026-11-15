#Requires -Version 5.1
<#
.SYNOPSIS
  Helpers shared by the MSIX scripts: SDK tool lookup, progress lines, version
  formatting.

.DESCRIPTION
  Dot-source this. It is deliberately small: hippo-transcribe's helper carries
  CMake, 7-Zip and GPU detection, none of which this app needs.
#>

function Write-Step {
    param([Parameter(Mandatory = $true)][string]$Message)
    Write-Host "==> $Message" -ForegroundColor Cyan
}

function Write-Warn {
    param([Parameter(Mandatory = $true)][string]$Message)
    Write-Host "warning: $Message" -ForegroundColor Yellow
}

# makeappx.exe, makepri.exe and signtool.exe are not on PATH in a normal
# terminal. They live under the Windows 10/11 SDK, in a versioned directory, so
# ask the PATH first and then the newest SDK that has the tool.
function Resolve-SdkTool {
    param([Parameter(Mandatory = $true)][string]$Name)

    $command = Get-Command $Name -ErrorAction SilentlyContinue
    if ($command) { return $command.Source }

    $kits = @(
        "${env:ProgramFiles(x86)}\Windows Kits\10",
        "$env:ProgramFiles\Windows Kits\10"
    ) | Where-Object { $_ -and (Test-Path $_) }

    foreach ($kit in $kits) {
        $found = Get-ChildItem -Path (Join-Path $kit "bin") -Directory -ErrorAction SilentlyContinue |
            Sort-Object Name -Descending |
            ForEach-Object { Join-Path $_.FullName "x64\$Name" } |
            Where-Object { Test-Path $_ } |
            Select-Object -First 1
        if ($found) { return $found }
    }

    foreach ($kit in $kits) {
        $found = @(
            (Join-Path (Join-Path $kit "App Certification Kit") $Name),
            (Join-Path (Join-Path (Join-Path $kit "App Certification Kit") "x64") $Name)
        ) | Where-Object { Test-Path $_ } | Select-Object -First 1
        if ($found) { return $found }
    }
    return $null
}

# MSIX versions have four numeric parts; the crate version has three.
function ConvertTo-FourPartVersion {
    param([Parameter(Mandatory = $true)][string]$Version)

    $parts = @($Version -split '\.')
    if ($parts.Count -gt 4) { throw "MSIX versions have at most four parts ($Version)" }
    foreach ($part in $parts) {
        if ($part -notmatch '^\d+$') { throw "MSIX versions must be numeric ($Version)" }
    }
    while ($parts.Count -lt 4) { $parts += "0" }
    return ($parts -join '.')
}
