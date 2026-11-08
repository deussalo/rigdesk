# rigdesk — the Rig plugin for RigDesk PCs

A thin bridge between Rig agents and RigDesk PCs. RigDesk is AGPL-3.0, source at
https://github.com/deussalo/rigdesk; this plugin talks to it over HTTP only.

## Install

    rig plugin add <rigdesk checkout>/rig-plugin/rigdesk

Then name each PC in Rig's settings (`plugins.rigdesk`):

    {"pcs": {"deus": {"url": "https://deus.<tailnet>.ts.net",
                      "token_file": "~/.rig/rigdesk/deus.token",
                      "client": "acme"}},
     "for_fleet": true}

`url` is the PC's agent API as `tailscale serve` publishes it; `token_file` holds the agent-API token the
install line set on that PC (`chmod 600`). `POST /x/rigdesk/token {"pc": "deus", "token": "..."}` keeps a token
in such a file (`~/.rig/rigdesk/<pc>.token`) and answers its path. With `for_fleet` off, only graphs and agents whose
`mcp_servers` name `desk` reach the tools.

## Stock RustDesk PCs

A PC running unmodified RustDesk (1.4.0 or later) can be handed to an agent too, with no RigDesk on it. Rig runs
`rigdesk-remote` for it on this host: a headless RustDesk client that logs in to the PC by direct IP over the tailnet
with the PC's permanent password and serves the same agent API on loopback.

On the PC, in RustDesk's settings:

* Security → **Enable direct IP access** (port 21118), and **Use permanent password** with a password set;
* the PC on your tailnet (Tailscale installed and signed in). No public RustDesk server is used.

On the Rig host, put `rigdesk-remote` on `PATH` (or name it as `remote_bin`): CI builds it as the
`rigdesk-remote-linux-x64` artifact of the `stock` workflow. Keep the PC's password in a 0600 file, and a token of
your choosing for the local API in another (`POST /x/rigdesk/token` keeps one), then name the PC:

    {"pcs": {"front": {"url": "http://127.0.0.1:21120",
                       "token_file": "~/.rig/rigdesk/front.token",
                       "client": "acme",
                       "stock": {"peer": "front.<tailnet>.ts.net",
                                 "password_file": "~/.rig/rigdesk/front.password"}}}}

Each stock PC needs its own loopback port. Rig starts `rigdesk-remote` with the plugin, restarts it if it exits, and
writes its output to `remote-<pc>.log` in the plugin's state directory. It connects to the PC when the agent first
acts and leaves after ten idle minutes, so the PC shows a RustDesk session only while the agent works.

The **RigDesk** tab stands in for the viewer: **share with agent** mints a one-use link (copied to the clipboard)
to give an agent for `desk/join`, and **hand to agent**, **stop**, **block**, **unblock** and **end share**
act as the viewer's buttons do.

### The RigDesk GUI's agent panel

The RigDesk viewer (Linux build: the `rigdesk-linux-x64` artifact of the `linux` workflow) connects straight to the
stock PC as ordinary RustDesk does, so the human watches the agent work live. Its agent panel talks HTTP to the PC's
`rigdesk-remote` on the Rig host, not to the PC: in a remote tab, **Set up agent host** takes that PC's `url`, its
token and the PC's name in Rig, kept per peer ID on that machine. The panel then shares, hands input over, blocks,
ends the share, shows the owning agent and its actions, and sends a note (text plus an annotated still of the screen
from `/viewer/frame`) to the agent. A click or key in the viewer while the agent holds input takes it back.

**Stop** (the panel's button, or Ctrl+Alt+Esc in the viewer) takes input back at once
and interrupts the owning agent's current Rig turn, as the operator's interrupt does; the agent is then told why.

The GUI runs on the Rig host for now, where `127.0.0.1:<port>` reaches the agent host. Running it elsewhere (bridge
mode: a laptop or phone viewer while the agent's session stays on the Rig host) needs the agent host published to
that device, e.g. with `tailscale serve`; that is not set up yet.

Known limits against a RigDesk PC:

* tools: screenshot, move, click, drag, scroll, type, key, shortcut, wait and clipboard only; no UI tree
  (snapshot), app or powershell;
* the human takes input back by moving the PC's mouse (or with **take back**); keyboard use at the PC is not seen;
* direct IP access has no RustDesk-layer encryption; the tailnet's WireGuard is what protects the session;
* screenshots come from the PC's own screenshot feature, so each costs a round trip of a full PNG.

## What an agent gets

* `desk/join(link)` — the human clicks **Share with agent** in the viewer and gives the agent the
  `rigdesk://<pc>/<grant>#<secret>` link. Joining claims the PC's session for that agent alone; a
  link works once.
* `desk/<tool>` — the PC's own tools (screenshot, snapshot, click, type, scroll, move, drag, key,
  shortcut, wait, app, powershell, clipboard), sent to the PC the calling agent joined. Images come
  back as PNG files under the plugin's state directory.
* `desk/attachment(id)` — an image the human sent with a message.
* `desk/leave` — ends the session.

What the human does reaches the owning agent as Rig messages (steered into its turn): taking input
back, Stop (which also interrupts its turn), handing it over, block and unblock, the session ending, and messages typed in the viewer with
their screenshots and clips. Each is keyed by the PC's boot and event id, so a reconnect never
delivers one twice; an image is copied before the PC is told it arrived.

The PC alone decides who may act: when the human holds input the PC refuses actions (reads still
work), and while blocked it refuses every call.

## Pages

* `/x/rigdesk/all` (the **RigDesk** tab): each PC, up or down, who holds input, and the owning agent.
* An agent pin under each owning agent's transcript: its PC, the PC's state and who holds input.
* `rig status` prints a line for each PC that is down.

## Testing off Windows

`cargo run -p rigdesk-agent --example fake_desk -- 39119 <token>` serves the same API over a stand-in
desktop and reads viewer commands (`mint`, `hand`, `human`, `block`, `unblock`, `msg <text>`) from stdin.
Point a PC's `url` at `http://127.0.0.1:39119` on a scratch Rig.
