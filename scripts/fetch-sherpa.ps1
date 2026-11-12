#!/usr/bin/env pwsh
# Fetch the pinned sherpa-onnx runtime into vendor\sherpa\.
#
# Windows publishes only the MD-Release variant: a flattened `lib\` with the C
# API, ONNX Runtime and the providers shim, and no headers. Nothing is linked at
# build time, so this is the only native piece the app needs.
#
# Extraction is the awkward part. Windows' built-in tar is bsdtar without bzip2,
# so the order is: an installed 7-Zip, then Git for Windows' bzip2 driving tar,
# then a standalone 7-Zip downloaded to the cache. The same chain is used by
# hippo-transcribe's Windows build.
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

function Find-SevenZip {
    $candidates = @(
        "C:\Program Files\7-Zip\7z.exe",
        "C:\Program Files (x86)\7-Zip\7z.exe"
    )
    $command = Get-Command 7z.exe -ErrorAction SilentlyContinue
    if ($command) { $candidates = @($command.Source) + $candidates }
    return $candidates |
        Where-Object { $_ -and (Test-Path $_) } |
        Select-Object -First 1
}

function Find-Bzip2 {
    $paths = @(
        "C:\Program Files\Git\usr\bin\bzip2.exe",
        "C:\Program Files\Git\mingw64\bin\bzip2.exe",
        "C:\Program Files (x86)\Git\usr\bin\bzip2.exe",
        "C:\Program Files\Git\bin\bzip2.exe"
    )
    $command = Get-Command bzip2.exe -ErrorAction SilentlyContinue
    if ($command) { $paths = @($command.Source) + $paths }
    return $paths |
        Where-Object { $_ -and (Test-Path $_) } |
        Select-Object -First 1
}

# A .tar.bz2 is two archives to 7-Zip: it yields a .tar, which is then extracted.
function Expand-WithSevenZip {
    param(
        [Parameter(Mandatory = $true)][string]$SevenZip,
        [Parameter(Mandatory = $true)][string]$Archive,
        [Parameter(Mandatory = $true)][string]$Destination
    )

    & $SevenZip x -y "-o$Destination" $Archive | Out-Null
    if ($LASTEXITCODE -ne 0) { return $false }

    $inner = Join-Path $Destination ([System.IO.Path]::GetFileNameWithoutExtension($Archive))
    if ((Test-Path -LiteralPath $inner) -and ($inner -like "*.tar")) {
        & $SevenZip x -y "-o$Destination" $inner | Out-Null
        if ($LASTEXITCODE -ne 0) { return $false }
        Remove-Item -LiteralPath $inner -Force -ErrorAction SilentlyContinue
    }
    return $true
}

function Get-PortableSevenZip {
    param([Parameter(Mandatory = $true)][string]$CacheRoot)

    $dir = Join-Path $CacheRoot "7za"
    $exe = Join-Path $dir "7za.exe"
    if (Test-Path -LiteralPath $exe) { return $exe }

    $url = "https://www.7-zip.org/a/7za920.zip"
    $expected = "2a3afe19c180f8373fa02ff00254d5394fec0349f5804e0ad2f6067854ff28ac"
    $zip = Join-Path $CacheRoot "7za920.zip"

    New-Item -ItemType Directory -Path $CacheRoot -Force | Out-Null
    if (-not (Test-Path -LiteralPath $zip)) {
        Write-Host "downloading the standalone 7-Zip extractor"
        Invoke-WebRequest -Uri $url -OutFile $zip -UseBasicParsing
    }

    $actual = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actual -ne $expected) {
        Remove-Item -LiteralPath $zip -Force -ErrorAction SilentlyContinue
        throw "7za archive checksum mismatch (expected $expected, got $actual)"
    }

    Expand-Archive -Path $zip -DestinationPath $dir -Force
    if (-not (Test-Path -LiteralPath $exe)) {
        throw "7za.exe was not found after extracting $zip"
    }
    return $exe
}

function Expand-SherpaArchive {
    param(
        [Parameter(Mandatory = $true)][string]$Archive,
        [Parameter(Mandatory = $true)][string]$Destination,
        [Parameter(Mandatory = $true)][string]$CacheRoot
    )

    $sevenZip = Find-SevenZip
    if ($sevenZip) {
        Write-Host "extracting with 7-Zip"
        if (Expand-WithSevenZip -SevenZip $sevenZip -Archive $Archive -Destination $Destination) { return }
    }

    # bsdtar without libbz2 shells out to `bzip2 -d`; Git for Windows ships one.
    $bzip2 = Find-Bzip2
    if ($bzip2) {
        $env:PATH = "$(Split-Path -Parent $bzip2);$env:PATH"
        Write-Host "extracting with tar and bzip2"
        & tar.exe -xf $Archive -C $Destination
        if ($LASTEXITCODE -eq 0) { return }
    }

    $portable = Get-PortableSevenZip -CacheRoot $CacheRoot
    Write-Host "extracting with the standalone 7-Zip extractor"
    if (Expand-WithSevenZip -SevenZip $portable -Archive $Archive -Destination $Destination) { return }

    throw "could not extract $Archive; remove $CacheRoot and rerun this script"
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

# The extractor lives outside the checkout so it survives a clean of vendor\.
$cacheRoot = if ($env:LOCALAPPDATA) {
    Join-Path $env:LOCALAPPDATA 'hippo-voice\cache'
} else {
    Join-Path $Dest 'cache'
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
    throw "checksum mismatch: expected $sha, got $actual (keeping $tarball)"
}

$tmp = Join-Path $Dest ("sherpa-extract-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path $tmp | Out-Null
try {
    Expand-SherpaArchive -Archive $tarball -Destination $tmp -CacheRoot $cacheRoot

    $extractedRoot = Join-Path $tmp $archive
    if (-not (Test-Path -LiteralPath $extractedRoot)) {
        $extractedRoot = Get-ChildItem -LiteralPath $tmp -Directory |
            Select-Object -First 1 -ExpandProperty FullName
    }
    if (-not $extractedRoot) { throw "nothing was extracted from $tarball" }

    if (Test-Path -LiteralPath $out) { Remove-Item -LiteralPath $out -Recurse -Force }
    Move-Item -LiteralPath $extractedRoot -Destination $out
} finally {
    Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
}

foreach ($name in @('sherpa-onnx-c-api.dll', 'onnxruntime.dll')) {
    if (-not (Test-Path -LiteralPath (Join-Path $libDir $name))) {
        throw "$name missing after extraction; check the layout in $out"
    }
}

Write-Host "sherpa-onnx $version ready: $libDir"
Write-Host "copy $libDir\*.dll beside hippo-voice.exe, or set HIPPO_VOICE_SHERPA_DIR"
