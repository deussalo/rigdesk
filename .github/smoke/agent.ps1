# Agent API smoke test (plan acceptance 4.4): the built server, headless on the runner, against Notepad.
# Mints a grant through the debug-only route, claims it, then checks screenshot, UIA snapshot, type, Block and a holder change.
param([string]$Exe = ".\rigdesk\rustdesk.exe")
$ErrorActionPreference = "Stop"
$token = "smoke-" + [guid]::NewGuid().ToString("N")
$dir = Join-Path $env:APPDATA "RigDesk\config\agent"
New-Item -ItemType Directory -Force $dir | Out-Null
Set-Content -NoNewline -Path (Join-Path $dir "api.token") -Value $token
$env:RIGDESK_AGENT_DEBUG_MINT = "1"
$base = "http://127.0.0.1:21119"
$auth = @{ Authorization = "Bearer $token" }

function Fail($why) { Write-Host "::error::agent smoke: $why"; exit 1 }
function Post($path, $body, $headers = $auth) {
  Invoke-RestMethod -Method Post -Uri "$base$path" -Headers $headers -ContentType "application/json" -Body ($body | ConvertTo-Json -Depth 8 -Compress)
}
$script:id = 0
function Tool($name, $arguments = @{}) {
  $script:id += 1
  $reply = Post "/mcp" @{ jsonrpc = "2.0"; id = $script:id; method = "tools/call"; params = @{ name = $name; arguments = $arguments } } ($auth + @{ "X-Desk-Session" = $script:session })
  if ($null -eq $reply.result) { Fail "$name gave no result: $($reply | ConvertTo-Json -Depth 8 -Compress)" }
  return $reply.result
}
function Text($result) { ($result.content | Where-Object { $_.type -eq "text" } | ForEach-Object { $_.text }) -join "`n" }

$notepad = Start-Process notepad -PassThru
$server = Start-Process $Exe -ArgumentList "--server" -PassThru
try {
  $up = $false
  foreach ($i in 1..60) {
    try { $health = Invoke-RestMethod -Uri "$base/health" -Headers $auth; $up = $true; break } catch { Start-Sleep 1 }
  }
  if (-not $up) { Fail "the agent API did not answer on $base within 60 s" }
  Write-Host "health: $($health | ConvertTo-Json -Compress)"

  $minted = Post "/debug/mint" @{}
  $claim = Post "/claim" @{ grant = $minted.grant; secret = $minted.secret; agent = @{ id = "smoke"; title = "Smoke"; graph = "ci" } }
  $script:session = $claim.session
  if ($claim.standing.holder -ne "agent") { Fail "after claim the holder is $($claim.standing.holder), not agent" }

  $shot = Tool "screenshot"
  $png = $shot.content | Where-Object { $_.type -eq "image" } | Select-Object -First 1
  if ($shot.isError -or $null -eq $png -or $png.data.Length -lt 100) { Fail "screenshot is empty: $(Text $shot)" }
  Write-Host "screenshot: $($png.data.Length) base64 chars"

  $null = Tool "app" @{ action = "switch"; name = "Notepad" }
  $null = Tool "wait" @{ ms = 1000 }
  $tree = Text (Tool "snapshot")
  $edit = ($tree -split "`n") | Where-Object { $_ -match "^\s*Edit\b" -and $_ -match "@(\d+),(\d+)" } | Select-Object -First 1
  if (-not $edit) { Fail "the UIA snapshot shows no edit box for Notepad:`n$tree" }
  Write-Host "edit box: $edit"
  $null = $edit -match "@(\d+),(\d+)"
  $null = Tool "click" @{ x = [int]$Matches[1]; y = [int]$Matches[2] }

  $words = "rigdesk smoke $([guid]::NewGuid().ToString('N').Substring(0, 8))"
  $typed = Tool "type" @{ text = $words }
  if ($typed.isError) { Fail "type refused: $(Text $typed)" }
  $null = Tool "shortcut" @{ keys = "ctrl+a" }
  $null = Tool "shortcut" @{ keys = "ctrl+c" }
  $null = Tool "wait" @{ ms = 500 }
  $clip = Text (Tool "clipboard")
  if ($clip.Trim() -ne $words) { Fail "Notepad holds '$clip', not the typed '$words'" }
  Write-Host "typed into Notepad: $clip"

  $null = Post "/debug/viewer/human" @{}
  $held = Tool "type" @{ text = "x" }
  if (-not $held.isError -or (Text $held) -notmatch "took over") { Fail "an action ran while the human held input: $(Text $held)" }
  $read = Tool "screenshot"
  if ($read.isError) { Fail "a read was refused while the human held input: $(Text $read)" }
  $null = Post "/debug/viewer/hand" @{}
  $back = Tool "wait" @{ ms = 10 }
  if ($back.isError) { Fail "handing input back did not take effect: $(Text $back)" }

  $null = Post "/debug/viewer/block" @{}
  $blocked = Tool "screenshot"
  if (-not $blocked.isError -or (Text $blocked) -notmatch "blocked") { Fail "a read ran while blocked: $(Text $blocked)" }
  Write-Host "blocked: $(Text $blocked)"
  Write-Host "agent smoke passed"
} finally {
  Stop-Process -Id $server.Id -Force -ErrorAction SilentlyContinue
  Get-Process rustdesk -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
  Stop-Process -Id $notepad.Id -Force -ErrorAction SilentlyContinue
}
