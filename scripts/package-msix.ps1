#Requires -Version 5.1
<#
.SYNOPSIS
  One command: build, self-sign and pack myware-voice as an MSIX.

.DESCRIPTION
  The whole pipeline, in the order the pieces depend on each other:

    1. build-windows.ps1 stages the executables, the sherpa runtime and the VC
       runtime into dist\myware-voice-<version>-win-x64
    2. make-test-cert.ps1 creates (or reuses) a self-signed code-signing
       certificate whose subject matches packaging\identity.json
    3. make-msix.ps1 lays out the package, indexes the assets, packs it, verifies
       the contents and signs it with that certificate

  The result installs with `Add-AppxPackage` on this machine, because the
  certificate is trusted for this user. It will not install anywhere else unless
  that machine trusts the same certificate: a sideloaded package is a
  self-signed one, not a Store one.

.PARAMETER Install
  Install the package and start the app when packing finishes.

.PARAMETER SkipBuild
  Reuse the existing dist folder instead of rebuilding.

.EXAMPLE
  .\scripts\package-msix.ps1

.EXAMPLE
  .\scripts\package-msix.ps1 -Install -SkipBuild

.EXAMPLE
  .\scripts\package-msix.ps1 -NoCertificate   # unsigned, for inspecting contents
#>
[CmdletBinding()]
param(
    [switch]$Install,
    [switch]$SkipBuild,
    [switch]$NoCertificate,
    [string]$Password = "myware-voice-test",
    [string]$PfxPath = "dist\myware-voice-test.pfx"
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

. (Join-Path $PSScriptRoot "windows-sdk.ps1")
$root = Split-Path -Parent $PSScriptRoot
function Resolve-FromRoot { param([string]$Path) if ([System.IO.Path]::IsPathRooted($Path)) { $Path } else { Join-Path $root $Path } }
$PfxPath = Resolve-FromRoot $PfxPath
$OutDir = Join-Path $root "dist"

$manifest = Get-Content -Path (Join-Path $root "Cargo.toml") -Raw
if ($manifest -notmatch '(?m)^version = "([^"]+)"') {
    throw "Could not read the version from Cargo.toml"
}
$version = $Matches[1]
$stage = Join-Path $OutDir "myware-voice-$version-win-x64"
$name = "myware-voice-$version-win-x64"

if (-not $SkipBuild) {
    Write-Step "building the Windows stage"
    # A PowerShell script reports failure by throwing; `exit` inside it ends this
    # process with its code. $LASTEXITCODE only speaks for native commands.
    & (Join-Path $PSScriptRoot "build-windows.ps1")
}
if (-not (Test-Path -LiteralPath $stage)) {
    throw "No stage at $stage. Run without -SkipBuild, or build it first."
}

if ($NoCertificate) {
    Write-Step "packing without a certificate"
    & (Join-Path $PSScriptRoot "make-msix.ps1") -Stage $stage -Name $name -Version $version -OutDir $OutDir -NoCertificate
}

if (-not (Test-Path -LiteralPath $PfxPath)) {
    Write-Step "creating the self-signed certificate"
    & (Join-Path $PSScriptRoot "make-test-cert.ps1") -PfxPath $PfxPath -Password $Password
}

Write-Step "packing and signing"
& (Join-Path $PSScriptRoot "make-msix.ps1") -Stage $stage -Name $name -Version $version -OutDir $OutDir `
    -PfxPath $PfxPath -Password $Password

$msix = Join-Path $OutDir "$name.msix"
if ($Install) {
    Write-Step "installing $msix"
    Add-AppxPackage -Path $msix
    Write-Host "Installed. Start it from the Start menu, then run:" -ForegroundColor Green
    Write-Host "  myware-voice model download"
}

Write-Host ""
Write-Host ("  Package: " + $msix) -ForegroundColor Green
Write-Host ("  Install: Add-AppxPackage -Path `"" + $msix + "`"")
