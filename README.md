# RigDesk

RigDesk is a light fork of [RustDesk](https://github.com/rustdesk/rustdesk) 1.5.1. It comes as a prebuilt,
preconfigured Windows desk that runs as a locked-down service on a client's PC and is reached only over our tailnet.
A remote viewer can **share the session with a Rig agent**, which then drives the PC with Windows-MCP-like tools. The
human watching can take input back at any moment, block the agent outright, or send it a message with an annotated
screenshot.

RigDesk is free software under the [AGPL-3.0](LICENCE), and this repository is its complete source. RustDesk is a
trademark of its owners; RigDesk is not affiliated with them, and their names and icons are replaced. The upstream
README is kept at [docs/README-upstream.md](docs/README-upstream.md).

## How it is wired

- **No servers of ours or RustDesk's.** There is no rendezvous, relay, ID server, API server or updater. The build
  embeds its settings (`res/rigdesk.json`), and CI greps the binary for RustDesk's public hosts and keys.
- **Direct IP over the tailnet.** The viewer dials `<pc>.<tailnet>.ts.net:21118`. The PC accepts connections only
  from Tailscale addresses (100.64.0.0/10 and fd7a:115c:a1e0::/48), and the firewall rule admits nothing else.
- **Agent API.** The user-session server process serves MCP on `127.0.0.1:21119`, published on the tailnet at
  `https://<pc>.<tailnet>.ts.net/` by `tailscale serve`. Every request needs the PC's bearer token, and every tool
  call also needs the session token that `/claim` returns for a share link.
- **Settings.** The Security and Network pages are shown: the permanent password, and the ID and relay servers, can
  be set at the PC as well as by the install line. The admin settings PIN guards them.
- **Trimmed.** English only, Flutter UI only (the Sciter UI, iOS and Linux packaging are gone). Port forwarding,
  the remote terminal and the remote printer are switched off in the embedded settings.

## Enrolling a PC

You need:

- a Tailscale auth key that may apply `tag:pc` and `tag:client-<slug>` (reusable or one-off, preauthorized), with
  HTTPS certificates turned on for the tailnet;
- a permanent password (8-64 characters), an admin PIN (4-32) and an agent API token (32-128 of `A-Za-z0-9_-`) for
  this PC. Generate them, and give the token to Rig through the plugin's token route (see below).

Then run this once in an **administrator** PowerShell on the PC. Each release's notes give the exact line, with
`<tag>` and the SHA-256 of that release's `install.ps1` filled in; `install.ps1` in turn checks the package's SHA-256:

```powershell
[Net.ServicePointManager]::SecurityProtocol='Tls12'; $f="$env:TEMP\rigdesk-install.ps1"; iwr -UseBasicParsing https://github.com/deussalo/rigdesk/releases/download/<tag>/install.ps1 -OutFile $f; if ((Get-FileHash $f -Algorithm SHA256).Hash -ne '<INSTALL_PS1_SHA256>') { throw 'install.ps1 checksum mismatch' }; & ([scriptblock]::Create((Get-Content -Raw $f))) -Client <slug> -TailscaleAuthKey <tskey-auth-...> -Password <password> -Pin <pin> -AgentToken <token>; Remove-Item $f
```

The script ([install/install.ps1](install/install.ps1)):

1. installs Tailscale unattended and joins as `tag:pc,tag:client-<slug>`, named after the computer;
2. downloads the RigDesk package pinned by that release, **checks its SHA-256**, and installs it as the `RigDesk`
   service, which starts at boot;
3. passes the password, PIN and agent token to the service through `enroll.json` in the install folder (readable by
   SYSTEM and Administrators only). The service applies it at startup and deletes it. No secret is printed or
   logged, and the line is scrubbed from PowerShell's history;
4. replaces the installer's inbound firewall rule with one for TCP 21118 from the tailnet only;
5. runs `tailscale serve --bg --https=443 http://127.0.0.1:21119` for the agent API.

Run it again with new values to rotate the secrets. CI runs the same script (minus Tailscale) against every build:
see `.github/smoke/install.ps1`.

`-AgentShell on|off` (on when left out) sets the PC's agent shell: whether Rig's agents may run commands on it with no
desktop session, as SYSTEM. Only rerunning the install line changes it; the person at the PC can pause it from
RigDesk's PIN-locked security settings. Have the client sign the text below before you run the line with it on.

### Consent text

> **Remote management of this computer by Rig**
>
> RigDesk is installed on this computer as a Windows service that starts with it. While its *agent shell* is on,
> automated agents run by **<operator>** through Rig may run commands on this computer at any time, **with the
> highest local rights (SYSTEM)** or as the signed-in user, without a window and without interrupting whoever is at
> the computer. Such a command can read, change or delete anything on this computer. Commands reach it only over a
> private network (Tailscale) from Rig's host, carrying a secret token that Rig keeps readable only by its own user.
>
> **What is run is recorded.** Every command, run or refused, is logged on this computer with the agent that sent it,
> whom it ran as, its exit code and how long it took. The tray icon's *Managed by Rig: commands run here* opens a
> list of the latest 200. *Pause the agent shell*, in RigDesk's security settings behind the admin PIN, stops new
> commands and ends running ones until it is unticked. You see what ran after it ran, not as it runs.
>
> **What is collected.** Within a minute of start-up and every 10 minutes after, RigDesk reads, as SYSTEM: Windows
> edition, build and uptime; processor name and load; memory in use; size and free space of each local drive;
> whether a restart is pending; Windows Update's last install and pending count (checked hourly); Defender's
> real-time protection and signature age; BitLocker status per drive; each physical disk's health, wear and
> temperature; automatic services that are stopped; the count of System-log errors in the last 24 hours; the
> battery where there is one; and RigDesk's own version. It reads no files, documents, browsing or keystrokes. The
> computer keeps the last 24 hours of these readings; Rig fetches them over the same private network and keeps 7 days
> of them on its host, where they are shown to **<operator>** and their agents and checked for faults.
>
> Turning the agent shell off (rerunning the install line with `-AgentShell off`) stops remote commands; uninstalling
> RigDesk stops the readings too.

`-AgentJoin on|off` (on when left out; `agent_join` in a Linux PC's enroll.json) sets whether the operator may start an
agent's desktop session from Rig, with no share link: an agent asks with `desk/request`, the operator accepts in Rig,
and Rig's bearer token asks the PC for the session (`POST /join`). The person at the PC is told and can end it; *Pause
sessions started from Rig*, in the PIN-locked security settings, refuses new ones until it is unticked. With this on,
Rig's agent API token starts desktop sessions on the PC, so it is as strong as a share link from a viewer. Have the
client sign this text too before you run the line with it on.

> **Agent sessions started by the operator**
>
> While this is on, **<operator>** may let an automated agent take this computer's desktop through Rig, without anyone
> connecting to it: the agent sees the screen, moves the pointer, types and runs commands as the signed-in user. The
> operator approves each session in Rig, for one agent at a time, or allows one agent always until they take it back.
>
> **You are told and you can end it.** When such a session starts, a notification names the agent, and while it runs
> the RigDesk tray icon names it with *End the agent's session*. Your own mouse and keyboard always go first: while you
> use the computer, the agent's actions are refused until you pause for 3 seconds. Every action the agent takes is
> logged on this computer.
>
> *Pause sessions started from Rig*, in RigDesk's security settings behind the admin PIN, refuses new sessions until it
> is unticked. Rerunning the install line with `-AgentJoin off` turns them off.

### A Linux PC

Debian or Ubuntu with systemd. You need the same values as for Windows, and a **lane token** too (32-128 of
`A-Za-z0-9_-`, not the agent token): the desktop user can read the agent token, so the agent shell, which runs as
root, takes its own. Give it to Rig through the plugin's `lane_token_file`. Write the secrets into a file only you can
read (`chmod 600`; the line below opens it as you, then hands it to sudo), one `key=value` a line, so no command line or
shell history holds them:

```
tailscale_auth_key=tskey-auth-...
password=...
pin=...
agent_token=...
lane_token=...
```

Then run the line from the release's notes as a user with sudo; it checks `install-linux.sh` against its SHA-256:

```bash
f=$(mktemp) && curl -fsSL -o "$f" https://github.com/deussalo/rigdesk/releases/download/<tag>/install-linux.sh && echo "<INSTALL_LINUX_SHA256>  $f" | sha256sum -c --quiet - && sudo bash "$f" --client <slug> --user <desktop user> < secrets; rm -f "$f"
```

and delete the secrets file. The script ([install/install-linux.sh](install/install-linux.sh)):

1. installs Tailscale and joins as `tag:pc,tag:client-<slug>` (leave out `tailscale_auth_key` where it is already up);
2. downloads the `rigdesk` package pinned by that release, **checks its SHA-256**, and installs it. It replaces a
   stock `rustdesk` package (the two cannot be installed together) and leaves a RustDesk Flatpak alone. Its
   `rigdesk.service` runs as root and starts at boot; the desktop user's session runs RigDesk's server and tray;
3. hands the lane token and the switch to the root service through `/var/lib/rigdesk/enroll.json`, and the password,
   PIN and agent token to the desktop user's server through `~/.config/rigdesk/agent/enroll.json`, each 0600 and
   deleted once applied;
4. lets TCP 21118 in from the tailnet only (nftables, applied again at every start of the service);
5. publishes the agent API at `/` and the agent shell's lane at `/lane` with `tailscale serve`. The lane is a unix
   socket only root can connect to.

`--agent-shell on|off` (on when left out) and `--agent-join on|off` work as `-AgentShell` and `-AgentJoin` do on
Windows; the agent shell runs commands as root, or as the user signed in at the PC's screen in their own session.
Commands as that user run while they wait; background jobs run as root only. Every command is its own systemd unit
(`rigdesk-run-*`, `rigdesk-job-*`), so a job outlives RigDesk's restart and is ended whole. `apt remove rigdesk`
stops it and keeps the enrollment; `apt purge rigdesk` forgets it. CI moves a stock RustDesk install to RigDesk with
the same script against every build: see `.github/smoke/deb.sh`.

Have the client sign this text before you run the line with the agent shell on:

> **Remote management of this computer by Rig**
>
> RigDesk is installed on this computer as a system service that starts with it. While its *agent shell* is on,
> automated agents run by **<operator>** through Rig may run commands on this computer at any time, **as root, with
> the highest local rights,** or as the user signed in at the screen, without a window and without interrupting
> whoever is at the computer. Such a command can read, change or delete anything on this computer. Commands reach it
> only over a private network (Tailscale) from Rig's host, carrying a secret token that Rig keeps readable only by its
> own user.
>
> **What is run is recorded.** Every command, run or refused, is logged on this computer with the agent that sent it,
> whom it ran as, its exit code and how long it took. The tray icon's *Managed by Rig: commands run here* opens a list
> of the latest 200 (`/var/log/rigdesk/managed-by-rig.txt`). *Pause the agent shell*, in RigDesk's security settings
> behind the admin PIN, stops new commands and ends running ones until it is unticked. Blocking an agent in RigDesk
> refuses its commands too. You see what ran after it ran, not as it runs.
>
> **What is collected.** Within a minute of start-up and every 10 minutes after, RigDesk reads, as root: the
> distribution's name and version, the kernel and uptime; processor name and load; memory in use; size and free space
> of each local file system; whether a restart is pending; the packages apt would upgrade, as of the last package-list
> refresh, and when a package was last installed; each physical disk's health, wear and temperature where smartctl is
> installed; services that failed; the count of errors in the system journal in the last 24 hours; the battery where
> there is one; and RigDesk's own version. It reads no files, documents, browsing or keystrokes. The computer keeps
> the last 24 hours of these readings; Rig fetches them over the same private network and keeps 7 days of them on its
> host, where they are shown to **<operator>** and their agents and checked for faults.
>
> Turning the agent shell off (rerunning the install line with `--agent-shell off`) stops remote commands;
> uninstalling RigDesk stops the readings too.

### Tailnet policy

```jsonc
"tagOwners": { "tag:pc": ["autogroup:admin"], "tag:rig": ["autogroup:admin"], "tag:viewer": ["autogroup:admin"] },
"grants": [
  { "src": ["tag:viewer"], "dst": ["tag:pc"], "ip": ["tcp:21118"] },   // the remote viewer
  { "src": ["tag:rig"],    "dst": ["tag:pc"], "ip": ["tcp:443"] }      // Rig's plugin to the agent API
]
```

A PC reaches nobody; viewers and Rig open every connection.

## Sharing a session with an agent

In the remote window, the agent bar (bottom right) offers:

- **Share with agent** copies `rigdesk://<pc>/<grant>#<secret>`. Paste it to any Rig agent, which calls
  `desk/join(link)`. The link works once, and the agent that claims it alone owns the session. The bar then shows
  that agent's name.
- **Your input goes first.** Any mouse or keyboard input, from a viewer or at the PC itself, reaches the PC at once,
  and the agent's actions are refused ("the human is using the PC …", reads still work) until that input pauses for
  3 seconds; then the agent may act again by itself.
- **Stop** (big, red) blocks the agent, reads included, and cuts its turn short. It turns into a green **Resume**,
  which lets the agent act again.
- **Message** opens a box with a still of the current screen. Clip a region, mark it up with pen, arrow, box or
  text, and send. The agent gets a Rig message naming the image, which it reads with `desk/attachment`.
- The agent's pointer shows as a green dot labelled with its name and last action, and an action strip lists its
  recent calls.

On Android (the arm64 APK, which installs as RigDesk beside RustDesk) the phone connects to the bridge's viewer port,
`<bridge host>.<tailnet>.ts.net:21128`, with the bridge's viewer password, and the agent badge at the top left opens
these controls as a sheet. Its bridge settings take the bridge's agent API URL, which must be https (e.g.
`tailscale serve`) since Android blocks cleartext HTTP. The red Stop (green Resume while stopped) stays on screen
while an agent holds the share.

Each claim, holder change, block and tool call is appended to the PC's action log, readable at `GET /log`.

## The Rig plugin

`rig-plugin/rigdesk` bridges Rig and the PCs. Install it with `rig plugin add rig-plugin/rigdesk` and list the PCs
under `plugins.rigdesk.pcs` as `{name: {url, token_file, client}}`. See [rig-plugin/rigdesk/README.md](rig-plugin/rigdesk/README.md).

## Agent API

| Route | What |
|---|---|
| `POST /claim` | `{grant, secret, agent: {id, title, graph}}` → session token (single use) |
| `POST /mcp` | MCP JSON-RPC. `X-Desk-Session` carries the session token |
| `GET /events` | SSE, resumable with `Last-Event-ID`: claimed, holder, revoked, message, dropped, action |
| `GET /attachments/<id>`, `POST /attachments/<id>/ack` | images sent with human messages |
| `POST /leave` | the owning agent ends its session |
| `GET /log?n=N` | the action log |
| `GET /frame?display=N` | a PNG of the screen now, for the bearer's holder (the viewer's note, Rig's screenshot on demand); journaled |
| `POST /viewer/note` | bridge only: `{text, pngs, key?}`, the human's message to the owning agent; a note sent again with a `key` already sent to the same agent answers the first event (`replayed: true`) and is not told twice. Keys are forgotten when the bridge restarts, so a note replayed across a restart is told again |
| `GET /health` | boot id, current standing and the agent shell's switch (`{open, code, words}`, null without one) |
| `POST /join` | `{agent: {id, title, graph}, ask}`: a session the operator accepted in Rig, with no share link; answers `{session, standing}` as `/claim` does; refused with `join_off`, `join_paused`, `held` (another agent holds the session, named) or `blocked`; journaled either way; 404 `no_join` where the host offers none |
| `POST /shell` | `{agent: {id, title, graph}, command, as, timeout_s, background}` or `{agent, job}`: the management lane, no session; refused with `shell_off`, `shell_paused`, `blocked`, `jobs_full`, `no_job` or `bad_request`; journaled either way |
| `GET /samples?after=ID` | the health samples taken after ID (the PC keeps 24 h); a `health` event on `/events` says each new one |

Tools: `screenshot`, `click`, `type`, `scroll`, `move`, `drag`, `key`, `shortcut`, `wait`, `clipboard` and `shell`
(one PowerShell command as the signed-in user, never SYSTEM, with no input and a time limit; it answers the exit code and
the output, without escape codes, cut in the middle past 32 KiB). A stock PC driven through rigdesk-bridge has the same
tools, its `shell` through RustDesk's terminal.
Input goes through the same injection path as viewer input, so viewers see it live.

## Building

GitHub Actions (`.github/workflows/windows.yml`) builds the Windows x64 package. It then runs the binary grep, an
agent API smoke test against Notepad, the installer smoke test and the unit tests. The pure agent logic lives in
`libs/agent` (`cargo test -p rigdesk-agent`), and runs and tests on any OS.

`.github/workflows/android.yml` builds the Android arm64 APK (artifact `rigdesk-android-arm64`, debug-signed), with
the same grep for public hosts and keys.

A tag `v*` publishes a release: the zip, `install.ps1` pinned to the tag and the zip's SHA-256, `SHA256SUMS`, and
notes carrying the install line. Taking a new RustDesk release is described in [docs/UPSTREAM.md](docs/UPSTREAM.md).
