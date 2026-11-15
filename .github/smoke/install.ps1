# Installer smoke test (plan S3): the install line, minus Tailscale, against the build just made. Checks the service
# runs at boot start, the secrets reach it without being printed, the direct port is open to the tailnet only, and
# the agent API answers with the enrolled token, and that the service-spawned server (SYSTEM in the user's session)
# runs the agent's shell as the user with its output, and takes a screenshot.
$ErrorActionPreference = "Stop"
function Show-State {
  Write-Host "--- processes"
  Get-CimInstance Win32_Process | Where-Object { $_.Name -match '^(rustdesk|rigdesk|cmd|conhost|consent|powershell)\.exe$' } |
    ForEach-Object { Write-Host "$($_.ProcessId) <- $($_.ParentProcessId) $($_.Name): $($_.CommandLine)" }
  Write-Host "--- service: $((Get-Service -Name RigDesk -ErrorAction SilentlyContinue).Status)"
  Write-Host "--- EnableLUA: $((Get-ItemProperty HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System).EnableLUA)"
  foreach ($dir in @("$env:APPDATA\RigDesk\log", "C:\Windows\ServiceProfiles\LocalService\AppData\Roaming\RigDesk\log")) {
    Get-ChildItem -Path $dir -Recurse -Filter *.log -ErrorAction SilentlyContinue | ForEach-Object {
      Write-Host "--- $($_.FullName)"; Get-Content $_.FullName -Tail 60 | ForEach-Object { Write-Host $_ }
    }
  }
}
function Fail($why) { Write-Host "::error::install smoke: $why"; Show-State; exit 1 }

$zip = Join-Path $PWD "rigdesk-windows-x64.zip"
Compress-Archive -Path ".\rigdesk\*" -DestinationPath $zip -Force
$sha = (Get-FileHash -Algorithm SHA256 $zip).Hash
$token = [guid]::NewGuid().ToString("N") + [guid]::NewGuid().ToString("N")
$password = "Ci-" + [guid]::NewGuid().ToString("N").Substring(0, 12)
# Masked so that a leak fails the check below without landing in the log.
Write-Host "::add-mask::$password"
Write-Host "::add-mask::$token"

# The service's server reads the machine environment when it starts: the debug mint lets this check claim a session.
[Environment]::SetEnvironmentVariable("RIGDESK_AGENT_DEBUG_MINT", "1", "Machine")

try {
  $lines = & .\install\install.ps1 -Client ci -SkipTailscale -Password $password -Pin "2468" -AgentToken $token `
    -PackageUrl ([Uri]$zip).AbsoluteUri -PackageSha256 $sha *>&1 | ForEach-Object { $s = "$_"; Write-Host $s; $s }
} catch { Fail "the install line failed: $_" }
$out = $lines -join "`n"
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

$auth = @{ Authorization = "Bearer $token" }
function Post($path, $body, $headers = $auth) {
  Invoke-RestMethod -Method Post -Uri "http://127.0.0.1:21119$path" -Headers $headers -ContentType "application/json" -Body ($body | ConvertTo-Json -Depth 8 -Compress)
}
try { $minted = Post "/debug/mint" @{} } catch { Fail "the service's server has no debug mint: $_" }
$claim = Post "/claim" @{ grant = $minted.grant; secret = $minted.secret; agent = @{ id = "install-smoke"; title = "Install smoke"; graph = "ci" } }
$session = $auth + @{ "X-Desk-Session" = $claim.session }
function Tool($id, $name, $arguments = @{}) {
  (Post "/mcp" @{ jsonrpc = "2.0"; id = $id; method = "tools/call"; params = @{ name = $name; arguments = $arguments } } $session).result
}
$ran = Tool 1 "shell" @{ command = "whoami; cmd /c exit 3" }
$answer = ($ran.content | Where-Object { $_.type -eq "text" } | Select-Object -First 1).text | ConvertFrom-Json
if ($ran.isError -or $answer.exit_code -ne 3 -or $answer.output -notmatch "\\") { Fail "the service's shell did not answer whoami and exit 3 as the user: $($ran | ConvertTo-Json -Depth 6 -Compress)" }
if ($answer.output -match "system") { Fail "the service's shell ran as SYSTEM: $($answer.output)" }
Write-Host "service shell: $($answer.output.Trim()), exit $($answer.exit_code)"
$shot = Tool 2 "screenshot"
$png = $shot.content | Where-Object { $_.type -eq "image" } | Select-Object -First 1
if ($shot.isError -or $null -eq $png) { Fail "the service's screenshot failed: $(($shot.content | Where-Object { $_.type -eq 'text' }).text)" }
Write-Host "service screenshot: $($png.data.Length) base64 chars"
Write-Host "install smoke passed"
