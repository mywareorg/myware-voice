#!/usr/bin/env pwsh
# Run the tray daemon from a checkout on Windows.
#
# One command for development: enters the Visual Studio developer environment
# when link.exe is missing, fetches the native runtime if it is absent, checks
# the speech model, then runs `cargo run --bin hippo-voice-daemon`. The daemon is
# a GUI-subsystem binary, so it leaves no console window behind; this terminal
# stays attached to cargo and the tray icon is the app's only surface.
#
# Left click the icon for settings, right click for the menu, and press
# ctrl+space to start and stop dictation. Quit from the menu to stop it.
#
# Usage:
#   .\scripts\run-windows.ps1
#   .\scripts\run-windows.ps1 -WithModel    # download the 640 MB model first
#   .\scripts\run-windows.ps1 -Release      # run the release profile
[CmdletBinding()]
param(
    [switch]$WithModel,
    [switch]$Release
)

$ErrorActionPreference = 'Stop'

# --- Visual Studio environment ------------------------------------------------
#
# Same reason as build-windows.ps1: Rust's MSVC target needs link.exe, which is
# only on PATH inside a Developer PowerShell.

function Find-VsInstallPath {
    $base = ${env:ProgramFiles(x86)}
    if (-not $base) { $base = 'C:\Program Files (x86)' }
    $vswhere = Join-Path $base 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (-not (Test-Path -LiteralPath $vswhere)) { return $null }

    $path = & $vswhere -latest -products * `
        -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
        -property installationPath
    if (-not $path) { return $null }
    return $path.Trim()
}

function Enter-DevEnvironment {
    param([Parameter(Mandatory = $true)][string]$VsPath)

    $cmd = Join-Path $VsPath 'Common7\Tools\VsDevCmd.bat'
    if (-not (Test-Path -LiteralPath $cmd)) {
        throw "VsDevCmd.bat was not found under $VsPath"
    }
    Write-Host 'entering the Visual Studio developer environment'

    $stdoutFile = [System.IO.Path]::GetTempFileName()
    $stderrFile = [System.IO.Path]::GetTempFileName()
    $inner = '""{0}" -arch=x64 -host_arch=x64 && set"' -f $cmd

    $process = Start-Process -FilePath 'cmd.exe' `
        -ArgumentList @('/c', $inner) `
        -NoNewWindow -Wait -PassThru `
        -RedirectStandardOutput $stdoutFile -RedirectStandardError $stderrFile
    $code = $process.ExitCode

    $lines = @()
    if (Test-Path -LiteralPath $stdoutFile) { $lines = @(Get-Content -Path $stdoutFile) }
    $errors = @()
    if (Test-Path -LiteralPath $stderrFile) { $errors = @(Get-Content -Path $stderrFile) }
    Remove-Item -LiteralPath $stdoutFile, $stderrFile -Force -ErrorAction SilentlyContinue

    if ($code -ne 0) {
        $why = @(@($errors) + @($lines) | Where-Object { $_ -and "$_".Trim() } |
            Select-Object -First 20) -join ' / '
        throw "VsDevCmd.bat failed with exit code $code. Output: $why"
    }

    $imported = 0
    foreach ($line in $lines) {
        if ($line -match '^([^=]+)=(.*)$') {
            [Environment]::SetEnvironmentVariable($matches[1], $matches[2], 'Process')
            $imported++
        }
    }
    if ($imported -eq 0) {
        throw 'VsDevCmd.bat produced no environment variables; the developer environment was not applied.'
    }
}

$root = Split-Path $PSScriptRoot -Parent
Push-Location $root
try {
    if ($env:OS -ne 'Windows_NT') {
        throw 'run-windows.ps1 is for a Windows host; on Linux use `cargo run -- daemon`'
    }
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        throw 'cargo not found. Install Rust from https://rustup.rs, then reopen the terminal.'
    }

    $hostLine = (& rustc -vV | Select-String '^host:').ToString()
    $hostTarget = $hostLine.Substring($hostLine.IndexOf(':') + 1).Trim()
    if ($hostTarget -notmatch 'windows') {
        throw "this script runs a Windows build, but the host is $hostTarget"
    }

    if (-not (Get-Command link.exe -ErrorAction SilentlyContinue)) {
        $vsPath = Find-VsInstallPath
        if (-not $vsPath) {
            throw 'link.exe is not on PATH and no Visual Studio C++ build tools were found. Install "Build Tools for Visual Studio" with the Desktop development with C++ workload, then rerun this script.'
        }
        Enter-DevEnvironment -VsPath $vsPath
    }
    if (-not (Get-Command link.exe -ErrorAction SilentlyContinue)) {
        throw 'link.exe is still not on PATH after entering the Visual Studio environment. Add the "Desktop development with C++" workload.'
    }

    $sherpaDll = Join-Path $root 'vendor\sherpa\lib\sherpa-onnx-c-api.dll'
    if (-not (Test-Path -LiteralPath $sherpaDll)) {
        Write-Host 'fetching the sherpa-onnx runtime'
        & (Join-Path $PSScriptRoot 'fetch-sherpa.ps1')
        if ($LASTEXITCODE -ne 0) { throw 'fetch-sherpa.ps1 failed' }
    }

    $modelDir = $null
    $lock = Get-Content -LiteralPath (Join-Path $root 'models.lock') -Raw
    if ($lock -match '(?m)^dir\s*=\s*"(.+)"') {
        $modelDir = Join-Path $root (Join-Path 'vendor\models' $matches[1])
    }
    $modelReady = $modelDir -and (Test-Path -LiteralPath (Join-Path $modelDir 'encoder.int8.onnx'))

    if ($WithModel -and -not $modelReady) {
        Write-Host 'fetching the speech model (640 MB)'
        & (Join-Path $PSScriptRoot 'fetch-model.ps1')
        if ($LASTEXITCODE -ne 0) { throw 'fetch-model.ps1 failed' }
        $modelReady = $true
    }
    if (-not $modelReady) {
        Write-Warning "no model at $modelDir"
        Write-Warning 'dictation will fail until it is fetched: .\scripts\fetch-model.ps1 (or rerun with -WithModel)'
    }

    Write-Host ''
    Write-Host 'starting the tray daemon - left click for settings, ctrl+space to dictate'
    Write-Host "log: $env:LOCALAPPDATA\hippo-voice\log"
    Write-Host ''

    $cargoArgs = @('run', '--bin', 'hippo-voice-daemon')
    if ($Release) { $cargoArgs = @('run', '--release', '--bin', 'hippo-voice-daemon') }
    & cargo @cargoArgs
    if ($LASTEXITCODE -ne 0) {
        throw "cargo run failed with exit code $LASTEXITCODE"
    }
} finally {
    Pop-Location
}
