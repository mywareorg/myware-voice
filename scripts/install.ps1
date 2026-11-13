#!/usr/bin/env pwsh
# Install hippo-voice for the current user.
#
# No administrator rights: everything lives under %LOCALAPPDATA%, the start-up
# entry is HKCU, and the PATH change is the user PATH.
#
# Usage, from the extracted release folder:
#   .\install.ps1
#   .\install.ps1 -NoStartup        # portable: do not start at login
#   .\install.ps1 -NoPath           # do not touch the user PATH
[CmdletBinding()]
param(
    [string]$Source = $PSScriptRoot,
    [switch]$NoPath,
    [switch]$NoStartup
)

$ErrorActionPreference = 'Stop'

if (-not $env:LOCALAPPDATA) { throw 'LOCALAPPDATA is not set; this is not a normal Windows session' }

$target = Join-Path $env:LOCALAPPDATA 'Programs\hippo-voice'

foreach ($required in @('hippo-voice.exe', 'hippo-voice-daemon.exe', 'sherpa-onnx-c-api.dll', 'onnxruntime.dll')) {
    if (-not (Test-Path -LiteralPath (Join-Path $Source $required))) {
        throw "missing $required; run install.ps1 from the extracted release folder"
    }
}

New-Item -ItemType Directory -Force -Path $target | Out-Null
# Everything in the release folder: both executables, the sherpa DLLs, the staged
# VC runtime, the docs and uninstall.ps1.
Get-ChildItem -LiteralPath $Source -File |
    ForEach-Object { Copy-Item -LiteralPath $_.FullName -Destination $target -Force }
Write-Host "installed to $target"

# The CI artifact carries no Visual C++ runtime: sherpa-onnx-c-api.dll imports
# vcruntime140 and msvcp140, and onnxruntime.dll also imports msvcp140_1.
$vc = @('vcruntime140.dll', 'vcruntime140_1.dll', 'msvcp140.dll', 'msvcp140_1.dll')
$missingVc = @($vc | Where-Object { -not (Test-Path -LiteralPath (Join-Path $target $_)) })
if ($missingVc.Count -gt 0) {
    Write-Warning ("the release folder has no Visual C++ runtime ({0})" -f ($missingVc -join ', '))
    Write-Warning 'if the app fails to start, install the Microsoft Visual C++ Redistributable (x64)'
}

if (-not $NoPath) {
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if ($null -eq $userPath) { $userPath = '' }
    $parts = @($userPath -split ';' | Where-Object { $_ -ne '' })
    if ($parts -notcontains $target) {
        [Environment]::SetEnvironmentVariable('Path', ((@($parts) + $target) -join ';'), 'User')
        Write-Host 'added the install folder to the user PATH'
    }
    # Make it usable in this session too.
    $env:Path = "$target;$env:Path"
}

if (-not $NoStartup) {
    $run = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
    New-Item -Path $run -Force | Out-Null
    # The daemon binary is GUI-subsystem, so this opens no console at login.
    Set-ItemProperty -Path $run -Name 'HippoVoice' `
        -Value "`"$(Join-Path $target 'hippo-voice-daemon.exe')`""
    Write-Host 'start-on-login registered (HKCU Run)'
}

$client = Join-Path $target 'hippo-voice.exe'
& $client status *> $null
if ($LASTEXITCODE -ne 0) {
    Start-Process -FilePath (Join-Path $target 'hippo-voice-daemon.exe')
    Start-Sleep -Milliseconds 800
}

Write-Host ''
& $client doctor
Write-Host ''
Write-Host 'Next: download the speech model once (640 MB)'
Write-Host "  $client model download"
Write-Host 'Then press the hotkey to dictate.'
