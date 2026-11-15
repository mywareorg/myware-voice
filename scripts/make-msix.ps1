#Requires -Version 5.1
<#
.SYNOPSIS
  Pack a staged Windows build into an MSIX and sign it with a test certificate.

.DESCRIPTION
  build-windows.ps1 stages a folder of executables and DLLs; this turns that
  folder into a package Windows will install. The order matters and is the
  documented manual-packaging sequence: lay out the files, render the manifest,
  index the assets into resources.pri, pack with makeappx, then sign.

  The package is unpacked again and its contents compared with the layout. A
  package that quietly loses a DLL installs fine and only fails at the point of
  use, on someone else's machine.

.PARAMETER Stage
  Directory holding hippo-dictate*.exe and the sherpa/VC runtime DLLs.

.PARAMETER OutDir
  Where the .msix, its .sha256 and the .pfx are written.

.PARAMETER Name
  Package base name, without the extension.

.PARAMETER Version
  Version to render into the manifest. Three parts are padded to four.

.PARAMETER PfxPath
  Signing certificate. Omit to leave the package unsigned.

.EXAMPLE
  .\scripts\make-msix.ps1 -Stage dist\hippo-dictate-0.2.0-win-x64 `
      -OutDir dist -Name hippo-dictate-0.2.0-win-x64 -Version 0.2.0
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Stage,
    [Parameter(Mandatory = $true)][string]$Name,
    [Parameter(Mandatory = $true)][string]$Version,
    [string]$OutDir = "dist",
    [string]$IdentityPath = "packaging\identity.json",
    [string]$PfxPath = "dist\hippo-dictate-test.pfx",
    [string]$Password = "hippo-dictate-test",
    [switch]$NoCertificate,
    [switch]$KeepLayout
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

trap {
    Write-Host ""
    Write-Host "Packing failed at:" -ForegroundColor Red
    Write-Host $_.InvocationInfo.PositionMessage
    Write-Host ("  Message: " + $_.Exception.Message)
    exit 1
}

. (Join-Path $PSScriptRoot "windows-sdk.ps1")
$root = Split-Path -Parent $PSScriptRoot
function Resolve-FromRoot { param([string]$Path) if ([System.IO.Path]::IsPathRooted($Path)) { $Path } else { Join-Path $root $Path } }
$Stage = Resolve-FromRoot $Stage
$OutDir = Resolve-FromRoot $OutDir
$IdentityPath = Resolve-FromRoot $IdentityPath
$PfxPath = Resolve-FromRoot $PfxPath

$makeappx = Resolve-SdkTool "makeappx.exe"
$makepri = Resolve-SdkTool "makepri.exe"
if (-not $makeappx) { throw "makeappx.exe not found. Install the Windows 10/11 SDK." }
if (-not $makepri) { throw "makepri.exe not found. Install the Windows 10/11 SDK." }
if (-not (Test-Path -LiteralPath $Stage)) { throw "Stage directory not found: $Stage" }

$identity = Get-Content -Path $IdentityPath -Raw | ConvertFrom-Json
foreach ($key in @("packageName", "publisher", "publisherDisplayName", "displayName", "description", "applicationId", "backgroundColor")) {
    if (-not $identity.PSObject.Properties[$key] -or -not $identity.PSObject.Properties[$key].Value) {
        throw "$IdentityPath is missing '$key'"
    }
}

$work = Join-Path $OutDir "$Name-msix"
$layout = Join-Path $work "layout"
if (Test-Path -LiteralPath $work) { Remove-Item -LiteralPath $work -Recurse -Force }
New-Item -ItemType Directory -Path $layout -Force | Out-Null

Write-Step "laying out the package"
# The three Windows executables and their runtime. The overlay is Linux-only and
# builds as an empty stub elsewhere, so it is not shipped here.
foreach ($exe in @("hippo-dictate.exe", "hippo-dictate-daemon.exe", "hippo-dictate-settings.exe")) {
    Copy-Item -LiteralPath (Join-Path $Stage $exe) $layout
}
# sherpa-onnx-c-api.dll resolves onnxruntime.dll from the application directory,
# and both import the VC runtime: a package that omits the runtime installs and
# then fails to load on a machine without the redistributable.
foreach ($dll in @("sherpa-onnx-c-api.dll", "onnxruntime.dll", "onnxruntime_providers_shared.dll",
        "vcruntime140.dll", "vcruntime140_1.dll", "msvcp140.dll", "msvcp140_1.dll", "concrt140.dll")) {
    $path = Join-Path $Stage $dll
    if (Test-Path -LiteralPath $path) {
        Copy-Item -LiteralPath $path $layout
    } elseif ($dll -like "vcruntime*" -or $dll -like "msvcp*" -or $dll -like "concrt*") {
        Write-Warn "$dll is missing from the stage; the package will need the VC redistributable"
    } else {
        throw "$dll is missing from $Stage"
    }
}
$assets = Join-Path $layout "Assets"
New-Item -ItemType Directory -Path $assets -Force | Out-Null
Copy-Item -Path (Join-Path $root "packaging\assets\*.png") $assets
Copy-Item -LiteralPath (Join-Path $root "LICENSE") $layout

Write-Step "rendering AppxManifest.xml"
$template = Get-Content -Path (Join-Path $root "packaging\AppxManifest.xml.template") -Raw
$values = [ordered]@{
    PackageName          = $identity.packageName
    Publisher            = $identity.publisher
    PublisherDisplayName = $identity.publisherDisplayName
    DisplayName          = $identity.displayName
    Description          = $identity.description
    ApplicationId        = $identity.applicationId
    BackgroundColor      = $identity.backgroundColor
    Version              = (ConvertTo-FourPartVersion -Version $Version)
}
foreach ($key in $values.Keys) {
    $template = $template.Replace("{{$key}}", [string]$values[$key])
}
if ($template -match '\{\{') { throw "The rendered manifest still has placeholders" }
Set-Content -Path (Join-Path $layout "AppxManifest.xml") -Value $template -Encoding UTF8

# Windows maps the tile label and the scaled asset variants through
# resources.pri. Without one the package installs with the executable's file
# name in the Start menu.
Write-Step "indexing resources.pri"
$priConfig = Join-Path $work "priconfig.xml"
& $makepri createconfig /cf $priConfig /dq en-US /o | Out-Host
if ($LASTEXITCODE -ne 0) { throw "makepri createconfig failed with $LASTEXITCODE" }
& $makepri new /pr $layout /cf $priConfig /of (Join-Path $layout "resources.pri") /mn $identity.packageName /o | Out-Host
if ($LASTEXITCODE -ne 0) { throw "makepri new failed with $LASTEXITCODE" }

$msix = Join-Path $OutDir "$Name.msix"
if (Test-Path -LiteralPath $msix) { Remove-Item -LiteralPath $msix -Force }
Write-Step "packing $msix"
& $makeappx pack /d $layout /p $msix /o | Out-Host
if ($LASTEXITCODE -ne 0) { throw "makeappx pack failed with $LASTEXITCODE" }

Write-Step "verifying the package contents"
$verify = Join-Path $work "verify"
& $makeappx unpack /p $msix /d $verify /o | Out-Host
if ($LASTEXITCODE -ne 0) { throw "makeappx unpack failed with $LASTEXITCODE" }
$packed = Get-ChildItem -LiteralPath $verify -Recurse -File |
    ForEach-Object { $_.FullName.Substring((Resolve-Path $verify).Path.Length).TrimStart('\') }
foreach ($required in @("hippo-dictate.exe", "hippo-dictate-daemon.exe", "hippo-dictate-settings.exe",
        "AppxManifest.xml", "resources.pri", "sherpa-onnx-c-api.dll", "onnxruntime.dll",
        "Assets\StoreLogo.png", "LICENSE")) {
    if ($packed -notcontains $required) { throw "$required is required in the package but is not in it" }
}
Write-Host ("  {0:N0} files in the package" -f $packed.Count) -ForegroundColor Green

if (-not $NoCertificate -and (Test-Path -LiteralPath $PfxPath)) {
    $signtool = Resolve-SdkTool "signtool.exe"
    if (-not $signtool) { throw "signtool.exe not found. Install the Windows 10/11 SDK." }
    Write-Step "signing with $PfxPath"
    & $signtool sign /fd SHA256 /a /f $PfxPath /p $Password $msix | Out-Host
    if ($LASTEXITCODE -ne 0) { throw "signtool sign failed with $LASTEXITCODE" }
    # /pa checks against the machine policy, which is what Add-AppxPackage uses.
    & $signtool verify /pa $msix | Out-Host
    if ($LASTEXITCODE -ne 0) { throw "signtool verify failed with $LASTEXITCODE" }
} elseif (-not $NoCertificate) {
    Write-Warn "no certificate at $PfxPath: the package is unsigned and will not install"
}

$hash = (Get-FileHash -LiteralPath $msix -Algorithm SHA256).Hash.ToLowerInvariant()
"$hash  $(Split-Path $msix -Leaf)" | Set-Content -Path "$msix.sha256" -Encoding ASCII

if (-not $KeepLayout) { Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue }
Write-Host ""
Write-Host ("  Package: " + $msix) -ForegroundColor Green
Write-Host ("  SHA-256: " + $hash)
