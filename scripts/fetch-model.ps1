#!/usr/bin/env pwsh
# Fetch the Parakeet TDT v3 int8 package into vendor\models\.
#
# 640 MB, once. Ungated (no Hugging Face token) and CC BY 4.0, so the attribution
# in models.lock has to reach the app's About text.
#
# Resumable: a partial file is kept as <name>.part and continued next run. Every
# file is hashed against models.lock before it is moved into place.
#
# Usage:
#   .\scripts\fetch-model.ps1
#   .\scripts\fetch-model.ps1 -Dir "$env:LOCALAPPDATA\hippo-voice\models\parakeet-v3-int8"
[CmdletBinding()]
param(
    [string]$Dir
)

$ErrorActionPreference = 'Stop'

$root = Split-Path $PSScriptRoot -Parent
$lock = Join-Path $root 'models.lock'
$lockText = Get-Content -LiteralPath $lock

$base = ($lockText | Select-String -Pattern '^base\s*=\s*"(.*)"').Matches.Groups[1].Value
$subdir = ($lockText | Select-String -Pattern '^dir\s*=\s*"(.*)"').Matches.Groups[1].Value

if (-not $base) { throw "models.lock has no base url" }
if (-not $Dir) { $Dir = Join-Path $root (Join-Path 'vendor\models' $subdir) }

# name/sha256 pairs in file order.
$files = @()
$pending = $null
foreach ($line in $lockText) {
    if ($line -match '^\s*name\s*=\s*"(.*)"\s*$') {
        $pending = $Matches[1]
    } elseif ($line -match '^\s*sha256\s*=\s*"(.*)"\s*$' -and $pending) {
        $files += [pscustomobject]@{ Name = $pending; Sha256 = $Matches[1] }
        $pending = $null
    }
}

if ($files.Count -eq 0) { throw "models.lock has no [[file]] entries" }

New-Item -ItemType Directory -Force -Path $Dir | Out-Null
Write-Host "model $subdir -> $Dir"

$fetched = 0
$bytes = 0

foreach ($file in $files) {
    $target = Join-Path $Dir $file.Name
    if (Test-Path -LiteralPath $target) {
        $actual = (Get-FileHash -LiteralPath $target -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($actual -eq $file.Sha256.ToLowerInvariant()) {
            Write-Host ("ok      {0}" -f $file.Name)
            continue
        }
        Write-Host ("stale   {0}" -f $file.Name)
    }

    $part = "$target.part"
    $url = "$base/$($file.Name)"
    Write-Host ("fetch   {0}" -f $file.Name)

    & curl.exe -fSL --retry 3 --continue-at - -o $part $url
    if ($LASTEXITCODE -ne 0) {
        throw "curl failed with exit code $LASTEXITCODE for $($file.Name)"
    }

    $actual = (Get-FileHash -LiteralPath $part -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actual -ne $file.Sha256.ToLowerInvariant()) {
        throw ("checksum mismatch for {0}`n  expected {1}`n  actual   {2}`nkeeping {3} for a retry" -f $file.Name, $file.Sha256, $actual, $part)
    }

    $size = (Get-Item -LiteralPath $part).Length
    Move-Item -LiteralPath $part -Destination $target -Force
    $fetched++
    $bytes += $size
}

Write-Host ("fetched {0} file(s), {1:N0} MiB" -f $fetched, ($bytes / 1MB))
Write-Host "verify with: cargo run -- model verify"
