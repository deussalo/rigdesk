# Installer smoke test (plan S3): the install line, minus Tailscale, against the build just made. Checks the service
# runs at boot start, the secrets reach it without being printed, the direct port is open to the tailnet only, and
# the agent API answers with the enrolled token.
$ErrorActionPreference = "Stop"
function Fail($why) { Write-Host "::error::install smoke: $why"; exit 1 }

$zip = Join-Path $PWD "rigdesk-windows-x64.zip"
Compress-Archive -Path ".\rigdesk\*" -DestinationPath $zip -Force
$sha = (Get-FileHash -Algorithm SHA256 $zip).Hash
$token = [guid]::NewGuid().ToString("N") + [guid]::NewGuid().ToString("N")
$password = "Ci-" + [guid]::NewGuid().ToString("N").Substring(0, 12)

$out = & .\install\install.ps1 -Client ci -SkipTailscale -Password $password -Pin "2468" -AgentToken $token `
  -PackageUrl ([Uri]$zip).AbsoluteUri -PackageSha256 $sha *>&1 | Out-String
Write-Host $out
foreach ($secret in @($password, $token)) { if ($out.Contains($secret)) { Fail "the install line printed a secret" } }

$service = Get-Service -Name RigDesk -ErrorAction SilentlyContinue
if (-not $service) { Fail "no RigDesk service" }
if ($service.StartType -ne "Automatic") { Fail "the service starts $($service.StartType), not at boot" }
if ($service.Status -ne "Running") { Fail "the service is $($service.Status)" }

$installDir = Join-Path $env:ProgramFiles "RigDesk"
if (Test-Path (Join-Path $installDir "enroll.json")) { Fail "enroll.json was left behind" }

$svcHome = "C:\Windows\ServiceProfiles\LocalService\AppData\Roaming\RigDesk"
$tokenFile = Join-Path $svcHome "config\agent\api.token"
if (-not (Test-Path $tokenFile)) { Fail "no agent API token at $tokenFile" }
if ((Get-Content -Raw $tokenFile).Trim() -ne $token) { Fail "the enrolled agent API token differs" }
$log = Get-ChildItem -Path (Join-Path $svcHome "log") -Recurse -Filter *.log -ErrorAction SilentlyContinue | Get-Content -ErrorAction SilentlyContinue | Out-String
foreach ($line in @("enroll: permanent password set", "enroll: admin settings PIN set", "enroll: agent API token set")) {
  if (-not $log.Contains($line)) { Fail "the service log lacks '$line'" }
}
if ($log.Contains($password) -or $log.Contains($token)) { Fail "a secret reached the service log" }

$rule = Get-NetFirewallRule -DisplayName "RigDesk tailnet" -ErrorAction SilentlyContinue
if (-not $rule) { Fail "no tailnet firewall rule" }
$addresses = ($rule | Get-NetFirewallAddressFilter).RemoteAddress -join ","
if ($addresses -notmatch "100\.64\.0\.0") { Fail "the firewall rule admits $addresses" }
$open = Get-NetFirewallRule -Direction Inbound -Enabled True -ErrorAction SilentlyContinue | Where-Object { $_.DisplayName -like "RigDesk*" } |
  Where-Object { ($_ | Get-NetFirewallAddressFilter).RemoteAddress -contains "Any" }
if ($open) { Fail "an inbound RigDesk rule admits any address: $($open.DisplayName -join ', ')" }

$up = $false
foreach ($i in 1..90) {
  try { $health = Invoke-RestMethod -Uri "http://127.0.0.1:21119/health" -Headers @{ Authorization = "Bearer $token" }; $up = $true; break } catch { Start-Sleep 1 }
}
if (-not $up) { Fail "the installed agent API did not answer within 90 s" }
Write-Host "health: $($health | ConvertTo-Json -Compress)"
Write-Host "install smoke passed"
