<#
.SYNOPSIS
  Enroll a Windows PC as a RigDesk desk: Tailscale (tagged), RigDesk as a boot service, and the PC's secrets.

.DESCRIPTION
  Run once, as administrator, from the one line the operator gives you:

    & ([scriptblock]::Create((irm https://github.com/deussalo/rigdesk/releases/download/<tag>/install.ps1))) `
        -Client acme -TailscaleAuthKey tskey-auth-... -Password ... -Pin ... -AgentToken ...

  It never prints a secret back. It installs Tailscale and joins the tailnet as tag:pc,tag:client-<client>; installs
  RigDesk from the release, checking its SHA-256; hands the permanent password, the admin settings PIN and the agent
  API token to the service through enroll.json (under Program Files, admin-only, deleted once applied), with
  -AgentShell on|off (on by default: Rig's agents may run commands as SYSTEM, every one logged) and -AgentJoin on|off
  (on by default: the operator may start an agent's desktop session from Rig with no share link); lets the
  direct port 21118 in from the tailnet only; and publishes the agent API with `tailscale serve`. Rerunning it with
  new secrets rotates them.

  A PC Rig does not reach over Tailscale takes -RigPairKey (Rig's public pairing key, from Rig's pair_key route) in
  place of -AgentToken and -TailscaleAuthKey: the person there invites an agent from the tray, and Rig redeems the
  invite through the relay. Given with -AgentToken, the key is kept, and invites still go over the tailnet.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)] [ValidatePattern('^[a-z0-9][a-z0-9-]{0,62}$')] [string] $Client,
    [string] $TailscaleAuthKey = '',
    [Parameter(Mandatory)] [ValidateLength(8, 64)] [string] $Password,
    [Parameter(Mandatory)] [ValidateLength(4, 32)] [string] $Pin,
    [ValidatePattern('^[A-Za-z0-9_-]{32,128}$')] [string] $AgentToken = '',
    # Rig's public pairing key: invites made on this PC are redeemed with it, through the relay when there is no -AgentToken.
    [ValidatePattern('^[A-Za-z0-9+/]{43}=$')] [string] $RigPairKey = '',
    # Whether Rig's agents may run commands on this PC as SYSTEM without a session (the client's written consent, README).
    [ValidateSet('on', 'off')] [string] $AgentShell = 'on',
    # Whether the operator may start an agent's desktop session from Rig, with no share link (the client's written consent, README).
    [ValidateSet('on', 'off')] [string] $AgentJoin = 'on',
    # Pinned by the release that ships this script; override only to test a build.
    [string] $PackageUrl = 'https://github.com/deussalo/rigdesk/releases/download/__TAG__/rigdesk-windows-x64.zip',
    [string] $PackageSha256 = '__SHA256__',
    [string] $Hostname = $env:COMPUTERNAME.ToLowerInvariant(),
    # For CI: install and enroll without touching Tailscale. Implied without -AgentToken.
    [switch] $SkipTailscale
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$AppName = 'RigDesk'
$DirectPort = 21118
$AgentPort = 21119

function Step($words) { Write-Host "==> $words" }

function Remove-SecretsFromHistory {
    # The line that started this carried the secrets; keep them out of PSReadLine's history file.
    Clear-History -ErrorAction SilentlyContinue
    try {
        $file = (Get-PSReadLineOption).HistorySavePath
    } catch {
        return
    }
    if ($file -and (Test-Path $file)) {
        $needles = @($TailscaleAuthKey, $Password, $Pin, $AgentToken) | Where-Object { $_ }
        $kept = Get-Content $file | Where-Object { $line = $_; -not ($needles | Where-Object { $line.Contains($_) }) }
        Set-Content -Path $file -Value $kept -Encoding UTF8
    }
}

$principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run this from an administrator PowerShell.'
}

if (-not $AgentToken -and -not $RigPairKey) { throw 'Give -AgentToken (Rig reaches this PC over Tailscale) or -RigPairKey (invites only, through the relay).' }
if (-not $AgentToken) { $SkipTailscale = [switch]$true }
if (-not $SkipTailscale -and -not $TailscaleAuthKey) { throw 'Give -TailscaleAuthKey (a tagged auth key from the operator).' }

try {
    # 1. Tailscale, tagged for this client.
    $tailscale = Join-Path $env:ProgramFiles 'Tailscale\tailscale.exe'
    if ($SkipTailscale) {
        Step 'Skipping Tailscale'
    } elseif (-not (Test-Path $tailscale)) {
        Step 'Installing Tailscale'
        $msi = Join-Path $env:TEMP 'tailscale-setup.msi'
        Invoke-WebRequest -UseBasicParsing -Uri 'https://pkgs.tailscale.com/stable/tailscale-setup-latest-amd64.msi' -OutFile $msi
        $p = Start-Process msiexec.exe -ArgumentList @('/i', "`"$msi`"", '/quiet', '/norestart', 'TS_UNATTENDEDMODE=always', 'TS_NOLAUNCH=true') -Wait -PassThru
        if ($p.ExitCode -ne 0) { throw "Tailscale install failed (msiexec exit $($p.ExitCode))" }
        Remove-Item $msi -Force
    }
    if (-not $SkipTailscale) {
        Step "Joining the tailnet as $Hostname (tag:pc, tag:client-$Client)"
        # The Tailscale service may still be starting right after its install.
        for ($try = 1; $try -le 10; $try++) {
            & $tailscale up "--auth-key=$TailscaleAuthKey" "--advertise-tags=tag:pc,tag:client-$Client" "--hostname=$Hostname" --unattended --reset 2>$null | Out-Null
            if ($LASTEXITCODE -eq 0) { break }
            if ($try -eq 10) { throw "tailscale up failed (exit $LASTEXITCODE)" }
            Start-Sleep -Seconds 3
        }
    }

    # 2. RigDesk, checked against the release's SHA-256, installed as a boot service.
    if ($PackageSha256 -notmatch '^[0-9a-fA-F]{64}$') { throw 'No package SHA-256 pinned: use the line from a RigDesk release.' }
    Step 'Downloading RigDesk'
    $work = Join-Path $env:TEMP ("rigdesk-" + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $work | Out-Null
    $zip = Join-Path $work 'rigdesk.zip'
    Invoke-WebRequest -UseBasicParsing -Uri $PackageUrl -OutFile $zip
    $hash = (Get-FileHash -Algorithm SHA256 $zip).Hash
    if ($hash -ne $PackageSha256.ToUpperInvariant()) { throw "Package SHA-256 mismatch: got $hash" }
    Expand-Archive -Path $zip -DestinationPath (Join-Path $work 'pkg')
    $exe = Get-ChildItem -Path (Join-Path $work 'pkg') -Recurse -Include 'rustdesk.exe', 'RigDesk.exe' | Select-Object -First 1
    if (-not $exe) { throw 'The package holds no RigDesk executable.' }

    Step 'Installing RigDesk as a service'
    # Not Start-Process -Wait: that also waits for every process the installer starts, and RigDesk keeps running.
    $p = Start-Process -FilePath $exe.FullName -ArgumentList '--silent-install' -PassThru
    if (-not $p.WaitForExit(300000)) { throw 'The RigDesk installer did not finish within 5 minutes.' }
    $deadline = (Get-Date).AddMinutes(3)
    while (-not (Get-Service -Name $AppName -ErrorAction SilentlyContinue)) {
        if ((Get-Date) -gt $deadline) { throw "The $AppName service did not appear." }
        Start-Sleep -Seconds 2
    }
    Set-Service -Name $AppName -StartupType Automatic

    $installDir = (Get-ItemProperty -Path "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\$AppName" -ErrorAction SilentlyContinue).InstallLocation
    if (-not $installDir) { $installDir = Join-Path $env:ProgramFiles $AppName }

    # 3. The PC's secrets, applied by the service at startup and deleted.
    Step "Setting the permanent password, admin PIN and agent API token; agent shell $AgentShell; sessions from Rig $AgentJoin"
    $enroll = Join-Path $installDir 'enroll.json'
    $json = @{ password = $Password; pin = $Pin; agent_token = $AgentToken; agent_shell = $AgentShell; agent_join = $AgentJoin; rig_pair_key = $RigPairKey } | ConvertTo-Json -Compress
    [IO.File]::WriteAllText($enroll, $json, (New-Object Text.UTF8Encoding($false)))
    $acl = New-Object Security.AccessControl.FileSecurity
    $acl.SetAccessRuleProtection($true, $false)
    foreach ($who in @('NT AUTHORITY\SYSTEM', 'BUILTIN\Administrators')) {
        $acl.AddAccessRule((New-Object Security.AccessControl.FileSystemAccessRule($who, 'FullControl', 'Allow')))
    }
    Set-Acl -Path $enroll -AclObject $acl
    Restart-Service -Name $AppName -Force
    $deadline = (Get-Date).AddMinutes(2)
    while (Test-Path $enroll) {
        if ((Get-Date) -gt $deadline) {
            Remove-Item $enroll -Force
            throw 'The service did not pick up the secrets (enroll.json removed). Is a desktop session running? Rerun the line.'
        }
        Start-Sleep -Seconds 2
    }

    # 4. The direct port from the tailnet only, and the agent API on the tailnet's HTTPS.
    Step "Letting $DirectPort in from the tailnet only"
    Get-NetFirewallRule -DisplayName "$AppName Service" -ErrorAction SilentlyContinue |
        Where-Object { $_.Direction -eq 'Inbound' } | Remove-NetFirewallRule
    Get-NetFirewallRule -DisplayName "$AppName tailnet" -ErrorAction SilentlyContinue | Remove-NetFirewallRule
    New-NetFirewallRule -DisplayName "$AppName tailnet" -Direction Inbound -Action Allow -Protocol TCP `
        -LocalPort $DirectPort -RemoteAddress '100.64.0.0/10', 'fd7a:115c:a1e0::/48' `
        -Program (Join-Path $installDir "$AppName.exe") | Out-Null

    Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
    if (-not $AgentToken) {
        Step 'Done (no Tailscale). Rig reaches this PC only by an invite made from the tray, through the relay.'
    } elseif ($SkipTailscale) {
        Step "Done (no Tailscale). The agent API is on 127.0.0.1:$AgentPort only."
    } else {
        Step 'Publishing the agent API with tailscale serve'
        & $tailscale serve --bg --https=443 "http://127.0.0.1:$AgentPort" | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "tailscale serve failed (exit $LASTEXITCODE); is HTTPS enabled for the tailnet?" }
        $dns = (& $tailscale status --json | ConvertFrom-Json).Self.DNSName.TrimEnd('.')
        Step "Done. Viewers connect to ${dns}:$DirectPort; Rig reaches the agent API at https://$dns/"
    }
} finally {
    Remove-SecretsFromHistory
}
