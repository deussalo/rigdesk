# Scratch proof for rig/rigdesk/desktop-honest-notice, not for merge: on the runner's real desktop, after the install
# smoke, each way an agent gets in from Rig shows its toast and tray line, Disconnect in the tray ends the session, and
# a Rig knock on this RigDesk desktop is refused before its person sees any prompt. Screenshots go to notice-shots.
param([string]$Bridge = ".\target\release\rigdesk-bridge.exe")
$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Windows.Forms, System.Drawing, UIAutomationClient, UIAutomationTypes
Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class Win32Menu {
  [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern IntPtr FindWindow(string cls, string title);
  [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr h);
  public static bool Open() { var h = FindWindow("#32768", null); return h != IntPtr.Zero && IsWindowVisible(h); }
}
public static class Keys {
  [DllImport("user32.dll")] static extern void keybd_event(byte vk, byte scan, uint flags, UIntPtr extra);
  // Pressed and let go, with no wait for whoever has the focus to take it.
  public static void Press(byte vk) { keybd_event(vk, 0, 0, UIntPtr.Zero); System.Threading.Thread.Sleep(60); keybd_event(vk, 0, 2, UIntPtr.Zero); }
}
public static class Mouse {
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint flags, uint dx, uint dy, uint data, UIntPtr extra);
  public static void Click(int x, int y, bool right) {
    SetCursorPos(x, y); System.Threading.Thread.Sleep(150);
    mouse_event(right ? 0x08u : 0x02u, 0, 0, 0, UIntPtr.Zero); System.Threading.Thread.Sleep(80);
    mouse_event(right ? 0x10u : 0x04u, 0, 0, 0, UIntPtr.Zero);
  }
}
public static class Tray {
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
  [StructLayout(LayoutKind.Sequential)] public struct NII { public uint cbSize; public IntPtr hWnd; public uint uID; public Guid guidItem; }
  [DllImport("shell32.dll")] static extern int Shell_NotifyIconGetRect(ref NII id, out RECT r);
  [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern IntPtr FindWindowEx(IntPtr parent, IntPtr after, string cls, string title);
  [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, uint msg, IntPtr w, IntPtr l);
  public static IntPtr Window; public static uint Id; public static string Said = "";
  // tray-icon's own window (class tray_icon_app) and the uID it registered, from the shell; Said tells what was found.
  public static int[] Find() {
    Said = ""; IntPtr after = IntPtr.Zero;
    while ((after = FindWindowEx(IntPtr.Zero, after, "tray_icon_app", null)) != IntPtr.Zero) {
      uint pid; GetWindowThreadProcessId(after, out pid);
      Said += " window " + after.ToInt64() + " of pid " + pid + ":";
      for (uint id = 0; id < 256; id++) {
        var nii = new NII { cbSize = (uint)Marshal.SizeOf(typeof(NII)), hWnd = after, uID = id };
        RECT r; int hr = Shell_NotifyIconGetRect(ref nii, out r);
        if (hr == 0) { Window = after; Id = id; Said += " uID " + id + " at " + r.Left + "," + r.Top + "-" + r.Right + "," + r.Bottom; return new int[] { (r.Left + r.Right) / 2, (r.Top + r.Bottom) / 2 }; }
        if (id == 0) Said += " hr " + hr.ToString("x");
      }
    }
    if (Said == "") Said = " no tray_icon_app window";
    return null;
  }
  [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern uint RegisterWindowMessage(string s);
  // The shell's "TaskbarCreated", to every tray_icon_app window: tray-icon registers its icon again on it.
  public static int Reregister() {
    uint created = RegisterWindowMessage("TaskbarCreated"); int n = 0; IntPtr after = IntPtr.Zero;
    while ((after = FindWindowEx(IntPtr.Zero, after, "tray_icon_app", null)) != IntPtr.Zero) { PostMessage(after, created, IntPtr.Zero, IntPtr.Zero); n++; }
    return n;
  }
  // What a right press on the icon sends the window: tray-icon then shows its menu at the cursor.
  public static void RightPress() { PostMessage(Window, 6002, new IntPtr(Id), new IntPtr(0x204)); }
}
"@
$out = Join-Path $env:RUNNER_TEMP "notice-shots"
New-Item -ItemType Directory -Force $out | Out-Null
$failed = @()
function Say($line) { Write-Host "$(Get-Date -Format HH:mm:ss.fff) $line" }
function SaveLogs {
foreach ($dir in @("$env:APPDATA\RigDesk\log", "C:\Windows\ServiceProfiles\LocalService\AppData\Roaming\RigDesk\log", "C:\Windows\System32\config\systemprofile\AppData\Roaming\RigDesk\log")) {
  Get-ChildItem -Path $dir -Recurse -Filter *.log -ErrorAction SilentlyContinue | ForEach-Object {
    Add-Content (Join-Path $out "rigdesk-logs.txt") "--- $($_.FullName)"; Get-Content $_.FullName -Tail 120 | Add-Content (Join-Path $out "rigdesk-logs.txt")
  }
}
}
trap { Say "::error::notice shots: uncaught: $_ at $($_.InvocationInfo.PositionMessage)"; SaveLogs; exit 1 }
function Miss($why) { Write-Host "::error::notice shots: $why"; $script:failed += $why }
function Shot($name) {
  $screen = [System.Windows.Forms.SystemInformation]::VirtualScreen
  $bitmap = New-Object System.Drawing.Bitmap $screen.Width, $screen.Height
  $g = [System.Drawing.Graphics]::FromImage($bitmap)
  $g.CopyFromScreen($screen.Left, $screen.Top, 0, 0, $bitmap.Size)
  $bitmap.Save((Join-Path $out "$name.png"), [System.Drawing.Imaging.ImageFormat]::Png)
  $g.Dispose(); $bitmap.Dispose()
  Say "shot: $name.png"
}
$A = [System.Windows.Automation.AutomationElement]
$Tree = [System.Windows.Automation.TreeScope]
function ByType($type) { New-Object System.Windows.Automation.PropertyCondition($A::ControlTypeProperty, $type) }
function Centre($element) { $r = $element.Current.BoundingRectangle; @([int]($r.X + $r.Width / 2), [int]($r.Y + $r.Height / 2)) }
function TrayIcon {
  foreach ($i in 1..10) {
    $c = [Tray]::Find()
    if ($i -eq 1 -or $c) { Say "tray:$([Tray]::Said)" }
    if ($c) { return $c }
    if ($i -eq 1) { Say "tray: the icon is not registered with the shell; TaskbarCreated to $([Tray]::Reregister()) tray window(s)" }
    Start-Sleep 1
  }
  $null
}
function OpenTrayMenu($c) {
  [Mouse]::SetCursorPos($c[0], $c[1]) | Out-Null; Start-Sleep -Milliseconds 300
  [Mouse]::Click($c[0], $c[1], $true); Start-Sleep 1
  if (-not (MenuShown)) { Say "tray: the right click showed no menu; posting the icon's own right press"; [Tray]::RightPress(); Start-Sleep 1 }
  MenuShown
}
function MenuShown { [Win32Menu]::Open() }
function MenuItem($name) {
  foreach ($i in 1..10) {
    $item = $A::RootElement.FindAll($Tree::Descendants, (ByType ([System.Windows.Automation.ControlType]::MenuItem))) |
      Where-Object { $_.Current.Name -eq $name } | Select-Object -First 1
    if ($item) { return $item }
    Start-Sleep -Milliseconds 300
  }
  $null
}


# The tray started at install, before this session's shell took its icon; a restarted Explorer broadcasts TaskbarCreated,
# on which tray-icon registers it again, and with EnableAutoTray off it shows on the taskbar, not behind the chevron.
Say "restarting Explorer"
Set-ItemProperty -Path HKCU:\Software\Microsoft\Windows\CurrentVersion\Explorer -Name EnableAutoTray -Value 0
Stop-Process -Name explorer -Force -ErrorAction SilentlyContinue
foreach ($i in 1..20) { Start-Sleep 1; if (Get-Process explorer -ErrorAction SilentlyContinue) { break } }
if (-not (Get-Process explorer -ErrorAction SilentlyContinue)) { Start-Process explorer }
Start-Sleep 10
Shot "0-taskbar"
$svcHome = "C:\Windows\ServiceProfiles\LocalService\AppData\Roaming\RigDesk"
$token = (Get-Content -Raw (Join-Path $svcHome "config\agent\api.token")).Trim()
Say "::add-mask::$token"
$auth = @{ Authorization = "Bearer $token" }
function Post($url, $body, $headers = $auth) {
  try { Invoke-RestMethod -Method Post -Uri $url -Headers $headers -ContentType "application/json" -Body ($body | ConvertTo-Json -Depth 8 -Compress) }
  catch { if ($_.ErrorDetails.Message) { $_.ErrorDetails.Message | ConvertFrom-Json } else { throw } }
}
function Owner { (Invoke-RestMethod -Uri "http://127.0.0.1:21119/health" -Headers $auth).standing.owner }

$ways = [ordered]@{
  accept         = "Builder (Rig agent) joined: your Rig Accept, let in by this PC's -AgentJoin"
  standing_grant = "Builder (Rig agent) joined: Always allow in Rig, let in by -AgentJoin"
  person         = "Builder (Rig agent) joined: your Allow"
}
$first = $true
foreach ($how in $ways.Keys) {
  if (-not $first) { Start-Sleep 25 }  # Windows shows one toast at a time, and these stay up for about 25 s
  $first = $false
  Say "join how=$how"
  $joined = Post "http://127.0.0.1:21119/join" @{ agent = @{ id = "notice-$how"; title = "Builder"; graph = "ci" }; ask = "shots-$how"; how = $how }
  if (-not $joined.session) { Miss "/join how=$how started no session: $($joined | ConvertTo-Json -Compress)"; continue }
  Start-Sleep 4
  Shot "1-$how-toast"
  Say "finding the tray icon"
  $c = TrayIcon
  if (-not $c) { Miss "no RigDesk tray icon on the taskbar"; Post "http://127.0.0.1:21119/leave" @{} ($auth + @{ "X-Desk-Session" = $joined.session }) | Out-Null; continue }
  Say "opening the tray menu at $($c -join ',')"
  if (-not (OpenTrayMenu $c)) { Miss "the RigDesk tray menu did not open" }
  Shot "2-$how-tray"
  Say "looking for Disconnect Builder"
  $disconnect = MenuItem "Disconnect Builder"
  if ($disconnect) { $d = Centre $disconnect; [Mouse]::Click($d[0], $d[1], $false) }
  else { Say "tray: UI Automation sees no menu items; Disconnect is the menu's last entry, so Up then Enter"; [Keys]::Press(0x26); Start-Sleep -Milliseconds 300; Shot "2-$how-tray-disconnect-selected"; [Keys]::Press(0x0D) }
  $ended = $false
  foreach ($i in 1..10) { if ($null -eq (Owner)) { $ended = $true; break }; Start-Sleep 1 }
  if (-not $ended) { Miss "Disconnect Builder did not end the $how session: owner $((Owner) | ConvertTo-Json -Compress)"; [Keys]::Press(0x1B); Post "http://127.0.0.1:21119/leave" @{} ($auth + @{ "X-Desk-Session" = $joined.session }) | Out-Null }
  Shot "3-$how-disconnected"
}
$ends = @((Invoke-RestMethod -Uri "http://127.0.0.1:21119/log?n=400" -Headers $auth).entries | Where-Object { $_.what -eq "join" -and $_.detail.ask -like "shots-*" } | ForEach-Object { "$($_.detail.ask)=$($_.detail.by)" })
Say "journal joins: $($ends -join ' | ')"

# B: rigdesk-bridge reaches this RigDesk desktop as a stock PC with no saved password, and knocks for an agent's
# request. The desktop refuses the knock in its own words; nobody here is shown a prompt with only Dismiss.
Say "B: the knock"
$btoken = "shots-" + [guid]::NewGuid().ToString("N")
$secrets = Join-Path $env:RUNNER_TEMP "shots-secrets"
New-Item -ItemType Directory -Force $secrets | Out-Null
[IO.File]::WriteAllText((Join-Path $secrets "token"), $btoken, (New-Object System.Text.UTF8Encoding $false))
# RigDesk takes direct connections from the tailnet only, as the Rig host's bridge dials it; the runner has no tailnet,
# so it gets a tailnet address of its own, never used as a source for anything else, and the bridge dials that.
$adapter = Get-NetIPConfiguration | Where-Object { $_.IPv4DefaultGateway } | Select-Object -First 1
Say "B: 100.64.0.1 on $($adapter.InterfaceAlias) ($($adapter.InterfaceIndex))"
New-NetIPAddress -InterfaceIndex $adapter.InterfaceIndex -IPAddress 100.64.0.1 -PrefixLength 32 | Out-Null
Start-Sleep 3
$bridgeRun = Start-Process $Bridge -PassThru -WindowStyle Hidden -RedirectStandardError (Join-Path $out "bridge.err.log") -RedirectStandardOutput (Join-Path $out "bridge.out.log") `
  -ArgumentList @("--peer", "100.64.0.1", "--token-file", (Join-Path $secrets "token"), "--port", "21130")
try {
  $bauth = @{ Authorization = "Bearer $btoken" }
  $up = $false
  foreach ($i in 1..30) { try { $null = Invoke-RestMethod -Uri "http://127.0.0.1:21130/health" -Headers $bauth; $up = $true; break } catch { Start-Sleep 1 } }
  if (-not $up) { Miss "rigdesk-bridge did not answer within 30 s" } else {
    $cms = @(Get-CimInstance Win32_Process | Where-Object { $_.CommandLine -match "--cm" }).Count
    $offered = Post "http://127.0.0.1:21130/offer" @{ agent = @{ id = "knock-b"; title = "Builder"; graph = "ci" }; ask = "shots-b"; nonce = "0123456789abcdef0123" } $bauth
    Say "offer: $($offered | ConvertTo-Json -Compress)"
    $answer = $null
    foreach ($i in 1..30) {
      Start-Sleep 1
      $request = [System.Net.HttpWebRequest]::Create("http://127.0.0.1:21130/events?last-event-id=0")
      $request.Headers.Add("Authorization", "Bearer $btoken"); $request.Timeout = 3000; $request.ReadWriteTimeout = 3000
      try {
        $reader = New-Object IO.StreamReader($request.GetResponse().GetResponseStream())
        while ($null -ne ($l = $reader.ReadLine())) { if ($l -match '"answer"') { $answer = $l; break } }
      } catch {} finally { if ($reader) { $reader.Dispose() } }
      if ($answer) { break }
    }
    Shot "4-b-knock-refused"
    Say "offer event: $answer"
    if ($answer -notmatch "refused: This PC runs RigDesk") { Miss "the desktop did not refuse the knock in its words: $answer" }
    $cmsAfter = @(Get-CimInstance Win32_Process | Where-Object { $_.CommandLine -match "--cm" }).Count
    if ($cmsAfter -gt $cms) { Miss "a connection manager opened for the knock ($cms before, $cmsAfter after)" }
  }
} finally {
  if ($bridgeRun) { Stop-Process -Id $bridgeRun.Id -Force -ErrorAction SilentlyContinue }
  Remove-NetIPAddress -IPAddress 100.64.0.1 -Confirm:$false -ErrorAction SilentlyContinue
}

SaveLogs
if ($failed.Count) { exit 1 }
Say "notice shots passed"
