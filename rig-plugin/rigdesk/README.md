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

* Security → **Enable direct IP access** (port 21118), and **Use permanent password** with a password set, for
  unattended access; with no password kept in Rig, an agent joins only by the person's Allow (see `desk/request`);
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

The **RigDesk** tab stands in for the viewer: **share** mints a one-use link (copied to the clipboard)
to give an agent for `desk/join`, and **hand over**, **stop**, **block**, **unblock** and **end share**
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
the owning agent and its actions, and sends a note (text plus an annotated still from `/frame`) to the agent.

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

* `shell` runs in RustDesk's own terminal, which the PC must allow (`enable-terminal`, on by default); on Windows it
  needs a signed-in user, as the user's PowerShell; elsewhere it is the user's shell as RustDesk's user;
* someone at the PC itself takes input back by moving its mouse; their keyboard use is not seen (a GUI's is);
* direct IP access has no RustDesk-layer encryption; the tailnet's WireGuard is what protects the session;
* screenshots come from the PC's own screenshot feature, so each costs a round trip of a full PNG.

## What an agent gets

* `desk/join(link)` — the human clicks **Share with agent** in the viewer, or **Invite a Rig agent** in a RigDesk PC's
  tray, and gives the agent the `rigdesk://<pc>/<grant>#<secret>` link. An invite lapses unclaimed after 15 minutes. Joining claims the PC's session for that agent alone; a
  link works once.
* **Standing grant.** rigdesk keeps who holds one, `"agent:<id>"` or `"graph:<name>"` per PC, in its own
  `grants` doc in Rig's store, and alone writes it: on the RigDesk page, which assigns any agent or every agent in a
  graph, or on the operator's Accept of a `desk/request`. It is not in Rig's settings. Such an agent needs no
  link: `desk/join(pc)` joins the PC (on a stock PC the plugin mints and claims on the bridge itself; a RigDesk PC is asked with `POST /join`). A desk call never joins a PC by
  itself, so a call cannot land on a PC the agent did not name; after its session ends, the agent joins again the same
  way. Stop and Block hold across a rejoin: they are on the agent, not its session, and only the human's unblock lifts
  them. An agent sharing through a link at the time is not
  displaced. A change applies at once and does not restart the bridge; taking a grant away ends a session held through
  it. A PC removed from settings loses its grants. A session held through the grant ends after
  5 minutes without a desk call, so other assigned agents can take the PC.
* **A session that ends** without the agent's own `desk/leave` (RigDesk restarts, the human ends the share or shares with
  another agent, an idle leave, the PC leaves Rig's settings) is told to the agent, queued for its next turn rather
  than waking it; Rig's log has it too. While the PC is down, its MCP servers close for the holder and reopen when it is back.
* `desk/pcs` — every PC in Rig, with its client, whether it is up, whether it is assigned to the caller, and who holds
  it now.
* `desk/request(pc, why, kind)` — asks the operator, with Accept/Deny, about a stock PC: `join` (the default) joins
  the caller to it now for one session, as a share link would; `assign` gives it a standing grant. Refused at once for
  an unknown PC, or one already assigned to the caller.
  On a RigDesk PC, Accept has Rig's bearer ask the PC for the session (`POST /join`), with no share link.
  The PC admits it only while its own switch is on (`-AgentJoin`, and the PIN-locked pause), no other agent holds its
  session and the caller is not blocked there. The person at the PC is notified and can end the session from the tray.
  The PC's switch is read when the agent asks, and a closed one refuses at once with the PC's words, before the operator is asked.
  On a RigDesk PC the operator picks **Join once**, **Always allow** or **Deny**, whichever kind was asked. Join once is
  the Accept above. Always allow also gives the caller a standing grant to that PC, so its later `desk/request` or
  `desk/join(pc)` joins with no ask, through `POST /join` with the ask `standing grant` in the PC's journal. The PC's
  switch and another holder still refuse it. The RigDesk page lists the grant under the PC, and unassigning it there
  ends a session held through it; the agent is told, and its next `desk/request` asks again.
  **The PC asks too.** While a join request waits for the operator, the PC itself shows "<agent> (Rig agent)" in its own
  accept prompt, where Rig is new enough to have plugins answer their own asks (`PluginHost.confirm_answered`). Its
  Accept answers the request as the operator's Accept (Join once) would, and its Dismiss as Deny; the agent is told the
  person at the PC answered. The first answer wins: when the operator answers first, the prompt is taken off the PC
  within 2 s, and an answer either side gives later changes nothing. A prompt the PC lets lapse is shown again, three
  times in all. Either PC is sent `POST /offer {agent, ask, nonce}` over Rig's bearer; `POST /offer/end {ask}` takes
  the prompt away. A RigDesk PC shows the agent in its connection manager. A stock PC's rigdesk-bridge (a phone among
  them, as a notification with Allow) logs in to it with no password, so the PC shows RustDesk's own prompt, and on
  Allow that very connection carries the agent's session: no password is used, and a stock PC needs none kept in Rig
  (leave out `password_file`, and only the person's Allow lets an agent in). An allowed connection no agent claims within
  30 s, as when the operator answered first, is hung up. With no password kept, the agent is told when the allowed
  connection ends (the person disconnects, or ten idle minutes), and its next `desk/request` asks again; with one kept,
  the agent keeps its hold and its next action dials in with the password, as unattended access. How the answer is bound: it counts only when it comes on that PC's own `/events` stream under Rig's bearer,
  names that request and carries the 128-bit nonce Rig made for it, which goes to the PC alone and never to the agent;
  and only while no agent holds a session on that PC, since one could click Accept there. No prompt is shown while one does.
  Known limits:
  * a stock PC whose RustDesk approves by password only shows no Accept, so only the operator can answer;
  * on a stock PC, Accept fully authorizes the login that asked, so the person sees it connected at once, for up to 30 s if no agent then claims it;
  * an agent picks its own title, and the prompt shows it, marked "(Rig agent)": the name says who asks only as far as the title does;
  * a PC that hides its connection manager (`allow-hide-cm`) shows nothing;
  * a request open when Rig restarts is not shown on the PC again;
  * agents' shells run as Rig's own user, so an agent that can run commands on Rig's host can do what Rig can.
* `desk/join(pc)` — joins a PC assigned to the caller, by name. A desk call with no session joins the caller's
  assigned PC by itself only when it has exactly one; with more it is refused and lists them.
* `desk/<tool>` — the PC's own tools (screenshot, click, type, scroll, move, drag, key, shortcut, wait, clipboard
  and shell), the same on every PC, sent to the PC the calling agent joined. Images come back as PNG files under the
  plugin's state directory. `desk/shell(command, timeout_s)` answers `{exit_code, output, cut_bytes}`: output and
  errors as plain text, cut from the middle past 32 KiB; a command over its limit (60 s by default, 600 at most) is
  ended and refused as timed out, with what it printed. The PC's journal keeps each command and its status.
* `desk/attachment(id)` — an image the human sent with a message.
* `<pc>.<server>/<tool>` — the MCP servers already running on a RigDesk PC (Blender's, an IDE's), for the agent
  holding it alone. While an agent holds the PC, its RigDesk finds each MCP server listening on loopback; the plugin
  opens each under the holder's session (initialize, then tools/list) and tells the holder each one's tools by exact
  name, and when one goes. They load on request, for the holder alone: `tool_load` `<pc>.<server>/` lists their tools to
  it, any other agent is shown none, and a call by any but the holder is refused. A call goes through the PC under its holder and Block rules, as a desk call does, and is journaled there;
  a server that forgot its MCP session is opened again once. A call the server fails, or does not answer, is refused with its own
  error, and the page marks that server as not answering until a call succeeds. A PC's `mcp: false` in settings opens none, and turned off
  closes those open and tells the holder; the person at the PC has a switch of their own. A stock PC's bridge finds
  none by itself: it offers the servers its `stock.mcp_servers` name, each `{"url": "http://<host>[:port][/path]",
  "token_file": ...}` on the PC's own tailnet address (a phone's native tools, say), asked while an agent holds the PC
  and dropped when the address stops answering. The bridge sends the server its token; a client's never passes.
  A phone's native tools are not RigDesk's: Rig Assistant, a module in the same Android app, serves them to the agent
  resident on the phone through Rig's rig-assistant plugin, with its own setup and no screen sharing needed.
* `desk/connect(id, password, why)` — joins a PC by its RustDesk ID and password. One of our RigDesk PCs that answers
  under that ID joins at once; any other PC is dialed only once the operator accepts, and its password waits in memory
  alone. Unanswered for 15 minutes, the request lapses: it is withdrawn and the agent is told. Known limits:
  * a request open when Rig restarts stays open, as nothing that would withdraw it outlives the restart; its password
    is gone, so a later Accept refuses and dials nothing;
  * Accept dials through the ID server RigDesk's settings name at that moment: if `connect_server` changed after the
    request was filed, that is not the server the request named.
* `desk/join(link)` with a device's invite, `rigdesk://id.<RustDesk ID>/<grant>#<secret>` — the person at RigDesk on
  Android made it, and it needs no password and no setup entry. Rig dials the device over the tailnet or through its ID
  server, and the login carries a signature by Rig's pairing key over the invite, that login's challenge and the agent
  Rig names. The device, which fetched the public half from the `pair_key` route, checks it and spends the invite on the
  first login that passes, so the first agent owns the session; it shows that agent and can end it. The invite lapses
  after 15 minutes unclaimed. Known limits: a device requiring 2FA refuses invites, the session is the desktop only (no
  terminal), and an invite spent on a login that then fails needs a new one.
* `desk/leave` — ends the session.

## Agent shell and health (RigDesk PCs)

A Windows RigDesk PC installed with `-AgentShell on` runs commands from Rig with no desktop session: as SYSTEM in session 0,
where nothing it starts can draw on the user's screen, or as the signed-in user. The client signs the consent text in
the [RigDesk README](../../README.md#consent-text) first. **The PC's bearer token is then a SYSTEM credential:** keep
its `token_file` 0600 under `~/.rig/rigdesk`, let only Rig's host reach `tag:pc`, and rotate it by rerunning the
install line.

A Linux PC installed with `install-linux.sh` (agent shell on by default) runs commands as root, from its own transient
systemd unit with no display, or as the user signed in at its screen. Its agent shell is root's **management lane**,
apart from the desktop user's agent API, whose token that user can read: it takes its own token, the lane token, and
answers at `<url>/lane`. Keep it with `POST /x/rigdesk/token {pc, token, lane: true}` (it lands in
`~/.rig/rigdesk/<pc>.lane.token`, 0600) and name it as the PC's `lane_token_file`; `desk/shell`, the switch and what ran
then go to the lane. **The lane token is a root credential.** While the lane does not answer, the PC's shell reads as
closed (`lane_down`). The grant level stays `system` on the wire and reads "root" for a Linux PC.

* `desk/shell(pc, command, as, timeout_s)` runs one command on the named PC without joining it and without waiting on
  the person at it; `background: true` starts a job and answers its id at once, and `desk/shell(pc, job)` reads its
  state and output so far. A job outlives the call and Rig's restarts, a reboot ends it, and a PC runs at most 8.
  Without `pc` it runs on the PC the agent joined, as before.
* **Who may.** The operator allows an agent or every agent in a graph per PC on the RigDesk page, as the user or as
  SYSTEM (`POST /x/rigdesk/shell_grant {pc, grantee, level}`, `level: null` removes it); the grants live in the plugin's
  `grants` doc beside the stock-PC assignments. The agent holding the PC's desktop session may run commands as the
  user without a grant. Background jobs and job reads need SYSTEM. A tool policy denying `mcp:desk/shell` still refuses
  it, and the PC refuses on its own when its switch is off (`rigdesk.shell_off`), the person at it paused it
  (`rigdesk.shell_paused`), the human blocked that agent (`rigdesk.blocked`), it already runs 8 jobs
  (`rigdesk.jobs_full`) or the job is unknown (`rigdesk.no_job`); Rig refuses an agent with no grant
  (`rigdesk.shell_not_allowed`) or with a user grant asking for more (`rigdesk.system_not_granted`). `desk/pcs` tells
  each agent, per PC, what it may run there and whether the switch is on.
* **What ran.** The PC journals every command, run or refused, with agent, graph, as whom, exit code and duration; the
  PC's opened row on the RigDesk page lists the latest 25 (`GET /x/rigdesk/shell_ran?pc=`), and the PC's tray lists them
  for the person there.

The PC takes a health sample within a minute of boot and every 10 minutes after (raw figures only, never a verdict)
and keeps 24 h of them. Rig reads each new one when the PC says it on `/events`, and on every reconnect reads all it
missed, so a Rig restart or a network gap leaves no hole. It keeps 7 days per PC under `health/` in the plugin's state
directory and judges the newest against `plugins.rigdesk.health`:

    {"disk_free_warn_pct": 15, "disk_free_fault_pct": 5, "disk_wear_warn_pct": 80,
     "signature_warn_days": 3, "signature_fault_days": 7, "system_errors_warn": 25, "stale_after_min": 25}

Defender real-time protection off or an unhealthy drive is a fault; a drive's warning, a stopped automatic service that
failed, or a failed health script warns; an up PC with no sample for `stale_after_min` reads stale. The verdict is
`ok`, `warn`, `fault` or `stale` with the failing checks in words.

* Each change of a PC's verdict is published once as the event **`rigdesk.pc_health`** (subject: the PC; `state`,
  `was`, `failing`, `client`), so a routine can wake an agent on it. `rig status` prints a line for each PC in fault.
* `desk/pcs` gives each PC's `health: {state, failing}`; `desk/pcs(pc)` adds its newest sample in full.

What the human does reaches the owning agent as Rig messages (steered into its turn): taking input
back, Stop (which also interrupts its turn), handing it over, block and unblock, the session ending, and messages typed in the viewer with
their screenshots and clips. Each is keyed by the PC's boot and event id, so a reconnect never
delivers one twice; an image is copied before the PC is told it arrived.

The PC alone decides who may act: when the human holds input the PC refuses actions (reads still
work), and while blocked it refuses every call.

## Pages

* `/x/rigdesk/all` (the **RigDesk** tab): each PC, up or down and why, who holds input, the owning agent, and its MCP
  servers open for that agent (`mcp` in `GET /x/rigdesk/pcs`: Rig's switch, the PC's, why none, and each server's key,
  words, tools, and why its last call failed while it still fails). Opening a
  PC's row takes a screenshot when asked (`POST /x/rigdesk/screenshot {pc}`, through the PC's `/frame`; shown, kept
  nowhere, journaled on the PC; it works while the human has blocked the agent, since Block binds agents, not the operator) and, for a stock PC, assigns it to any agent or every agent in a graph, picked by title;
  each standing grant is listed by the agent's title with its unassign.
* A row among each owning agent's holds, beside its shell and sub-agents: its PC, the PC's state or who holds input, and how long it has been joined; the row opens RigDesk.
* An up PC's row shows its health verdict, free space per volume, RAM, CPU, Windows build and uptime; opened, the newest
  sample in full with each volume's week of free space as a line (`GET /x/rigdesk/health?pc=`), and its agent shell:
  the PC's switch, the allow list with a form to add to it, and what ran.
* `rig status` prints a line for each PC that is down, and for each in health fault.

## Testing off Windows

`cargo run -p rigdesk-agent --example fake_desk -- 39119 <token>` serves the same API over a stand-in
desktop and reads viewer commands (`mint`, `hand`, `human`, `block`, `unblock`, `msg <text>`, and for the agent shell
`shell on|off`, `pause on|off`, `sample`, `reboot`) from stdin. Its lane runs commands through `sh`; `FAKE_DESK_DIR` keeps its
journal, jobs and samples, `FAKE_DESK_HEALTH_S` sets the sampling interval and `FAKE_DESK_FIGURES` names a JSON file of figures.
Point a PC's `url` at `http://127.0.0.1:39119` on a scratch Rig.
