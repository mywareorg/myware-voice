#!/usr/bin/env pwsh
# Remove hippo-dictate for the current user.
#
# Stops the daemon, removes the start-up entry, the user PATH entry, the program
# folder, and - after asking - the downloaded model and the local state.
#
# Usage:
#   .\uninstall.ps1
#   .\uninstall.ps1 -KeepModel -Force
[CmdletBinding()]
param(
    [switch]$KeepModel,
    [switch]$Force
)

$ErrorActionPreference = 'Stop'

$target = Join-Path $env:LOCALAPPDATA 'Programs\hippo-dictate'
$state = Join-Path $env:LOCALAPPDATA 'hippo-dictate'
$config = Join-Path $env:APPDATA 'hippo-dictate'
$client = Join-Path $target 'hippo-dictate.exe'

if (-not $Force) {
    $answer = Read-Host "Remove hippo-dictate from $target? [y/N]"
    if ($answer -notmatch '^(y|yes)$') {
        Write-Host 'cancelled'
        exit 0
    }
}

if (Test-Path -LiteralPath $client) {
    & $client quit *> $null
    Write-Host 'daemon stopped'
}

Remove-ItemProperty -Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' `
    -Name 'HippoVoice' -ErrorAction SilentlyContinue

$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if ($userPath) {
    $kept = @($userPath -split ';' | Where-Object { $_ -ne '' -and $_ -ne $target })
    $updated = $kept -join ';'
    if ($updated -ne $userPath) {
        [Environment]::SetEnvironmentVariable('Path', $updated, 'User')
        Write-Host 'removed the install folder from the user PATH'
    }
}

if (Test-Path -LiteralPath $target) {
    Remove-Item -LiteralPath $target -Recurse -Force
    Write-Host "removed $target"
}

if (Test-Path -LiteralPath $state) {
    $deleteModel = $false
    if (-not $KeepModel -and -not $Force) {
        $answer = Read-Host 'Delete the downloaded model and local state (about 640 MB)? [y/N]'
        $deleteModel = $answer -match '^(y|yes)$'
    } elseif ($Force -and -not $KeepModel) {
        $deleteModel = $true
    }

    if ($deleteModel) {
        Remove-Item -LiteralPath $state -Recurse -Force
        Write-Host "removed $state"
    } else {
        # Keep the model, drop the log, endpoint and cue files.
        Get-ChildItem -LiteralPath $state -Force |
            Where-Object { $_.Name -ne 'models' } |
            Remove-Item -Recurse -Force
        Write-Host "kept the model in $state\models"
    }
}

if (Test-Path -LiteralPath $config) {
    Remove-Item -LiteralPath $config -Recurse -Force
    Write-Host "removed $config"
}

Write-Host 'hippo-dictate is uninstalled'
