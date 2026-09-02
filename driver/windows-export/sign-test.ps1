[CmdletBinding()]
param()

$ErrorActionPreference = "Stop"
$principal = [Security.Principal.WindowsPrincipal]::new(
    [Security.Principal.WindowsIdentity]::GetCurrent()
)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw "Run this script from an elevated PowerShell window."
}

$subject = "CN=InterDriver Test Driver Signing"
$cert = Get-ChildItem Cert:\LocalMachine\My |
    Where-Object { $_.Subject -eq $subject -and $_.HasPrivateKey } |
    Select-Object -First 1
if (-not $cert) {
    $cert = New-SelfSignedCertificate `
        -Type CodeSigningCert `
        -Subject $subject `
        -CertStoreLocation Cert:\LocalMachine\My `
        -KeyAlgorithm RSA `
        -KeyLength 3072 `
        -HashAlgorithm SHA256
}

$certificatePath = Join-Path $PSScriptRoot "interdriver-test.cer"
Export-Certificate -Cert $cert -FilePath $certificatePath -Force | Out-Null
Import-Certificate -FilePath $certificatePath -CertStoreLocation Cert:\LocalMachine\Root | Out-Null
Import-Certificate -FilePath $certificatePath -CertStoreLocation Cert:\LocalMachine\TrustedPublisher | Out-Null

$sys = Join-Path $PSScriptRoot "interdriver-export.sys"
$cat = Join-Path $PSScriptRoot "interdriver-export.cat"
Remove-Item $cat -Force -ErrorAction SilentlyContinue
$signature = Set-AuthenticodeSignature $sys $cert
if ($signature.Status -ne "Valid") {
    throw "SYS signing failed: $($signature.StatusMessage)"
}
New-FileCatalog -Path $PSScriptRoot -CatalogFilePath $cat -CatalogVersion 2.0 | Out-Null
$signature = Set-AuthenticodeSignature $cat $cert
if ($signature.Status -ne "Valid") {
    throw "catalog signing failed: $($signature.StatusMessage)"
}

Write-Host "Test-signed interdriver-export.sys and interdriver-export.cat."
Write-Host "If test signing is not enabled, run: bcdedit /set testsigning on"
Write-Host "Then reboot before running install.ps1."
