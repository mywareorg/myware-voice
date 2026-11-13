#!/usr/bin/env pwsh
# Build hippo-voice on Windows and stage a release folder.
#
# One command: finds the MSVC toolchain (entering the Visual Studio developer
# environment when link.exe is not already on PATH), fetches the native runtime if
# it is missing, runs the tests, builds both executables, and stages exactly the
# files a user needs into dist\hippo-voice-<version>-win-x64, zipped.
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

# --- Visual Studio environment ------------------------------------------------
#
# Rust's MSVC target needs link.exe. It is only on PATH inside a Developer
# PowerShell, so a plain terminal fails with "linker `link.exe` not found" long
# after the build starts. This imports the environment instead of asking the user
# to know about Developer PowerShell.

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

    # cmd.exe is started through Start-Process because the command line needs a
    # path with spaces, a nested call and a captured environment, and hand-built
    # quoting for `cmd /c` silently loses its final quote. Both streams go to
    # files: in Windows PowerShell 5.1 a native command's stderr becomes an
    # ErrorRecord as soon as it joins the pipeline, and with
    # $ErrorActionPreference = 'Stop' that aborts the script even on success.
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

# Rust links the dynamic C runtime, so a clean machine without the Visual C++
# redistributable cannot start the exe. Stage the three DLLs that matter.
function Copy-VcRuntime {
    param([string]$VsPath, [string]$Destination)

    if (-not $VsPath) { return $false }
    $redist = Join-Path $VsPath 'VC\Redist\MSVC'
    if (-not (Test-Path -LiteralPath $redist)) {
        Write-Warning 'no VC redistributable found; the app will need the Visual C++ runtime installed'
        return $false
    }

    $crt = Get-ChildItem -LiteralPath $redist -Recurse -Directory -Filter 'Microsoft.VC*.CRT' `
        -ErrorAction SilentlyContinue |
        Where-Object { $_.FullName -match '\\x64\\' } |
        Select-Object -First 1 -ExpandProperty FullName
    if (-not $crt) {
        Write-Warning 'no x64 VC redistributable found; the app will need the Visual C++ runtime installed'
        return $false
    }

    # Copy by family, not by a fixed list: sherpa-onnx-c-api.dll imports
    # VCRUNTIME140 and MSVCP140, and onnxruntime.dll also imports MSVCP140_1.
    # A missing member of either family fails the load with "the specified module
    # could not be found", which reads like a missing sherpa DLL.
    $copied = 0
    foreach ($pattern in @('vcruntime140*.dll', 'msvcp140*.dll', 'concrt140*.dll')) {
        Get-ChildItem -LiteralPath $crt -Filter $pattern -File -ErrorAction SilentlyContinue |
            ForEach-Object {
                Copy-Item -LiteralPath $_.FullName -Destination $Destination -Force
                $copied++
            }
    }

    $needed = @('vcruntime140.dll', 'vcruntime140_1.dll', 'msvcp140.dll', 'msvcp140_1.dll')
    $missing = @($needed | Where-Object { -not (Test-Path -LiteralPath (Join-Path $Destination $_)) })
    if ($missing.Count -gt 0) {
        Write-Warning ("missing from the staged runtime: {0}" -f ($missing -join ', '))
        Write-Warning 'the app will need the Microsoft Visual C++ Redistributable (x64) installed'
    }
    return ($copied -gt 0)
}

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

    $vsPath = Find-VsInstallPath
    if (-not (Get-Command link.exe -ErrorAction SilentlyContinue)) {
        if (-not $vsPath) {
            throw 'link.exe is not on PATH and no Visual Studio C++ build tools were found. Install "Build Tools for Visual Studio" with the Desktop development with C++ workload, then rerun this script.'
        }
        Enter-DevEnvironment -VsPath $vsPath
    }
    if (-not (Get-Command link.exe -ErrorAction SilentlyContinue)) {
        throw 'link.exe is still not on PATH after entering the Visual Studio environment. Add the "Desktop development with C++" workload.'
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
        & cargo test --workspace
        if ($LASTEXITCODE -ne 0) { throw 'tests failed' }
    }

    Write-Host "building ($cargoProfile)"
    if ($Dev) {
        & cargo build --workspace
    } else {
        & cargo build --workspace --release
    }
    if ($LASTEXITCODE -ne 0) { throw 'build failed' }

    # --- stage -------------------------------------------------------------
    $version = (Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version\s*=\s*"(.+)"').Matches[0].Groups[1].Value
    $stage = Join-Path $root "dist\hippo-voice-$version-win-x64"
    if (Test-Path -LiteralPath $stage) { Remove-Item -LiteralPath $stage -Recurse -Force }
    New-Item -ItemType Directory -Force -Path $stage | Out-Null

    $bin = Join-Path $root "target\$outDir"
    foreach ($name in @('hippo-voice.exe', 'hippo-voice-daemon.exe', 'hippo-voice-settings.exe')) {
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
    if (Copy-VcRuntime -VsPath $vsPath -Destination $stage) {
        Write-Host 'staged the Visual C++ runtime'
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
