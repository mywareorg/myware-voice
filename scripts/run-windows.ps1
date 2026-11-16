#!/usr/bin/env pwsh
# Run the tray daemon from a checkout on Windows.
#
# One command for development: enters the Visual Studio developer environment
# when link.exe is missing, fetches the native runtime if it is absent, checks
# the speech model, then runs `cargo run --bin myware-voice-daemon`. The daemon is
# a GUI-subsystem binary, so it leaves no console window behind; this terminal
# stays attached to cargo and the tray icon is the app's only surface.
#
# Left click the icon for settings, right click for the menu, and press
# ctrl+space to start and stop dictation. Quit from the menu to stop it.
#
# Usage:
#   .\scripts\run-windows.ps1
#   .\scripts\run-windows.ps1 -WithModel    # download the 640 MB model first
#   .\scripts\run-windows.ps1 -WithStreamingModel
#   .\scripts\run-windows.ps1 -Force        # stop a running daemon/settings first
[CmdletBinding()]
param(
    [switch]$WithModel,
    [switch]$WithStreamingModel,
    [switch]$Release,
    [switch]$Force
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

# Every failure in this script and every daemon error lands in one file beside
# the checkout, so a Windows tester can send one path back.
function Add-RunError {
    param(
        [Parameter(Mandatory = $true)][string]$Message,
        [string[]]$SourceLog
    )
    $target = Join-Path $root 'myware-voice-error.log'
    Add-Content -LiteralPath $target -Value ''
    Add-Content -LiteralPath $target -Value "=== $Message at $(Get-Date -Format o) ==="
    foreach ($file in @($SourceLog)) {
        if ($file -and (Test-Path -LiteralPath $file)) {
            Add-Content -LiteralPath $target -Value "--- $file ---"
            Get-Content -LiteralPath $file -Tail 200 |
                Add-Content -LiteralPath $target
        }
    }
}

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

    $streamDir = $null
    $streamLock = Get-Content -LiteralPath (Join-Path $root 'models-streaming.lock') -Raw
    if ($streamLock -match '(?m)^dir\s*=\s*"(.+)"') {
        $streamDir = Join-Path $root (Join-Path 'vendor\models' $matches[1])
    }
    $streamReady = $streamDir -and (Test-Path -LiteralPath (Join-Path $streamDir 'tokens.txt'))

    if ($WithStreamingModel -and -not $streamReady) {
        Write-Host 'fetching the streaming model (73 MB)'
        & (Join-Path $PSScriptRoot 'fetch-streaming-model.ps1')
        if ($LASTEXITCODE -ne 0) { throw 'fetch-streaming-model.ps1 failed' }
        $streamReady = $true
    }
    if (-not $streamReady) {
        Write-Warning "no streaming model at $streamDir"
        Write-Warning 'streaming mode will use phrase chunks: .\scripts\fetch-streaming-model.ps1 (or rerun with -WithStreamingModel)'
    }

    Write-Host ''
    Write-Host 'starting the tray daemon - left click for settings, ctrl+space to dictate'
    Write-Host "log: $env:LOCALAPPDATA\myware-voice\log"
    Write-Host ''

    # A daemon from an installed copy starts at login and owns the endpoint.
    # Starting a dev build beside it fails immediately, and the GUI daemon used
    # to fail silently; say which process is in the way instead.
    $endpoint = Join-Path $env:LOCALAPPDATA 'myware-voice\endpoint'
    if (Test-Path -LiteralPath $endpoint) {
        $port = (Get-Content -LiteralPath $endpoint -TotalCount 1).Trim()
        if ($port -match '^\d+$') {
            $client = New-Object System.Net.Sockets.TcpClient
            try {
                $task = $client.ConnectAsync('127.0.0.1', [int]$port)
                if ($task.Wait(300) -and $client.Connected) {
                    if ($Force) {
                        Write-Host "stopping the daemon on 127.0.0.1:$port"
                        Get-Process -Name 'myware-voice-daemon' `
                            -ErrorAction SilentlyContinue |
                            Stop-Process -Force
                        Start-Sleep -Milliseconds 500
                    } else {
                        Write-Warning "a daemon is already listening on 127.0.0.1:$port."
                        Write-Warning 'Quit it from its tray icon, or run `myware-voice quit`, before starting a development build (or pass -Force).'
                        return
                    }
                }
            } catch {
                # A stale endpoint is expected after a crash; carry on.
            } finally {
                $client.Close()
            }
        }
    }

    # The settings window is a separate process and can outlive the daemon.
    # Windows then refuses to replace target\debug\myware-voice-settings.exe and
    # cargo reports "failed to remove file" instead of naming the cause.
    $blocking = @(Get-Process -Name 'myware-voice-daemon', 'myware-voice-settings' `
        -ErrorAction SilentlyContinue)
    if ($blocking.Count -gt 0) {
        $list = ($blocking | ForEach-Object { "$($_.ProcessName) ($($_.Id))" }) -join ', '
        if ($Force) {
            Write-Host "stopping $list"
            $blocking | Stop-Process -Force
            Start-Sleep -Milliseconds 500
        } else {
            Write-Warning "cannot rebuild while these are running: $list"
            Write-Warning 'Quit the daemon from the tray and close Settings, then rerun; or pass -Force.'
            Add-RunError "build blocked by running process: $list"
            return
        }
    }

    $cargoArgs = @('run', '--bin', 'myware-voice-daemon')
    if ($Release) { $cargoArgs = @('run', '--release', '--bin', 'myware-voice-daemon') }
    $cargo = (Get-Command cargo).Source

    # The daemon launches myware-voice-settings.exe from its own directory, and
    # `cargo run --bin` builds only the daemon. Without this, left click finds no
    # settings binary and the tray silently does nothing.
    #
    # Build output is redirected rather than piped: in Windows PowerShell 5.1 a
    # native command's stderr becomes an ErrorRecord as soon as it joins the
    # pipeline, and with $ErrorActionPreference = 'Stop' that aborts the script
    # before its exit code can be reported.
    Write-Host 'building the workspace'
    $buildOut = [System.IO.Path]::GetTempFileName()
    $buildErr = [System.IO.Path]::GetTempFileName()
    $buildCode = 0
    try {
        $buildArgs = @('build', '--workspace')
        if ($Release) { $buildArgs += '--release' }
        $build = Start-Process -FilePath $cargo `
            -ArgumentList $buildArgs `
            -NoNewWindow -Wait -PassThru `
            -RedirectStandardOutput $buildOut -RedirectStandardError $buildErr
        $buildCode = $build.ExitCode

        # Warnings are on stderr; print them whether or not the build failed.
        Get-Content -LiteralPath $buildErr | ForEach-Object { Write-Host $_ }
        if ($buildCode -ne 0) {
            Get-Content -LiteralPath $buildOut -Tail 200 |
                ForEach-Object { Write-Host $_ }
            Write-Warning "cargo build exited with code $buildCode"
            Add-RunError "cargo build exited with code $buildCode" @($buildOut, $buildErr)
        }
    } finally {
        Remove-Item -LiteralPath $buildOut, $buildErr -Force -ErrorAction SilentlyContinue
    }
    if ($buildCode -ne 0) {
        return
    }

    & cargo @cargoArgs
    if ($LASTEXITCODE -ne 0) {
        $log = Join-Path $env:LOCALAPPDATA 'myware-voice\log'
        Write-Host ''
        Write-Warning "cargo run exited with code $LASTEXITCODE"
        if (Test-Path -LiteralPath $log) {
            Write-Host "last log lines from $log"
            Get-Content -LiteralPath $log -Tail 20 | ForEach-Object { Write-Host "  $_" }
        } else {
            Write-Host "no log at $log"
        }
        Add-RunError "cargo run exited with code $LASTEXITCODE" $log
    }
} catch {
    $reason = $_.Exception.Message
    Write-Warning "run-windows.ps1 failed: $reason"
    Add-RunError "run-windows.ps1 failed: $reason"
} finally {
    Pop-Location
}
