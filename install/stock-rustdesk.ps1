# Puts a fresh Windows PC on our own RustDesk ID server, ready for a Rig agent through rigdesk-bridge.
# Run in an administrator PowerShell:
#   & .\stock-rustdesk.ps1 -Config '<config string>'
# It installs stock RustDesk (pinned, SHA-256 checked), points it at the ID server in the config string, sets a
# fresh permanent password, and prints the PC's ID and that password: give both to the Rig operator.
param(
    [Parameter(Mandatory = $true)][string]$Config,
    [string]$Password = ''
)
$ErrorActionPreference = 'Stop'
$Version = '1.5.3'
$Sha256 = '523BA0EFBF62A21739961B62C7DAAFFB44D3DA86139C546B364FD4E1333ED249'

if (-not ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run this in an administrator PowerShell.'
}
[Net.ServicePointManager]::SecurityProtocol = 'Tls12'
$exe = Join-Path $env:ProgramFiles 'RustDesk\rustdesk.exe'
if (-not (Test-Path $exe)) {
    $setup = Join-Path $env:TEMP "rustdesk-$Version-x86_64.exe"
    Invoke-WebRequest -UseBasicParsing "https://github.com/rustdesk/rustdesk/releases/download/$Version/rustdesk-$Version-x86_64.exe" -OutFile $setup
    if ((Get-FileHash $setup -Algorithm SHA256).Hash -ne $Sha256) { Remove-Item $setup; throw 'RustDesk installer checksum mismatch' }
    Start-Process $setup -ArgumentList '--silent-install' -Wait
    Remove-Item $setup
    for ($i = 0; $i -lt 60 -and -not ((Test-Path $exe) -and (Get-Service RustDesk -ErrorAction SilentlyContinue).Status -eq 'Running'); $i++) { Start-Sleep 2 }
    if (-not (Test-Path $exe)) { throw 'RustDesk did not install' }
}
if (-not $Password) {
    $chars = [char[]]'ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnpqrstuvwxyz23456789'
    $bytes = New-Object byte[] 20; [Security.Cryptography.RandomNumberGenerator]::Create().GetBytes($bytes)
    $Password = -join ($bytes | ForEach-Object { $chars[$_ % $chars.Length] })
}
& $exe --config $Config | Out-Null
& $exe --password $Password | Out-Null
Start-Sleep 3
$id = (& $exe --get-id | Out-String).Trim()
if (-not $id) { throw 'RustDesk gave no ID; is its service running?' }
Write-Host ''
Write-Host "RustDesk ID: $id"
Write-Host "Password:    $Password"
Write-Host 'Give both to the Rig operator. The RustDesk window should say Ready at the bottom.'
