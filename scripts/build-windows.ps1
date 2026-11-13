#!/usr/bin/env pwsh
# Build hippo-voice on Windows and stage a release folder.
#
# One command: checks the toolchain, fetches the native runtime if it is missing,
# runs the tests, builds both executables, and stages exactly the files a user
# needs into dist\hippo-voice-<version>-win-x64, zipped.
#
# The speech model is NOT bundled by default - it is 640 MB and downloaded once by
# the user. Pass -WithModel to fetch it into the checkout for offline testing.
#
# Usage:
#   .\scripts\build-windows.ps1
#   .\scripts\build-windows.ps1 -SkipTests
#   .\scripts\build-windows.ps1 -Dev -NoZip
#   .\scripts\build-windows.ps1 -WithModel
[CmdletBinding()]
param(
    [switch]$Dev,
    [switch]$SkipTests,
    [switch]$WithModel,
    [switch]$NoZip
)

$ErrorActionPreference = 'Stop'

$root = Split-Path $PSScriptRoot -Parent
Push-Location $root
try {
    # --- toolchain ---------------------------------------------------------
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        throw 'cargo not found. Install Rust from https://rustup.rs, then reopen the terminal.'
    }

    $hostLine = (& rustc -vV | Select-String '^host:').ToString()
    $hostTarget = $hostLine.Substring($hostLine.IndexOf(':') + 1).Trim()
    if ($hostTarget -notmatch 'windows') {
        throw "this script builds a Windows target, but the host is $hostTarget"
    }
    if ($hostTarget -notmatch 'msvc$') {
        Write-Warning "host target is $hostTarget; the release artifact is built for *-pc-windows-msvc"
    }
    if (-not (Get-Command link.exe -ErrorAction SilentlyContinue)) {
        Write-Warning 'link.exe is not on PATH. If the build fails with "linker `link.exe` not found",'
        Write-Warning 'install Visual Studio Build Tools with the Desktop development with C++ workload,'
        Write-Warning 'or run this script from a Developer PowerShell.'
    }

    # --- native runtime ----------------------------------------------------
    $sherpaDll = Join-Path $root 'vendor\sherpa\lib\sherpa-onnx-c-api.dll'
    if (-not (Test-Path -LiteralPath $sherpaDll)) {
        Write-Host 'fetching the sherpa-onnx runtime'
        & (Join-Path $PSScriptRoot 'fetch-sherpa.ps1')
        if ($LASTEXITCODE -ne 0) { throw 'fetch-sherpa.ps1 failed' }
    }

    if ($WithModel) {
        Write-Host 'fetching the speech model (640 MB)'
        & (Join-Path $PSScriptRoot 'fetch-model.ps1')
        if ($LASTEXITCODE -ne 0) { throw 'fetch-model.ps1 failed' }
    }

    # --- build -------------------------------------------------------------
    $cargoProfile = if ($Dev) { 'dev' } else { 'release' }
    $outDir = if ($Dev) { 'debug' } else { 'release' }

    if (-not $SkipTests) {
        Write-Host 'running tests'
        & cargo test -p hippo-voice
        if ($LASTEXITCODE -ne 0) { throw 'tests failed' }
    }

    Write-Host "building ($cargoProfile)"
    if ($Dev) {
        & cargo build -p hippo-voice
    } else {
        & cargo build -p hippo-voice --release
    }
    if ($LASTEXITCODE -ne 0) { throw 'build failed' }

    # --- stage -------------------------------------------------------------
    $version = (Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version\s*=\s*"(.+)"').Matches[0].Groups[1].Value
    $stage = Join-Path $root "dist\hippo-voice-$version-win-x64"
    if (Test-Path -LiteralPath $stage) { Remove-Item -LiteralPath $stage -Recurse -Force }
    New-Item -ItemType Directory -Force -Path $stage | Out-Null

    $bin = Join-Path $root "target\$outDir"
    foreach ($name in @('hippo-voice.exe', 'hippo-voice-daemon.exe')) {
        $from = Join-Path $bin $name
        if (-not (Test-Path -LiteralPath $from)) { throw "expected $from after the build" }
        Copy-Item -LiteralPath $from -Destination $stage
    }
    foreach ($name in @('sherpa-onnx-c-api.dll', 'onnxruntime.dll', 'onnxruntime_providers_shared.dll')) {
        $from = Join-Path $root "vendor\sherpa\lib\$name"
        if (-not (Test-Path -LiteralPath $from)) { throw "missing $name; run scripts\fetch-sherpa.ps1" }
        Copy-Item -LiteralPath $from -Destination $stage
    }
    foreach ($name in @('README.md', 'LICENSE', 'models.lock')) {
        Copy-Item -LiteralPath (Join-Path $root $name) -Destination $stage
    }
    foreach ($name in @('install.ps1', 'uninstall.ps1')) {
        Copy-Item -LiteralPath (Join-Path $PSScriptRoot $name) -Destination $stage
    }

    if (-not $NoZip) {
        $zip = "$stage.zip"
        if (Test-Path -LiteralPath $zip) { Remove-Item -LiteralPath $zip -Force }
        Compress-Archive -Path $stage -DestinationPath $zip
        Write-Host "zip: $zip"
    }

    Write-Host "staged: $stage"
    Write-Host ''
    Write-Host 'Smoke test before shipping:'
    Write-Host "  $stage\hippo-voice.exe doctor"
    if (-not $WithModel) {
        Write-Host '  (the model is not bundled: run `hippo-voice model download` on the target machine)'
    }
} finally {
    Pop-Location
}
