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
install line set on that PC (`chmod 600`). With `for_fleet` off, only graphs and agents whose
`mcp_servers` name `desk` reach the tools.

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
back, handing it over, block and unblock, the session ending, and messages typed in the viewer with
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
