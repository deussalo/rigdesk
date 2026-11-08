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
- **Locked settings.** Server, network, proxy and security settings are hidden. The permanent password can be changed
  only by running the install line again, and the admin settings PIN guards the rest.
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
- **Hand to agent / Take back.** Any mouse or keyboard input, from a viewer or at the PC itself, takes input back
  before the agent's next action. While the human holds input, the agent's actions are refused ("the human took over
  at …") but its reads still work.
- **Block agent** (big, red) refuses every call, reads included, until Unblock.
- **Message** opens a box with a still of the current screen. Clip a region, mark it up with pen, arrow, box or
  text, and send. The agent gets a Rig message naming the image, which it reads with `desk/attachment`.
- The agent's pointer shows as a green dot labelled with its name and last action, and an action strip lists its
  recent calls.

On Android (the arm64 APK, which installs as RigDesk beside RustDesk) the agent badge at the top left opens these
controls as a sheet, set up per PC as on the desktop; the red Stop stays on screen while an agent holds the share,
and a touch on the remote image takes input back.

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
| `GET /health` | boot id and current standing |

Tools: `screenshot`, `snapshot` (UI Automation tree with `@x,y` centres), `click`, `type`, `scroll`, `move`, `drag`,
`key`, `shortcut`, `wait`, `app` (launch/switch), `powershell` (as the signed-in user, never SYSTEM) and `clipboard`.
Input goes through the same injection path as viewer input, so viewers see it live.

## Building

GitHub Actions (`.github/workflows/windows.yml`) builds the Windows x64 package. It then runs the binary grep, an
agent API smoke test against Notepad, the installer smoke test and the unit tests. The pure agent logic lives in
`libs/agent` (`cargo test -p rigdesk-agent`), and runs and tests on any OS.

`.github/workflows/android.yml` builds the Android arm64 APK (artifact `rigdesk-android-arm64`, debug-signed), with
the same grep for public hosts and keys.

A tag `v*` publishes a release: the zip, `install.ps1` pinned to the tag and the zip's SHA-256, `SHA256SUMS`, and
notes carrying the install line. Taking a new RustDesk release is described in [docs/UPSTREAM.md](docs/UPSTREAM.md).
