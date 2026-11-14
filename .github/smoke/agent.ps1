# Agent API smoke test (plan acceptance 4.4): the built server, headless on the runner, against Notepad.
# Mints a grant through the debug-only route, claims it, then checks screenshot, type, shell, a stdio MCP server put on a
# port by `--mcp` and called through the API, Block and a holder change.
param([string]$Exe = ".\rigdesk\RigDesk.exe")
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
# What the desktop looked like when a check failed, kept as a CI artifact.
function Evidence($why) {
  $out = Join-Path $env:RUNNER_TEMP "agent-smoke"
  New-Item -ItemType Directory -Force $out | Out-Null
  try {
    $shot = Tool "screenshot"
    $png = $shot.content | Where-Object { $_.type -eq "image" } | Select-Object -First 1
    if ($png) { [IO.File]::WriteAllBytes((Join-Path $out "screen.png"), [Convert]::FromBase64String($png.data)) }
  } catch { Write-Host "no evidence: $_" }
  Fail $why
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
  # A switched-to Notepad has its text area focused.
  $null = Tool "wait" @{ ms = 1000 }

  $words = "rigdesk smoke $([guid]::NewGuid().ToString('N').Substring(0, 8))"
  $typed = Tool "type" @{ text = $words }
  if ($typed.isError) { Fail "type refused: $(Text $typed)" }
  $null = Tool "shortcut" @{ keys = "ctrl+a" }
  $null = Tool "shortcut" @{ keys = "ctrl+c" }
  $null = Tool "wait" @{ ms = 500 }
  $clip = Text (Tool "clipboard")
  if ($clip.Trim() -ne $words) { Evidence "Notepad holds '$clip', not the typed '$words'" }
  Write-Host "typed into Notepad: $clip"

  $ran = Tool "shell" @{ command = "whoami; cmd /c exit 3" }
  $answer = (Text $ran) | ConvertFrom-Json
  if ($ran.isError -or $answer.exit_code -ne 3 -or $answer.output -notmatch "\\") { Fail "shell did not answer whoami and exit 3: $(Text $ran)" }
  Write-Host "shell: $($answer.output.Trim()), exit $($answer.exit_code)"

  # A stdio MCP server (Python's standard library only) behind `--mcp`, found by the scan and called through the API.
  $echo = Join-Path $env:RUNNER_TEMP "echo_mcp.py"
  Set-Content -Path $echo -Encoding ascii -Value @'
import json, sys
for line in sys.stdin:
    m = json.loads(line)
    if "id" not in m:
        continue
    method, i = m.get("method"), m["id"]
    if method == "initialize":
        r = {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "Smoke Echo", "version": "1"}}
    elif method == "tools/call":
        r = {"content": [{"type": "text", "text": "echo: " + m["params"]["arguments"]["text"]}], "isError": False}
    else:
        print(json.dumps({"jsonrpc": "2.0", "id": i, "error": {"code": -32601, "message": method}}), flush=True)
        continue
    print(json.dumps({"jsonrpc": "2.0", "id": i, "result": r}), flush=True)
'@
  $wrapper = Start-Process $Exe -ArgumentList "--mcp", "--", "python", "`"$echo`"" -PassThru
  $servers = $null
  foreach ($i in 1..30) {
    $servers = Invoke-RestMethod -Uri "$base/servers" -Headers $auth
    if (@($servers.servers | Where-Object { $_.name -eq "smoke-echo" }).Count -eq 1) { break }
    Start-Sleep 1
  }
  if (@($servers.servers | Where-Object { $_.name -eq "smoke-echo" }).Count -ne 1) { Fail "the server behind --mcp was not found within 30 s: $($servers | ConvertTo-Json -Depth 5 -Compress)" }
  $mcpHeaders = $auth + @{ "X-Desk-Session" = $script:session }
  $null = Post "/servers/smoke-echo/mcp" @{ jsonrpc = "2.0"; id = 1; method = "initialize"; params = @{ protocolVersion = "2025-06-18"; capabilities = @{}; clientInfo = @{ name = "smoke"; version = "1" } } } $mcpHeaders
  $echoed = Post "/servers/smoke-echo/mcp" @{ jsonrpc = "2.0"; id = 2; method = "tools/call"; params = @{ name = "echo"; arguments = @{ text = "through rigdesk" } } } $mcpHeaders
  if ($echoed.result.content[0].text -ne "echo: through rigdesk") { Fail "the wrapped server's tool did not answer through the API: $($echoed | ConvertTo-Json -Depth 8 -Compress)" }
  Stop-Process -Id $wrapper.Id -Force -ErrorAction SilentlyContinue
  Write-Host "mcp: $($servers.servers | ConvertTo-Json -Compress), called through the API: $($echoed.result.content[0].text)"

  $null = Post "/debug/viewer/human" @{}
  $held = Tool "type" @{ text = "x" }
  if (-not $held.isError -or (Text $held) -notmatch "is using the PC") { Fail "an action ran while the human held input: $(Text $held)" }
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
