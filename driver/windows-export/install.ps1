[CmdletBinding()]
param(
    [string]$InfPath = (Join-Path $PSScriptRoot "interdriver-export.inf"),
    [string]$CliPath = (Join-Path $PSScriptRoot "interdriver-usb.exe")
)

$ErrorActionPreference = "Stop"
$principal = [Security.Principal.WindowsPrincipal]::new(
    [Security.Principal.WindowsIdentity]::GetCurrent()
)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw "Run this script from an elevated PowerShell window."
}

$InfPath = (Resolve-Path $InfPath).Path
$CliPath = (Resolve-Path $CliPath).Path
$modeIds = @(
    "USB\VID_05AC&PID_12A8",
    "USB\VID_05AC&PID_1281",
    "USB\VID_05AC&PID_1227"
)

foreach ($displayName in @("Apple Mobile Device Service", "Bonjour Service")) {
    $service = Get-Service -DisplayName $displayName -ErrorAction SilentlyContinue
    if ($service) {
        Stop-Service -InputObject $service -Force -ErrorAction SilentlyContinue
    }
}

Get-PnpDevice -PresentOnly -ErrorAction SilentlyContinue |
    Where-Object InstanceId -Match '^USB\\VID_05AC&PID_12A8&MI_' |
    Disable-PnpDevice -Confirm:$false -ErrorAction SilentlyContinue

& "$env:WINDIR\System32\pnputil.exe" /add-driver $InfPath
if ($LASTEXITCODE -notin 0, 3010) {
    throw "pnputil failed with exit code $LASTEXITCODE"
}

$parents = Get-PnpDevice -PresentOnly -ErrorAction SilentlyContinue |
    Where-Object InstanceId -Match '^USB\\VID_05AC&PID_(12A8|1281|1227)\\'
if (-not $parents) {
    throw "No attached iPhone was found. Attach it and run this script again."
}

foreach ($modeId in $modeIds) {
    if ($parents.InstanceId -like "$modeId\*") {
        & $CliPath windows force-driver $modeId $InfPath
        if ($LASTEXITCODE -ne 0) {
            throw "Driver binding failed for $modeId"
        }
    }
}

if (-not (Get-NetFirewallRule -DisplayName "InterDriver USB Exporter" -ErrorAction SilentlyContinue)) {
    New-NetFirewallRule `
        -DisplayName "InterDriver USB Exporter" `
        -Direction Inbound `
        -Action Allow `
        -Protocol TCP `
        -LocalPort 3240 | Out-Null
}

& $CliPath windows list
Write-Host "The Apple and Bonjour services remain stopped while this PC exports the iPhone."
