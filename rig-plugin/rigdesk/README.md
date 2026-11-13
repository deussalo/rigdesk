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
`rigdesk-bridge` for it on this host: a headless RustDesk client that logs in to the PC by direct IP over the tailnet
with the PC's permanent password and serves the same agent API on loopback.

On the PC, in RustDesk's settings:

* Security → **Enable direct IP access** (port 21118), and **Use permanent password** with a password set;
* the PC on your tailnet (Tailscale installed and signed in). No public RustDesk server is used.

On the Rig host, put `rigdesk-bridge` on `PATH` (or name it as `bridge_bin`): CI builds it as the
`rigdesk-bridge-linux-x64` artifact of the `stock` workflow. Keep the PC's password in a 0600 file, and a token of
your choosing for the local API in another (`POST /x/rigdesk/token` keeps one), then name the PC:

    {"pcs": {"front": {"url": "http://127.0.0.1:21120",
                       "token_file": "~/.rig/rigdesk/front.token",
                       "client": "acme",
                       "stock": {"peer": "front.<tailnet>.ts.net",
                                 "password_file": "~/.rig/rigdesk/front.password"}}}}

Each stock PC needs its own loopback port. Rig starts `rigdesk-bridge` with the plugin, restarts it if it exits, and
writes its output to `bridge-<pc>.log` in the plugin's state directory. It connects to the PC when the agent first
acts and leaves after ten idle minutes, so the PC shows a RustDesk session only while the agent works.

The **RigDesk** tab stands in for the viewer: **share with agent** mints a one-use link (copied to the clipboard)
to give an agent for `desk/join`, and **hand to agent**, **stop**, **block**, **unblock** and **end share**
act as the viewer's buttons do.

### Three parts: the PC, the bridge, the GUI

* **The PC** runs stock RustDesk, as above.
* **The bridge** (`rigdesk-bridge`) holds the one RustDesk session to the PC. It serves the agent its tools and, with a
  viewer port, relays the same session to people watching. It is headless; Rig runs it beside itself for each stock
  PC, and it can as well run on another machine, which Rig then reaches by its tailnet URL.
* **The GUI** (the RigDesk app: Linux build `rigdesk-linux-x64` of the `linux` workflow; Android to come) dials the
  bridge, never the PC, so it needs neither a route to the PC nor the PC's password.

To let GUIs attach, give the stock PC a viewer port and a password file of its own, then publish both of the bridge's
loopback ports on the tailnet from the bridge's machine:

    "stock": {"peer": "front.<tailnet>.ts.net", "password_file": "~/.rig/rigdesk/front.password",
              "viewer_port": 21128, "viewer_password_file": "~/.rig/rigdesk/front.viewer"}

    tailscale serve --bg --tcp 21128 tcp://127.0.0.1:21128      # the picture, for the GUI's connection
    tailscale serve --bg --https 8460 http://127.0.0.1:21120    # the agent API, for the GUI's agent panel

In the GUI, connect to `<bridge machine>.<tailnet>.ts.net:21128` with the viewer password. In the remote tab,
**Set up agent host** takes `https://<bridge machine>.<tailnet>.ts.net:8460`, the PC's token and its name in Rig, kept
per peer on that device. The panel shares the desktop with an agent, hands input over, blocks, ends the share, shows
the owning agent and its actions, and sends a note (text plus an annotated still from `/viewer/frame`) to the agent.

Every click, wheel turn and key from a GUI passes the bridge: while the agent holds input, the first one takes it
back before it reaches the PC, and the pointer merely crossing the picture is kept from the PC, so watching never
disturbs the agent. **Stop** (the panel's button, or Ctrl+Alt+Esc in the GUI) takes input back at once and interrupts
the owning agent's current Rig turn, as the operator's interrupt does; the agent is then told why. While a GUI watches,
the bridge asks the PC for a watchable picture (balanced quality, 30 fps), and for a lean one (5 fps) otherwise.

### By RustDesk ID, through our own ID server (no Tailscale on the PC)

A PC that is not on the tailnet is named by its RustDesk ID instead of a `peer`. It registers with our own ID server
(`hbbs`) and the bridge meets it on our relay (`hbbr`); no public RustDesk server is used. hbbs vouches for the PC's
key, so the session is encrypted end to end by RustDesk's key exchange, and a PC whose signed identity does not verify
is refused, never used in the clear.

On the Rig host, run hbbs and hbbr (rustdesk-server 1.1.16 or later) with the key required, hbbs naming the relay by
the address PCs reach it at, and open TCP 21115-21117 and UDP 21116 to the internet:

    hbbs -r <public address>:21117 -k _        # writes id_ed25519 and id_ed25519.pub in its working directory
    hbbr -k _                                  # same working directory, so the same key

Name the server once in Rig's settings; its address lives there and nowhere in this repo:

    {"id_server": {"host": "<public address>", "key_file": "~/.rig/rustdesk-server/id_ed25519.pub"}}

The **RigDesk** tab then shows the server's health (hbbs and hbbr, each probed with a TCP connection every 30 s, also in
`rig status`) and **add a PC**:

1. It gives one line for an administrator PowerShell on a Windows PC. The line fetches `install/stock-rustdesk.ps1` at a
   pinned commit, checks its SHA-256, and runs it with this server's config string: it installs stock RustDesk (pinned,
   checked), points it at the server, sets a fresh permanent password and prints the PC's ID and that password.
2. Enter a name, the ID and the password in the form. `POST /x/rigdesk/pc` keeps the password and a fresh bridge token in
   0600 files under `~/.rig/rigdesk/`, picks a free loopback port, and answers the PC's settings entry, which the page
   writes under `plugins.rigdesk.pcs`; Rig starts its rigdesk-bridge within seconds. The password never enters settings.

A PC on another OS: in RustDesk → Settings → Network → ID/Relay server, **ID server** `<public address>` and **Key**
the contents of `id_ed25519.pub`, Relay and API empty; under Security, **Use permanent password**. Then add it with the
form as above. Settings written by hand take the same shape:

    "pcs": {"front": {"url": "http://127.0.0.1:21122", "token_file": "~/.rig/rigdesk/front.token", "client": "acme",
                      "stock": {"id": "123456789", "password_file": "~/.rig/rigdesk/front.password"}}}

The bridge always takes the relay (it runs beside hbbr, so a hole punch gains nothing). Its log names why a connection
failed: the ID never registered, the PC is offline, or the key differs from hbbs's.

Known limits against a RigDesk PC:

* tools: screenshot, move, click, drag, scroll, type, key, shortcut, wait and clipboard only; no UI tree
  (snapshot), app or powershell;
* someone at the PC itself takes input back by moving its mouse; their keyboard use is not seen (a GUI's is);
* direct IP access has no RustDesk-layer encryption; the tailnet's WireGuard is what protects the session;
* screenshots come from the PC's own screenshot feature, so each costs a round trip of a full PNG.

## What an agent gets

* `desk/join(link)` — the human clicks **Share with agent** in the viewer and gives the agent the
  `rigdesk://<pc>/<grant>#<secret>` link. Joining claims the PC's session for that agent alone; a
  link works once.
* **Standing grant (stock PCs).** rigdesk keeps who holds one, `"agent:<id>"` or `"graph:<name>"` per PC, in its own
  `grants` doc in Rig's store, and alone writes it: on the PCs page (the `pcs` plugin, through the provision
  `rigdesk.assign`) or on the operator's Accept of a `desk/request`. It is not in Rig's settings. Such an agent needs no
  link: its first `desk/<tool>` call joins the PC (the plugin mints and claims on the bridge itself), and so does its
  next call after the bridge or Rig restarts. Stop still blocks it; an agent sharing through a link at the time is not
  displaced. A change applies at once and does not restart the bridge; taking a grant away ends a session held through
  it. A PC removed from settings, or no longer stock, loses its grants. A session held through the grant ends after
  5 minutes without a desk call, so other assigned agents can take the PC; its next call rejoins.
* **A session that ends** (RigDesk restarts, the human ends the share or shares with another agent, an idle leave) is not
  announced to the agent, which is not woken for it; Rig's log has it, and the agent's next `desk/<tool>` call says
  where it stands.
* `desk/pcs` — every PC in Rig, with its client, whether it is up, whether it is assigned to the caller, and who holds
  it now.
* `desk/request(pc, why, kind)` — asks the operator, with Accept/Deny, about a stock PC: `join` (the default) joins
  the caller to it now for one session, as a share link would; `assign` gives it a standing grant. Refused at once for
  an unknown PC, a RigDesk PC (its human shares it in the viewer), or one already assigned to the caller.
* `desk/join(pc)` — joins a stock PC assigned to the caller, by name. A desk call with no session joins the caller's
  assigned PC by itself only when it has exactly one; with more it is refused and lists them.
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
