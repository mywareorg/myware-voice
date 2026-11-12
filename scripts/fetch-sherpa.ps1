#!/usr/bin/env pwsh
# Fetch the pinned sherpa-onnx runtime into vendor\sherpa\.
#
# Windows publishes only the MD-Release variant: a flattened `lib\` with the C
# API, ONNX Runtime and the providers shim, and no headers. Nothing is linked at
# build time, so this is the only native piece the app needs.
#
# Extraction is the awkward part: the built-in `tar` is bsdtar without bzip2, so
# this tries the built-in tar, then 7-Zip, and otherwise tells you to install it.
#
# Usage:
#   .\scripts\fetch-sherpa.ps1
#   .\scripts\fetch-sherpa.ps1 -Dest D:\hippo\vendor
[CmdletBinding()]
param(
    [string]$Dest
)

$ErrorActionPreference = 'Stop'

$root = Split-Path $PSScriptRoot -Parent
if (-not $Dest) { $Dest = Join-Path $root 'vendor' }
$lock = Join-Path $root 'sherpa.lock'
$lockText = Get-Content -LiteralPath $lock

function Get-LockValue {
    param([string]$Section, [string]$Key)

    $inSection = $false
    foreach ($line in $lockText) {
        if ($line -match '^\s*\[') {
            $inSection = ($line.Trim() -ceq "[$Section]")
            continue
        }
        if ($inSection -and $line -match ('^\s*' + $Key + '\s*=\s*"(.*)"\s*$')) {
            return $Matches[1]
        }
    }
    return $null
}

$version = ($lockText | Select-String -Pattern '^version\s*=\s*"(.*)"').Matches.Groups[1].Value
$url     = Get-LockValue -Section 'windows' -Key 'url'
$sha     = Get-LockValue -Section 'windows' -Key 'sha256'
$archive = Get-LockValue -Section 'windows' -Key 'archive'

if (-not $url) { throw "sherpa.lock has no [windows] url" }

$out = Join-Path $Dest 'sherpa'
$libDir = Join-Path $out 'lib'
$dll = Join-Path $libDir 'sherpa-onnx-c-api.dll'

if (Test-Path -LiteralPath $dll) {
    Write-Host "sherpa-onnx $version already vendored at $libDir"
    exit 0
}

New-Item -ItemType Directory -Force -Path $Dest | Out-Null

$tarball = Join-Path $Dest "$archive.tar.bz2"
if (-not (Test-Path -LiteralPath $tarball)) {
    Write-Host "downloading sherpa-onnx $version"
    & curl.exe -fSL --retry 3 -o $tarball $url
    if ($LASTEXITCODE -ne 0) { throw "curl failed with exit code $LASTEXITCODE" }
}

$actual = (Get-FileHash -LiteralPath $tarball -Algorithm SHA256).Hash.ToLowerInvariant()
if ($actual -ne $sha.ToLowerInvariant()) {
    throw "checksum mismatch`n  expected $sha`n  actual   $actual`nkeeping $tarball"
}

$tmp = Join-Path $Dest ("sherpa-extract-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path $tmp | Out-Null

$extracted = $false

# 1. The built-in tar. Windows' bsdtar has no bzip2, so this often fails; it
#    succeeds when a real bzip2 is on PATH.
try {
    & tar -xjf $tarball -C $tmp 2>$null
    if ($LASTEXITCODE -eq 0) { $extracted = $true }
} catch {
    # tar not present at all; fall through.
}

# 2. 7-Zip, which treats .tar.bz2 as two archives: bz2 first, then tar.
if (-not $extracted) {
    $sevenZip = Get-Command 7z -ErrorAction SilentlyContinue
    if ($sevenZip) {
        Write-Host "extracting with 7-Zip"
        & $sevenZip.Source x $tarball "-o$tmp" -y | Out-Null
        $inner = Get-ChildItem -LiteralPath $tmp -Filter '*.tar' | Select-Object -First 1
        if ($inner) {
            & $sevenZip.Source x $inner.FullName "-o$tmp" -y | Out-Null
            Remove-Item -LiteralPath $inner.FullName
            $extracted = $true
        }
    }
}

if (-not $extracted) {
    throw "could not extract $tarball - the built-in tar has no bzip2. Install 7-Zip, or extract it by hand so that lib\ lands in $out"
}

$extractedRoot = Join-Path $tmp $archive
if (-not (Test-Path -LiteralPath $extractedRoot)) {
    $extractedRoot = Get-ChildItem -LiteralPath $tmp -Directory | Select-Object -First 1 -ExpandProperty FullName
}

if (Test-Path -LiteralPath $out) { Remove-Item -LiteralPath $out -Recurse -Force }
Move-Item -LiteralPath $extractedRoot -Destination $out
Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue

if (-not (Test-Path -LiteralPath $dll)) {
    throw "sherpa-onnx-c-api.dll missing after extraction; check the layout in $out"
}

Write-Host "sherpa-onnx $version ready: $libDir"
Write-Host "copy $libDir\*.dll beside hippo-voice.exe, or set HIPPO_VOICE_SHERPA_DIR"
