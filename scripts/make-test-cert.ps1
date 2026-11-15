#Requires -Version 5.1
<#
.SYNOPSIS
  Create the self-signed code-signing certificate a sideloaded MSIX needs.

.DESCRIPTION
  Windows only installs a package whose manifest Publisher matches the signing
  certificate's subject character for character, so the subject defaults to the
  Publisher in packaging/identity.json rather than a friendly name.

  The private half stays in Cert:\CurrentUser\My and is exported to a .pfx; the
  .cer is imported into Cert:\CurrentUser\TrustedPeople so this user's machine
  trusts packages signed by it. Both are per-user stores: no elevation.

  Re-running reuses the certificate already in the store, so repeated builds do
  not pile up duplicates. -Force replaces it, which invalidates packages signed
  with the old one.

.EXAMPLE
  .\scripts\make-test-cert.ps1

.EXAMPLE
  .\scripts\make-test-cert.ps1 -PfxPath D:\keys\hippo.pfx -Password correct-horse -Force
#>
[CmdletBinding()]
param(
    [string]$Subject,
    [string]$PfxPath = "dist\hippo-dictate-test.pfx",
    [string]$Password = "hippo-dictate-test",
    [switch]$NoTrust,
    [switch]$Force
)

$ErrorActionPreference = "Stop"

. (Join-Path $PSScriptRoot "windows-sdk.ps1")
$root = Split-Path -Parent $PSScriptRoot

if (-not $Subject) {
    $identityFile = Join-Path $root "packaging\identity.json"
    if (-not (Test-Path $identityFile)) { throw "packaging\identity.json is missing" }
    $identity = Get-Content -Path $identityFile -Raw | ConvertFrom-Json
    if (-not $identity.publisher) { throw "packaging\identity.json has no publisher" }
    $Subject = $identity.publisher
}

$existing = Get-ChildItem Cert:\CurrentUser\My -CodeSigningCert -ErrorAction SilentlyContinue |
    Where-Object { $_.Subject -eq $Subject } |
    Select-Object -First 1

if ($existing -and -not $Force) {
    $cert = $existing
    Write-Step "reusing the certificate already in Cert:\CurrentUser\My"
} else {
    if ($existing) {
        Write-Warn "replacing the existing certificate for $Subject"
        Remove-Item -LiteralPath ("Cert:\CurrentUser\My\" + $existing.Thumbprint) -Force
    }
    Write-Step "creating a self-signed code-signing certificate for $Subject"
    $cert = New-SelfSignedCertificate `
        -Type CodeSigningCert `
        -Subject $Subject `
        -KeyUsage DigitalSignature `
        -FriendlyName "Hippo Dictate self-signed" `
        -NotAfter (Get-Date).AddYears(3) `
        -CertStoreLocation "Cert:\CurrentUser\My"
}

$PfxPath = if ($PfxPath -and -not [System.IO.Path]::IsPathRooted($PfxPath)) {
    Join-Path $root $PfxPath
} else { $PfxPath }
New-Item -ItemType Directory -Force -Path (Split-Path -Parent $PfxPath) | Out-Null

$secure = ConvertTo-SecureString -String $Password -Force -AsPlainText
Export-PfxCertificate -Cert $cert -FilePath $PfxPath -Password $secure | Out-Null
$cerPath = [System.IO.Path]::ChangeExtension($PfxPath, ".cer")
Export-Certificate -Cert $cert -FilePath $cerPath -Type CERT | Out-Null

if (-not $NoTrust) {
    # TrustedPeople is the store Windows checks for sideloaded packages signed by
    # a certificate the machine has not otherwise been told to trust.
    Import-Certificate -FilePath $cerPath -CertStoreLocation "Cert:\CurrentUser\TrustedPeople" | Out-Null
    Write-Step "trusted for this user: Cert:\CurrentUser\TrustedPeople"
}

Write-Host ""
Write-Host "  subject     $($cert.Subject)"
Write-Host "  thumbprint  $($cert.Thumbprint)"
Write-Host "  pfx         $PfxPath"
Write-Host "  cer         $cerPath"
Write-Host ""
Write-Host "Pass -PfxPath and -Password to make-msix.ps1, or run package-msix.ps1" -ForegroundColor Green
