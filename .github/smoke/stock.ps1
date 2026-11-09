# Stock-PC smoke test (plan S9): unmodified RustDesk runs --server on the runner with direct IP access and a permanent
# password, as an operator would set up a client's PC; rigdesk-bridge logs in to it over 127.0.0.1 and serves the agent
# API. Checks a screenshot, a click and typing into Notepad, the clipboard both ways, a takeover by someone at the PC,
# hand-back, Block, and the action log.
param([Parameter(Mandatory = $true)][string]$Version, [string]$Remote = ".\target\release\rigdesk-bridge.exe")
$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Windows.Forms, System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class Win {
  [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr FindWindow(string cls, string title);
  [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr FindWindowEx(IntPtr parent, IntPtr after, string cls, string title);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr window, out RECT rect);
  [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr window, IntPtr after, int x, int y, int cx, int cy, uint flags);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
}
"@

$out = Join-Path $env:RUNNER_TEMP "stock-smoke"
New-Item -ItemType Directory -Force $out | Out-Null
$token = "stock-" + [guid]::NewGuid().ToString("N")
$password = "Stock-" + [guid]::NewGuid().ToString("N").Substring(0, 12)
Write-Host "::add-mask::$password"
$base = "http://127.0.0.1:21120"
$auth = @{ Authorization = "Bearer $token" }

function Fail($why) {
  Write-Host "::error::stock $Version smoke: $why"
  foreach ($f in @("remote.err.log", "remote.out.log")) {
    $p = Join-Path $out $f
    if (Test-Path $p) { Write-Host "--- $f"; Get-Content $p -Tail 80 | ForEach-Object { Write-Host $_ } }
  }
  Get-ChildItem -Path (Join-Path $env:APPDATA "RustDesk\log") -Recurse -Filter *.log -ErrorAction SilentlyContinue |
    ForEach-Object { Write-Host "--- $($_.FullName)"; Get-Content $_.FullName -Tail 60 | ForEach-Object { Write-Host $_ } }
  exit 1
}
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
function Shot($name) {
  $shot = Tool "screenshot"
  $png = $shot.content | Where-Object { $_.type -eq "image" } | Select-Object -First 1
  if ($shot.isError -or $null -eq $png -or $png.data.Length -lt 100) { Fail "screenshot is empty: $(Text $shot)" }
  [IO.File]::WriteAllBytes((Join-Path $out "$name.png"), [Convert]::FromBase64String($png.data))
  return $shot
}

# 1. The stock PC: its own release, config written as its settings page would, direct IP on, a permanent password.
$stock = Join-Path $env:RUNNER_TEMP "stock-rustdesk"
New-Item -ItemType Directory -Force $stock | Out-Null
$exe = Join-Path $stock "rustdesk.exe"
python .github/ci/fetch_asset.py "https://github.com/rustdesk/rustdesk/releases/download/$Version/rustdesk-$Version-x86_64.exe" $exe
if ($LASTEXITCODE -ne 0) { Fail "could not download RustDesk $Version" }
$config = Join-Path $env:APPDATA "RustDesk\config"
New-Item -ItemType Directory -Force $config | Out-Null
$utf8 = New-Object Text.UTF8Encoding($false)
[IO.File]::WriteAllText((Join-Path $config "RustDesk.toml"), "password = '$password'`nsalt = 'stocksmoke1'`n", $utf8)
[IO.File]::WriteAllText((Join-Path $config "RustDesk2.toml"), "[options]`ndirect-server = 'Y'`nverification-method = 'use-permanent-password'`napprove-mode = 'password'`n", $utf8)

$null = Start-Process notepad -PassThru
$null = Start-Process $exe -ArgumentList "--server" -PassThru
$listening = $false
foreach ($i in 1..90) {
  if (Get-NetTCPConnection -LocalPort 21118 -State Listen -ErrorAction SilentlyContinue) { $listening = $true; break }
  Start-Sleep 1
}
if (-not $listening) { Fail "RustDesk $Version is not listening on the direct-IP port 21118 after 90 s" }
Write-Host "RustDesk $Version listens on 21118"

# 2. rigdesk-bridge, as the Rig host runs it.
$secrets = Join-Path $env:RUNNER_TEMP "stock-secrets"
New-Item -ItemType Directory -Force $secrets | Out-Null
[IO.File]::WriteAllText((Join-Path $secrets "password"), $password, $utf8)
[IO.File]::WriteAllText((Join-Path $secrets "token"), $token, $utf8)
$remote = Start-Process $Remote -PassThru -WindowStyle Hidden -RedirectStandardError (Join-Path $out "remote.err.log") -RedirectStandardOutput (Join-Path $out "remote.out.log") `
  -ArgumentList @("--peer", "127.0.0.1", "--password-file", (Join-Path $secrets "password"), "--token-file", (Join-Path $secrets "token"),
                  "--port", "21120", "--journal", (Join-Path $out "actions.jsonl"))
try {
  $up = $false
  foreach ($i in 1..30) {
    try { $health = Invoke-RestMethod -Uri "$base/health" -Headers $auth; $up = $true; break } catch { Start-Sleep 1 }
  }
  if (-not $up) { Fail "rigdesk-bridge did not answer on $base within 30 s" }
  Write-Host "health: $($health | ConvertTo-Json -Compress)"

  $minted = Post "/viewer/mint" @{}
  $claim = Post "/claim" @{ grant = $minted.grant; secret = $minted.secret; agent = @{ id = "smoke"; title = "Smoke"; graph = "ci" } }
  $script:session = $claim.session
  if ($claim.standing.holder -ne "agent") { Fail "after claim the holder is $($claim.standing.holder), not agent" }

  $shot = Shot "first"
  Write-Host "screenshot: $(Text $shot)"
  if ((Text $shot) -notmatch "display (\d+) of \d+, (\d+)x(\d+) at (-?\d+),(-?\d+)") { Fail "screenshot did not name its display: $(Text $shot)" }
  $ox = [int]$Matches[4]; $oy = [int]$Matches[5]

  # Where Notepad's text area is, found by the test itself: a stock PC has no UI tree for the agent.
  $window = [IntPtr]::Zero
  foreach ($i in 1..20) { $window = [Win]::FindWindow("Notepad", [NullString]::Value); if ($window -ne [IntPtr]::Zero) { break }; Start-Sleep -Milliseconds 500 }
  if ($window -eq [IntPtr]::Zero) { Fail "the test could not find Notepad's window" }
  # Topmost, so no console window the runner opens covers it.
  $null = [Win]::SetWindowPos($window, [IntPtr](-1), 200, 150, 600, 400, 0x40)
  Start-Sleep -Milliseconds 500
  $edit = [Win]::FindWindowEx($window, [IntPtr]::Zero, "Edit", [NullString]::Value)
  if ($edit -eq [IntPtr]::Zero) { Fail "the test could not find Notepad's text area" }
  $r = New-Object Win+RECT
  $null = [Win]::GetWindowRect($edit, [ref]$r)
  $x = [int](($r.Left + $r.Right) / 2) - $ox; $y = [int](($r.Top + $r.Bottom) / 2) - $oy
  Write-Host "Notepad's text area centre: $x,$y in screenshot pixels"

  $clicked = Tool "click" @{ x = $x; y = $y }
  if ($clicked.isError) { Fail "click refused: $(Text $clicked)" }
  $null = Tool "wait" @{ ms = 500 }
  $words = "stock rustdesk $Version $([guid]::NewGuid().ToString('N').Substring(0, 8))"
  $typed = Tool "type" @{ text = $words }
  if ($typed.isError) { Fail "type refused: $(Text $typed)" }
  $null = Tool "wait" @{ ms = 500 }
  $null = Tool "shortcut" @{ keys = "ctrl+a" }
  $null = Tool "shortcut" @{ keys = "ctrl+c" }
  $null = Tool "wait" @{ ms = 1000 }
  $null = Shot "typed"
  $local = (Get-Clipboard -Raw)
  if ("$local".Trim() -ne $words) { Fail "the PC's clipboard holds '$local', not the typed '$words'" }
  Write-Host "typed into Notepad and copied on the PC: $local"
  $seen = ""
  foreach ($i in 1..10) {
    $got = Tool "clipboard"
    if (-not $got.isError) { $seen = (Text $got).Trim(); if ($seen -eq $words) { break } }
    Start-Sleep -Milliseconds 500
  }
  if ($seen -ne $words) { Fail "the clipboard tool read '$seen', not '$words'" }
  $set = "set by the agent $([guid]::NewGuid().ToString('N').Substring(0, 6))"
  $null = Tool "clipboard" @{ text = $set }
  Start-Sleep 1
  if ("$(Get-Clipboard -Raw)".Trim() -ne $set) { Fail "setting the clipboard did not reach the PC: '$(Get-Clipboard -Raw)'" }
  Write-Host "clipboard both ways"

  # Someone at the PC moves the mouse: the agent's next action waits, reads still work.
  Start-Sleep 1
  [System.Windows.Forms.Cursor]::Position = New-Object System.Drawing.Point(($ox + 40), ($oy + 40))
  Start-Sleep 1
  [System.Windows.Forms.Cursor]::Position = New-Object System.Drawing.Point(($ox + 90), ($oy + 70))
  Start-Sleep 1
  $held = Tool "type" @{ text = "x" }
  if (-not $held.isError -or (Text $held) -notmatch "took over") { Fail "an action ran after someone at the PC moved the mouse: $(Text $held)" }
  $null = Shot "while-human-holds"
  Write-Host "takeover: $(Text $held)"

  $null = Post "/viewer/hand" @{}
  $moved = Tool "move" @{ x = 10; y = 10 }
  if ($moved.isError) { Fail "after hand-back the agent still cannot act: $(Text $moved)" }

  $null = Post "/viewer/block" @{}
  $blocked = Tool "screenshot"
  if (-not $blocked.isError -or (Text $blocked) -notmatch "blocked") { Fail "a read ran while blocked: $(Text $blocked)" }
  $null = Post "/viewer/unblock" @{}

  $log = Invoke-RestMethod -Uri "$base/log?n=50" -Headers $auth
  $what = ($log.entries | ForEach-Object { $_.what }) -join ","
  foreach ($w in @("claim", "click", "type", "block")) { if ($what -notmatch $w) { Fail "the action log lacks $w`: $what" } }
  if ((Get-Content -Raw (Join-Path $out "remote.err.log")) -match [regex]::Escape($password)) { Fail "the password reached rigdesk-bridge's log" }
  Write-Host "stock $Version smoke passed"
} finally {
  if ($null -ne $remote -and $null -ne $remote.Id) { Stop-Process -Id $remote.Id -Force -ErrorAction SilentlyContinue }
  Get-Process rustdesk, notepad -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
}
