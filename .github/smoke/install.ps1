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
# An error nothing caught (a request the server cut) shows the same state as a failed check.
trap { Fail "uncaught: $_ at $($_.InvocationInfo.PositionMessage)" }

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

# The management lane (p-fda4): a command with no session runs as SYSTEM in session 0 and opens no window where the
# user is; it waits for no human, the switch and Block refuse it, pausing cuts jobs, and each command is journaled.
if (-not $log.Contains("enroll: agent shell on")) { Fail "the service log lacks 'enroll: agent shell on' (the install line's default)" }
$lane = @{ id = "lane-smoke"; title = "Lane smoke"; graph = "ci" }
function Lane($body) {
  try { Post "/shell" ($body + @{ agent = $lane }) } catch {
    if ($_.ErrorDetails.Message) { $_.ErrorDetails.Message | ConvertFrom-Json } else { Fail "POST /shell failed: $_" }
  }
}
function Output($answer) { ($answer.text | ConvertFrom-Json).output }
$mySession = (Get-Process -Id $PID).SessionId
$windowsBefore = @(Get-Process | Where-Object { $_.SessionId -eq $mySession -and $_.MainWindowHandle -ne 0 } | ForEach-Object { $_.Id })
$processesBefore = @(Get-Process | Where-Object { $_.SessionId -eq $mySession } | ForEach-Object { $_.Id })
Post "/debug/viewer/human" @{} | Out-Null
$ran = Lane @{ command = 'whoami; (Get-Process -Id $PID).SessionId; Start-Process notepad.exe; Start-Sleep 2; @(Get-Process notepad | Where-Object { $_.SessionId -eq 0 }).Count' }
$lines = @((Output $ran) -split "`r?`n" | Where-Object { $_ })
Write-Host "lane (smoke in session $mySession): $($lines -join ' | ')"
if (-not $ran.ok -or $lines[0] -ne "nt authority\system" -or $lines[1] -ne "0" -or [int] $lines[2] -lt 1) { Fail "the lane did not run as SYSTEM in session 0 while the human held input: $($ran | ConvertTo-Json -Compress)" }
if (@(Get-Process notepad -ErrorAction SilentlyContinue | Where-Object { $_.SessionId -ne 0 }).Count) { Fail "a notepad the lane started is outside session 0" }
$newWindows = @(Get-Process | Where-Object { $_.SessionId -eq $mySession -and $_.MainWindowHandle -ne 0 -and $windowsBefore -notcontains $_.Id })
$newProcesses = @(Get-Process | Where-Object { $_.SessionId -eq $mySession -and $processesBefore -notcontains $_.Id -and $_.Id -ne $PID })
Write-Host "new in session ${mySession}: windows [$($newWindows.ProcessName -join ', ')], processes [$($newProcesses.ProcessName -join ', ')]"
if ($newWindows.Count) { Fail "the lane opened windows in the smoke's session: $($newWindows.ProcessName -join ', ')" }
if ($newProcesses | Where-Object { $_.ProcessName -match '^(cmd|powershell|conhost|notepad)$' }) { Fail "the lane started processes in the smoke's session: $($newProcesses.ProcessName -join ', ')" }
Lane @{ command = 'Stop-Process -Name notepad -Force' } | Out-Null

$asUser = Lane @{ command = "whoami"; as = "user" }
if (-not $asUser.ok -or (Output $asUser) -match "system") { Fail "as user did not run as the signed-in user: $($asUser | ConvertTo-Json -Compress)" }
Write-Host "lane as user: $((Output $asUser).Trim())"

$job = (Lane @{ command = 'Start-Sleep 2; "from the job"; exit 4'; background = $true }).text | ConvertFrom-Json
foreach ($i in 1..30) { $read = (Lane @{ job = $job.job }).text | ConvertFrom-Json; if ($read.state -ne "running") { break }; Start-Sleep 1 }
if ($read.state -ne "exited" -or $read.exit_code -ne 4 -or $read.output -notmatch "from the job") { Fail "the background job did not end with exit 4 and its output: $($read | ConvertTo-Json -Compress)" }
Write-Host "lane job: $($read.state) $($read.exit_code)"

$switch = Join-Path $svcHome "config\agent\agent-shell"
Set-Content -Path $switch -Value "off" -NoNewline
$off = Lane @{ command = "whoami" }
Set-Content -Path $switch -Value "on" -NoNewline
if ($off.code -ne "shell_off") { Fail "the lane ran with its switch off: $($off | ConvertTo-Json -Compress)" }

# The settings' pause sets this option through the server's IPC, as `RigDesk.exe --option` does when run as SYSTEM.
function Set-Paused($value) {
  $action = New-ScheduledTaskAction -Execute (Join-Path $installDir "RigDesk.exe") -Argument "--option agent-shell-paused $value"
  Register-ScheduledTask -TaskName "rigdesk-smoke-pause" -Action $action -User "NT AUTHORITY\SYSTEM" -RunLevel Highest -Force | Out-Null
  Start-ScheduledTask -TaskName "rigdesk-smoke-pause"
  foreach ($i in 1..30) { Start-Sleep 1; if ((Get-ScheduledTask -TaskName "rigdesk-smoke-pause").State -eq "Ready") { break } }
  Unregister-ScheduledTask -TaskName "rigdesk-smoke-pause" -Confirm:$false
}
$long = (Lane @{ command = 'Start-Sleep 120'; background = $true }).text | ConvertFrom-Json
Set-Paused "Y"
foreach ($i in 1..10) { $paused = Lane @{ command = "whoami" }; if ($paused.code -eq "shell_paused") { break }; Start-Sleep 1 }
if ($paused.code -ne "shell_paused") { Fail "the lane ran while paused: $($paused | ConvertTo-Json -Compress)" }
Start-Sleep 7
Set-Paused "N"
foreach ($i in 1..10) { $resumed = Lane @{ command = "whoami" }; if ($resumed.ok) { break }; Start-Sleep 1 }
if (-not $resumed.ok) { Fail "the lane did not run once resumed: $($resumed | ConvertTo-Json -Compress)" }
$cut = (Lane @{ job = $long.job }).text | ConvertFrom-Json
if ($cut.state -ne "ended" -or $cut.why -notmatch "paused") { Fail "pausing did not cut the running job: $($cut | ConvertTo-Json -Compress)" }
Write-Host "lane pause: refused $($paused.code); job $($cut.why)"
$after = Invoke-RestMethod -Uri "http://127.0.0.1:21119/health" -Headers $auth
if ($after.boot -ne $health.boot) { Fail "the agent API restarted while the lane was paused and resumed: boot $($health.boot) became $($after.boot)" }

$minted = Post "/debug/mint" @{}
$own = Post "/claim" @{ grant = $minted.grant; secret = $minted.secret; agent = $lane }
Post "/debug/viewer/block" @{} | Out-Null
$blocked = Lane @{ command = "whoami" }
Post "/debug/viewer/unblock" @{} | Out-Null
if ($blocked.code -ne "blocked") { Fail "the lane ran for a blocked agent: $($blocked | ConvertTo-Json -Compress)" }

$journal = (Invoke-RestMethod -Uri "http://127.0.0.1:21119/log?n=200" -Headers $auth).entries | Where-Object { $_.what -eq "shell" }
$outcomes = @($journal | ForEach-Object { $_.outcome })
Write-Host "journal: $($outcomes -join ', ')"
foreach ($want in @("exit 0", "started", "exit 4", "refused: shell_off", "refused: shell_paused", "refused: blocked")) {
  if ($outcomes -notcontains $want) { Fail "the journal lacks a shell entry '$want'" }
}
$first = $journal | Where-Object { $_.detail.command -like "whoami;*" } | Select-Object -First 1
if ($first.detail.graph -ne "ci" -or $first.detail.as -ne "system" -or $null -eq $first.detail.took_ms) { Fail "a journaled command lacks its graph, as or duration: $($first | ConvertTo-Json -Compress)" }
$digest = Join-Path $env:ProgramData "RigDesk\managed-by-rig.txt"
if (-not (Test-Path $digest) -or -not (Get-Content -Raw $digest).Contains("Lane smoke")) { Fail "the tray's list of commands lacks the lane's" }
$usersMay = @((Get-Acl $digest).Access | Where-Object { $_.IdentityReference -match "Users$" } | ForEach-Object { "$($_.FileSystemRights)" })
if ($usersMay -notcontains "ReadAndExecute, Synchronize" -and $usersMay -notcontains "Read, Synchronize") { Fail "users may $($usersMay -join '/') the tray's list, not only read it" }

$sample = $null
foreach ($i in 1..120) {
  $samples = (Invoke-RestMethod -Uri "http://127.0.0.1:21119/samples?after=0" -Headers $auth).samples
  if ($samples) { $sample = @($samples)[-1]; break }
  Start-Sleep 1
}
if (-not $sample) { Fail "no health sample within 2 minutes of the checks" }
Write-Host "health sample: $($sample | ConvertTo-Json -Compress -Depth 8)"
if ($sample.taken -ne "ok") { Fail "the health sample failed: $($sample.why)" }
if (-not ($sample.figures.volumes | Where-Object { $_.name -eq "C:" -and $_.free_mb -gt 0 })) { Fail "the health sample has no C: with free space" }
if (-not $sample.figures.cpu_ms -or -not $sample.took_ms) { Fail "the health sample lacks its CPU time or duration" }
Write-Host "install smoke passed"
