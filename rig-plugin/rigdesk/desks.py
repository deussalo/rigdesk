"""The RigDesk PCs as Rig holds them: one /events stream per PC, the session each Rig agent claimed, and the calls it makes there.

The PC decides who may act; this side only carries. A PC's events reach the agent owning its session as Rig messages, keyed by the PC's
boot and event id, so a reconnect never delivers one twice. Images the human sends are copied here before the PC is told they arrived,
since the PC frees them then."""
from __future__ import annotations

import asyncio
import base64
import itertools
import binascii
import json
import re
import secrets
import socket
import time
from collections.abc import Callable
from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Annotated, Any, Literal, TypedDict, cast
from urllib.parse import urlsplit

import aiohttp
from pydantic import AfterValidator, BaseModel, ConfigDict, Field, StringConstraints, ValidationError, model_validator

from core.base import event_log, now_ms, replace_file
from core.event_kinds import EventField, StateSpec
from core.model import AgentId, GraphName, RefusedOutcome, SeatId
from core.node_data import Shape
from core.plugin_host import LiveEnding, PluginHost, PluginWords
from core.tasks import TASKS

from . import adhoc, prompts
from .health import STATES, History, Health, Thresholds, health, samples_of

ID = "rigdesk"
PC = f"{ID}.pc"
PC_HEALTH = f"{ID}.pc_health"
RENDEZVOUS = f"{ID}.rendezvous"
_CLIENT = EventField(name="client", type="text", description="The client the PC belongs to")
REPORTED = (
    StateSpec(PC, "What Rig's link to a RigDesk PC is now: connecting, up when its agent API answers, down when it does not", "pc",
              ("connecting", "up", "down"), "Why it is down, or how it answers", (_CLIENT,)),
    StateSpec(PC_HEALTH, "A RigDesk PC's health, as Rig judges its newest figures: ok, warn, fault, or stale, no sample for a while though the PC "
              "is up", "pc", STATES, "Each failing check in words, joined by '; '; every check passing when ok", (_CLIENT,)),
    StateSpec(RENDEZVOUS, "What one of the ID server's daemons is now, up when it takes a TCP connection on its port: hbbs, the rendezvous "
              "server, or hbbr, the relay", "server", ("up", "down"), "Why it is down, or where it answers",
              (EventField(name="address", type="text", description="host:port it is asked at"),)),
)
_LOG = event_log(__name__)
RETRY_S = (1.0, 2.0, 5.0, 10.0, 30.0)
SETTINGS_EVERY_S = 5.0
IDLE_LEAVE_MS = 5 * 60_000
CALL_LIMIT_S = 120.0
EVENTS_SILENT_S = 45.0
# How long a desk/connect to a PC not ours waits for the operator's approval before its password is dropped.
CONNECT_ASK_S = 15 * 60
SHELL_LIMIT_S = 600


def _call_limit_s(tool: str, arguments: dict[str, Any]) -> float:
    """A shell command may run up to its own limit, which the PC enforces; the call waits that long on top."""
    asked = arguments.get("timeout_s") if tool == "shell" else None
    return CALL_LIMIT_S + (asked if isinstance(asked, int) and 0 < asked <= SHELL_LIMIT_S else 60 if tool == "shell" else 0)
FILES_KEPT = 300
STOP_FRESH_MS = 60_000  # a Stop replayed after Rig was away is not acted on: the turn it meant is long gone
REMOTE_LOG_BYTES = 4 * 1024 * 1024
HEALTH_EVERY_S = 30.0
PROBE_LIMIT_S = 5.0
HBBS_PORT = 21116  # RustDesk's rendezvous port when the host names none; hbbr listens on the next one up
STOCK_PORTS = range(21120, 21200)
SECRETS = Path("~/.rig/rigdesk")
VIEWER_VERBS = frozenset({"mint", "hand", "human", "stop", "block", "unblock", "revoke"})
MCP_LIMIT_S = 15.0
MCP_CALL_LIMIT_S = 310.0  # the PC waits 300 s on its server's answer
MCP_PROTOCOL = "2025-06-18"
MCP_TOOL = re.compile(r"^[A-Za-z0-9_.-]{1,128}$")
MCP_KEY = re.compile(r"^[A-Za-z0-9_.-]{1,64}$")
LINK = re.compile(r"^rigdesk://(?P<host>[A-Za-z0-9._-]{1,253})/(?P<grant>[0-9a-f]{16,128})#(?P<secret>[0-9a-f]{16,128})$")
# A device's own invite names it by RustDesk ID, as id.<id>; no PC's name or tailnet host reads so, as an ID holds no dot.
INVITE_HOST = re.compile(r"^id\.(?P<id>[A-Za-z0-9_-]{6,32})$")
PAIR_KEY = "invite-pair.key"

type PcName = Annotated[str, StringConstraints(pattern=r"^[A-Za-z0-9_.-]{1,64}$")]
type Grantee = Annotated[str, StringConstraints(pattern=r"^(agent|graph):[A-Za-z0-9_.:-]{1,128}$")]
type ShellLevel = Literal["user", "system"]
GRANTS_DOC = "grants"
SAMPLES_LIMIT_S = 30.0
SHELL_LOG_READ = 2000
SHELL_LOG_SHOWN = 25


def _object(value: object) -> dict[str, Any]:
    """A JSON object from a PC or its server, or an empty one for anything else."""
    return cast(dict[str, Any], value) if isinstance(value, dict) else {}


def _objects(value: object) -> list[dict[str, Any]]:
    items: list[object] = cast(list[object], value) if isinstance(value, list) else []
    return [cast(dict[str, Any], each) for each in items if isinstance(each, dict)]


def _web_address(url: str) -> str:
    parts = urlsplit(url)
    if parts.scheme not in ("http", "https") or not parts.hostname:
        raise ValueError(f"{url!r} is not an http or https URL")
    if parts.username or parts.password:
        raise ValueError("a URL holds no credentials: name a token_file")
    return url.rstrip("/")


type WebAddress = Annotated[str, AfterValidator(_web_address)]


class StockMcpServer(Shape):
    model_config = ConfigDict(defer_build=True, json_schema_extra={"description": "An MCP server on the PC's own address, such as a phone's native tools: its bridge offers it to the holding agent as a found server."})
    url: Annotated[str, StringConstraints(pattern=r"^http://[A-Za-z0-9.:\[\]-]{1,255}(/[^\s]*)?$"), Field(description="http://host[:port][/path] (path /mcp by default), reached over the tailnet.")]
    token_file: Annotated[str | None, Field(description="A file readable by this user alone holding the bearer the server wants.")] = None


class StockRemote(Shape):
    model_config = ConfigDict(defer_build=True, json_schema_extra={"description": "A PC running stock RustDesk: Rig runs rigdesk-bridge for it, which logs in with the PC's permanent password, "
                                                                                 "by direct IP over the tailnet (peer) or by its RustDesk ID through the ID server (id)."})
    peer: Annotated[Annotated[str, StringConstraints(pattern=r"^[A-Za-z0-9.:\[\]-]{1,255}$")] | None, Field(description="The PC on the tailnet: its MagicDNS name or 100.x address, with :port if not 21118.")] = None
    id: Annotated[Annotated[str, StringConstraints(pattern=r"^[A-Za-z0-9_-]{6,32}$")] | None, Field(description="The PC's RustDesk ID, digits without spaces, for a PC registered with the id_server instead of on the tailnet.")] = None
    login_id: Annotated[Annotated[str, StringConstraints(pattern=r"^[A-Za-z0-9_-]{6,32}$")] | None, Field(description="With peer: the RustDesk ID this device shows, digits without spaces. "
                                                                                                                   "An invite the device makes is redeemed only through the entry naming its ID.")] = None
    password_file: Annotated[str | None, Field(description="A file readable by this user alone holding the PC's RustDesk permanent password, for unattended access. "
                                                       "Unset, an agent joins only when the person at the PC presses Allow on desk/request's prompt there.")] = None
    viewer_port: Annotated[int | None, Field(ge=1, le=65535, description="Where the bridge relays its session to RigDesk GUIs, on loopback (publish it with tailscale serve --tcp); unset, no GUI can attach.")] = None
    viewer_password_file: Annotated[str | None, Field(description="A file readable by this user alone holding the password GUIs log in to the bridge with.")] = None
    mcp_servers: Annotated[list[StockMcpServer], Field(description="MCP servers on the PC's own address that the bridge offers the holding agent.")] = []

    @model_validator(mode="after")
    def _viewer_whole(self) -> StockRemote:
        if (self.peer is None) == (self.id is None):
            raise ValueError("a stock PC has a peer (direct IP over the tailnet) or an id (through the ID server), not both")
        if self.login_id is not None and self.peer is None:
            raise ValueError("login_id goes with peer; a PC reached through the ID server is named by id")
        if (self.viewer_port is None) != (self.viewer_password_file is None):
            raise ValueError("viewer_port and viewer_password_file go together")
        return self


class IdServer(Shape):
    model_config = ConfigDict(defer_build=True, json_schema_extra={"description": "A self-hosted RustDesk ID server and the public key its PCs were configured with."})
    host: Annotated[str, StringConstraints(pattern=r"^[A-Za-z0-9.:\[\]-]{1,255}$"), Field(description="Where hbbs listens, as host or host:port (21116 by default); its relay is the one hbbs names (-r).")]
    key_file: Annotated[str, Field(description="A file holding hbbs's public key (id_ed25519.pub), the Key each PC's RustDesk was given.")]

    def listeners(self) -> tuple[tuple[Literal["hbbs", "hbbr"], str, int], ...]:
        """hbbs where the host says, and hbbr beside it on the next port, as rustdesk-server runs them."""
        name, port = self.host, HBBS_PORT
        if self.host.startswith("["):
            name, _, rest = self.host[1:].partition("]")
            port = int(rest[1:]) if rest.startswith(":") else HBBS_PORT
        elif self.host.count(":") == 1:
            name, given = self.host.split(":")
            port = int(given)
        return (("hbbs", name, port), ("hbbr", name, port + 1))


class ConnectServer(Shape):
    model_config = ConfigDict(defer_build=True, json_schema_extra={"description": "The RustDesk ID server desk/connect sends a bare ID to; unset, RustDesk's public server."})
    host: Annotated[str, StringConstraints(pattern=r"^[A-Za-z0-9.:\[\]-]{1,255}$"), Field(description="Where its hbbs listens, as host or host:port (21116 by default); its relay is the one hbbs names.")]
    key: Annotated[str, StringConstraints(pattern=r"^[A-Za-z0-9+/=]{43,44}$"), Field(description="Its public key (the id_ed25519.pub its PCs were given), base64.")]


class DeskPc(Shape):
    model_config = ConfigDict(defer_build=True, json_schema_extra={"description": "A PC running RigDesk: where its agent API is and the file holding its bearer token."})
    url: Annotated[WebAddress, Field(description="Its agent API, published by tailscale serve: https://<pc>.<tailnet>.ts.net; for a stock PC, http://127.0.0.1:<port> where Rig runs rigdesk-bridge.")]
    token_file: Annotated[str, Field(description="A file readable by this user alone holding the PC's agent-API token.")]
    client: Annotated[str, Field(description="The client this PC belongs to.")] = ""
    stock: Annotated[StockRemote | None, Field(description="Set for a PC running stock RustDesk instead of RigDesk; who may act on it with no share link is set on the RigDesk page.")] = None
    mcp: Annotated[bool, Field(description="The MCP servers running on the PC reach the agent holding it, as <pc>.<server>/<tool>; off, Rig opens none. The person at the PC has a switch of their own.")] = True
    lane_token_file: Annotated[str | None, Field(description="A Linux PC's: a file readable by this user alone holding its management lane's token, a root credential apart from the agent-API token. "
                                                             "Its agent shell is then reached at <url>/lane with it; unset, at <url> with token_file, as on Windows.")] = None

    @model_validator(mode="after")
    def _stock_is_local(self) -> DeskPc:
        if self.stock is not None and self.lane_token_file is not None:
            raise ValueError("a stock PC has no management lane: leave lane_token_file out")
        if self.stock is not None:
            parts = urlsplit(self.url)
            if parts.scheme != "http" or parts.hostname not in ("127.0.0.1", "localhost") or parts.port is None:
                raise ValueError("a stock PC's url is http://127.0.0.1:<port>, the port Rig runs its rigdesk-bridge on")
        return self


class RigdeskSettings(Shape):
    model_config = ConfigDict(defer_build=True, json_schema_extra={"description": "RigDesk PCs Rig agents may be handed with a share link."})
    pcs: Annotated[dict[PcName, DeskPc], Field(default_factory=dict[str, DeskPc], description='Each by name: {"deus": {"url": "https://deus.tail1234.ts.net", "token_file": "~/.rig/rigdesk/deus.token", "client": "acme"}}.')]
    for_fleet: Annotated[bool, Field(description="desk/* is open to every agent, so any agent given a link can join; off, only graphs and agents whose mcp_servers name desk.")] = True
    bridge_bin: Annotated[str, Field(description="The rigdesk-bridge program run for each stock PC: a path, or a name on PATH.")] = "rigdesk-bridge"
    id_server: Annotated[IdServer | None, Field(description="Our RustDesk ID server (hbbs, with hbbr beside it), through which stock PCs named by id are reached.")] = None
    health: Annotated[Thresholds, Field(description="Where a RigDesk PC's health figures warn or fault; the PC reports figures and Rig judges them.")] = Thresholds()
    connect_server: Annotated[ConnectServer | None, Field(description="Where desk/connect sends a bare RustDesk ID that is none of our PCs: an ID server's host and key. "
                                                                      "Unset, RustDesk's public server; an ID given as ID@host?key=<key> goes to that server instead.")] = None

    @model_validator(mode="after")
    def _ids_have_a_server(self) -> RigdeskSettings:
        by_id = sorted(name for name, pc in self.pcs.items() if pc.stock is not None and pc.stock.id is not None)
        if by_id and self.id_server is None:
            raise ValueError(f"{', '.join(by_id)} named by RustDesk id, but no id_server is set")
        return self


@dataclass
class Held:
    """One claimed session: the PC, the agent that owns it and where it sat when it joined, and the PC's session token."""
    pc: str
    agent: str
    graph: str
    node: str
    title: str
    session: str
    since_ms: int
    via: Literal["link", "assigned", "adhoc"] = "link"
    used_ms: int = 0


class DeskRefused(Exception):
    """A call that cannot go through, with the words the agent reads."""


class McpAnswered(DeskRefused):
    """A PC's MCP server, or the PC itself, answered and refused: a JSON-RPC error, or the PC's own 403 or 404. The server is not failing."""


class McpSessionGone(Exception):
    """A PC's MCP server no longer knows the session it gave: MCP's answer is HTTP 404, and the client opens a new one."""


class Grants(BaseModel):
    """rigdesk's own doc, written only on an operator's act: who holds a standing grant to each PC (Desks.assign, Desks.unassign),
    and who may run commands on each RigDesk PC's agent shell, as the user or with the highest local rights, SYSTEM or root (Desks.allow_shell)."""
    model_config = ConfigDict(frozen=True, extra="forbid")
    pcs: dict[PcName, tuple[Grantee, ...]] = {}
    shell: dict[PcName, dict[Grantee, ShellLevel]] = {}


@dataclass(frozen=True)
class PcServer:
    """One MCP server on a PC, as the holding agent's session opened it: its own words, its tools, and the MCP session it gave.
    Opened under one PC session; a new holder opens it afresh."""
    pc: str
    name: str
    says: str
    tools: tuple[dict[str, Any], ...]
    sid: str | None
    protocol: str
    session: str
    since_ms: int

    @property
    def key(self) -> str:
        return f"{self.pc}.{self.name}"


@dataclass
class PcLink:
    name: str
    spec: DeskPc
    state: Literal["connecting", "up", "down"] = "connecting"
    why: str | None = None
    down_since_ms: int | None = None
    standing: dict[str, Any] | None = None
    task: asyncio.Task[None] | None = None
    remote: asyncio.Task[None] | None = None
    remote_why: str | None = None
    servers: dict[str, PcServer] = field(default_factory=dict[str, PcServer])
    mcp_on: bool | None = None
    mcp_why: str | None = None
    # Each open server whose last call failed, by name: what went wrong and when; a later call that it answers clears it.
    server_faults: dict[str, tuple[str, int]] = field(default_factory=dict[str, tuple[str, int]])
    syncing: asyncio.Lock = field(default_factory=asyncio.Lock)
    # What the health watch is asking the PC now, and how long it waits, so a timeout can say what went unanswered.
    asking: tuple[str, float] = ("GET /health", 15.0)
    # The PC's agent shell as it last said: {open, code, words}; None for a RigDesk without one, or a stock PC.
    shell: dict[str, Any] | None = None
    # The OS its agent API names in /health: "windows" or "linux"; None until it answers, or from a RigDesk older than that.
    os: str | None = None
    # The host name the PC reports in /health, which its invites name it by; None until it answers, or from an older RigDesk.
    host: str | None = None
    # A Linux PC's watch on its management lane, which says the agent shell's switch apart from the desktop's agent API.
    lane: asyncio.Task[None] | None = None
    # The PC's switch for sessions the operator starts from Rig, as it last said: {open, code, words}; None for a RigDesk without /join, or a stock PC.
    join: dict[str, Any] | None = None
    sampling: asyncio.Lock = field(default_factory=asyncio.Lock)
    samples_why: str | None = None


@dataclass
class Listener:
    """One of the ID server's daemons as the last probe found it: up when it took a TCP connection on its port."""
    role: Literal["hbbs", "hbbr"]
    host: str
    port: int
    state: Literal["unchecked", "up", "down"] = "unchecked"
    why: str | None = None
    since_ms: int | None = None
    checked_ms: int | None = None

    @property
    def address(self) -> str:
        return f"[{self.host}]:{self.port}" if ":" in self.host else f"{self.host}:{self.port}"


class PcHolder(TypedDict):
    agent_id: str
    title: str
    graph: str


class PcEntry(TypedDict):
    """One RigDesk PC as desk/pcs reads it. assigned is who holds a standing grant to it ("agent:<id>" or "graph:<name>", in rigdesk's grants
    doc), which only a stock PC has; held_by is the agent that owns its session now, by a link or the grant."""

    name: str
    client: str
    stock: bool
    state: Literal["connecting", "up", "down"]
    why: str | None
    assigned: tuple[str, ...]
    held_by: PcHolder | None
    shell: dict[str, Any] | None
    os: str | None
    health: Health | None
    join: dict[str, Any] | None
    adhoc: dict[str, str] | None


@dataclass
class AdHoc:
    """A PC joined by ID and password, held while its bridge's one session lives: the bridge, the files it reads, and how it was reached."""
    id: str
    words: str = "connecting"
    process: asyncio.subprocess.Process | None = None
    files: tuple[Path, ...] = ()
    watch: asyncio.Task[None] | None = None
    port: int | None = None
    invited: bool = False
    # The settings entry an invite came through: the same device, so it cannot be joined under that name meanwhile.
    device: str | None = None


@dataclass(frozen=True)
class Invited:
    """A device's invite link, in place of a password: the bridge proves it with Rig's pairing key, inside RustDesk's login."""
    grant: str
    secret: str


@dataclass
class AskedConnect:
    """A desk/connect waiting on the operator's approval to dial out: its password is held here, in memory alone, until they accept or it lapses."""
    target: str
    password: str
    agent: str
    route: tuple[str, dict[str, str]]
    expiry: asyncio.Task[None] | None = None


class ConnectAsk(Exception):
    """desk/connect found none of our PCs under that ID: dialing out, to where, waits for the operator's approval."""
    def __init__(self, target: str, where: str, tried: tuple[str, ...]) -> None:
        super().__init__(f"{target} through {where}")
        self.target, self.where, self.tried = target, where, tried


class InviteAsk(Exception):
    """desk/join was given the invite a PC's person made, on a PC where an agent holds a shell grant: claiming it waits for the operator's Accept."""
    def __init__(self, pc: str) -> None:
        super().__init__(pc)
        self.pc = pc


def failure(failed: BaseException) -> str:
    """An exception in words. A timeout's own text is usually empty, so it is named as one."""
    words = str(failed).strip()
    if isinstance(failed, TimeoutError):
        return f"timed out ({words})" if words else "timed out"
    return f"{type(failed).__name__}: {words}" if words else type(failed).__name__


def _grants(assigned: tuple[str, ...], agent: str, graph: str) -> bool:
    return bool({f"agent:{agent}", f"graph:{graph}"} & set(assigned))


def pc_of_link(link: str, pcs: dict[str, DeskPc], hosts: dict[str, str] | None = None) -> tuple[str, str, str] | str:
    """The PC a share link names, and its grant and secret; or why it names none. The host is the PC's name, its URL's host, or that host's first label;
    or the host name the PC reported in hosts, by PC name, as an invite from its tray names it."""
    matched = LINK.match(link.strip())
    if matched is None:
        return ("that is not a RigDesk share link: one reads rigdesk://<pc>/<grant>#<secret>, copied with Share with agent in the viewer, or Invite a Rig agent in the PC's tray. "
                "Given a PC's RustDesk ID and password instead, desk/connect(id, password) joins it")
    host = matched["host"].lower()
    for name, spec in pcs.items():
        url_host = (urlsplit(spec.url).hostname or "").lower()
        if host in (name.lower(), url_host, url_host.split(".")[0]):
            return name, matched["grant"], matched["secret"]
    for name, reported in (hosts or {}).items():
        if name in pcs and host == reported.lower():
            return name, matched["grant"], matched["secret"]
    return f"no RigDesk PC named {host} is set up in Rig (plugins.rigdesk.pcs); the operator adds it there"


def _clock(at_ms: int) -> str:
    return time.strftime("%H:%M:%S", time.localtime(at_ms / 1000))


def words_of(pc: str, event: dict[str, Any]) -> str | None:
    """What the owning agent is told of one event from its PC; None for one it is not told of. A holder change is told by the PC's refusal
    of its next call, and an ended session by the plugin's (Desks._ended)."""
    kind, at = event.get("type"), _clock(int(event.get("at_ms") or now_ms()))
    if kind == "mcp":
        return (f"RigDesk {pc}: the human turned MCP back on at {at}; the MCP servers running there are offered to you again as they are found."
                if event.get("on") else
                f"RigDesk {pc}: the human turned MCP off at {at}; no MCP server there is offered to you until they turn it back on. "
                "Screen, input and shell are unchanged.")
    if kind == "revoked":
        why = str(event.get("reason") or "the share ended")
        if why == "the agent left":
            return None
        return f"RigDesk {pc}: your session there ended at {at} ({why}). Desk calls there are refused until you join it again."
    if kind == "dropped":
        count = int(event.get("messages") or 0)
        return (f"RigDesk {pc}: {count} of the human's messages were dropped unread at {at}: the PC's queue was full." if count
                else f"RigDesk {pc}: events from the PC were missed while Rig was away; ask the human to repeat anything they sent.")
    return None


def message_words(pc: str, text: str, images: list[tuple[str, Path | None]]) -> str:
    lines = [f"RigDesk {pc}: the human writes (their words, from the PC's viewer):", text or "(no text)"]
    for image, path in images:
        lines.append(f"Image {image}: " + (f"{path}" if path is not None else "not copied; desk/attachment may still fetch it") + ".")
    if images:
        lines.append("Read an image as a file, or open it with desk/attachment(id).")
    return "\n".join(lines)


class Desks:
    def __init__(self, host: PluginHost) -> None:
        self.host = host
        self._dir: Path = host.state_dir
        self._pcs: dict[str, PcLink] = {}
        self._for_fleet = True
        self._bridge_bin = "rigdesk-bridge"
        self._id_server: IdServer | None = None
        self._catalog: dict[str, dict[str, Any]] = {}
        self._tools: tuple[Any, ...] = ()
        self._tools_of = -1
        self._revision = 0
        self._sessions: dict[str, Held] = {}
        self._boots: dict[str, str] = {}
        self._last: dict[str, int] = {}
        # The newest event id handled per PC, and the human's messages not yet told to their agent: the resume point
        # (_last) stays below the oldest of those, so a reconnect or restart brings it back.
        self._seen: dict[str, int] = {}
        self._unsent: dict[str, dict[int, dict[str, Any]]] = {}
        self._ids = itertools.count(1)
        self._http: aiohttp.ClientSession | None = None
        self._watch: asyncio.Task[None] | None = None
        self._probe: asyncio.Task[None] | None = None
        self._probe_now = asyncio.Event()
        self._listeners: tuple[Listener, ...] = ()
        self._grants = Grants()
        self._grants_revision = 0
        self._granting = asyncio.Lock()
        self._thresholds = Thresholds()
        self._history = History(self._dir / "health")
        self._adhoc: dict[str, AdHoc] = {}
        self._asked: dict[str, AskedConnect] = {}
        self._adhoc_lost: list[Held] = []
        # Why an agent's session ended without its asking, by (agent, graph): its next desk call's refusal says so, until it joins again.
        self._ended_why: dict[tuple[str, str], str] = {}
        self.prompts: dict[str, prompts.Prompt] = {}
        self.person_answered: set[str] = set()
        self._read_state()

    # --- state kept across restarts ---

    @property
    def _state_file(self) -> Path:
        return self._dir / "state.json"

    def _read_state(self) -> None:
        try:
            kept = json.loads(self._state_file.read_text())
        except (OSError, ValueError):
            kept = {}
        self._sessions = {pc: Held(**held) for pc, held in kept.get("sessions", {}).items()}
        self._adhoc_lost = [self._sessions.pop(pc) for pc in [pc for pc, held in self._sessions.items() if held.via == "adhoc"]]
        self._boots = dict(kept.get("boots", {}))
        self._last = {pc: int(last) for pc, last in kept.get("last", {}).items()}
        self._catalog = {tool["name"]: tool for tool in kept.get("catalog", []) if isinstance(tool, dict) and "name" in tool}

    def _write_state(self) -> None:
        kept = {"sessions": {pc: asdict(held) for pc, held in self._sessions.items()}, "boots": self._boots, "last": self._last,
                "catalog": list(self._catalog.values())}
        replace_file(self._state_file, json.dumps(kept).encode(), new_mode=0o600, make_parents=True)

    # --- lifecycle ---

    async def settle(self) -> None:
        await self._read_grants()
        self._http = aiohttp.ClientSession(timeout=aiohttp.ClientTimeout(total=None, sock_connect=10))
        self._watch = TASKS.spawn("rigdesk-settings", self._watching())
        self._probe = TASKS.spawn("rigdesk-id-server", self._probing())
        TASKS.spawn("rigdesk-adhoc-lost", self._adhoc_restarted())

    async def end(self, _ending: LiveEnding) -> None:
        tasks = [task for task in (self._watch, self._probe, *(task for link in self._pcs.values() for task in (link.task, link.remote, link.lane))) if task is not None]
        for task in tasks:
            task.cancel()
        await asyncio.gather(*tasks, return_exceptions=True)
        await self._adhoc_end_all()
        await prompts.end_all(self)
        for asked in self._asked.values():
            if asked.expiry is not None:
                asked.expiry.cancel()
        self._asked.clear()
        if self._http is not None:
            await self._http.close()

    async def _watching(self) -> None:
        while True:
            try:
                settings = await asyncio.to_thread(self.host.settings, RigdeskSettings)
                self._reconcile(settings)
                await self._prune_grants()
                await self._idle_leaves()
                await self._adhoc_idle()
                await self._resend()
                await self._judge()
            except Exception as failed:  # noqa: BLE001
                _LOG.warning("rigdesk_settings_unread", f"RigDesk settings could not be read, PCs stay as they are: {failure(failed)}")
            await asyncio.sleep(SETTINGS_EVERY_S)

    def _reconcile(self, settings: RigdeskSettings) -> None:
        self._for_fleet = settings.for_fleet
        self._thresholds = settings.health
        moved = (settings.bridge_bin, settings.id_server) != (self._bridge_bin, self._id_server)
        self._bridge_bin = settings.bridge_bin
        if settings.id_server != self._id_server:
            self._listeners = () if settings.id_server is None else tuple(Listener(role, host, port) for role, host, port in settings.id_server.listeners())
            self.host.forget(RENDEZVOUS, {listener.role for listener in self._listeners})
            self._probe_now.set()
        self._id_server = settings.id_server
        for pc in [pc for pc in self._sessions if pc not in settings.pcs and pc not in self._adhoc]:
            held = self._sessions.pop(pc)
            self._unsent.pop(pc, None)
            _LOG.info("rigdesk_session_ended", f"RigDesk {pc}: the PC left Rig's settings, so {held.title}'s session there ended", pc=pc)
            TASKS.spawn(f"rigdesk-gone-{pc}", self._ended(held, f"RigDesk {pc}: the operator removed {pc} from Rig, so your session there ended."))
        carried: dict[str, dict[str, PcServer]] = {}
        for name in [name for name, link in self._pcs.items() if name not in self._adhoc and (settings.pcs.get(name) != link.spec or (moved and link.spec.stock is not None))]:
            link = self._pcs.pop(name)
            for task in (link.task, link.remote, link.lane):
                if task is not None:
                    task.cancel()
            if link.servers:
                carried[name] = link.servers
                self._revision += 1
        for name, spec in settings.pcs.items():
            if name not in self._pcs:
                # A PC whose entry changed keeps the servers its holder has open, so the next sync tells the holder what the change closed.
                link = self._pcs[name] = PcLink(name, spec, servers=carried.get(name, {}))
                if name in carried:
                    self._resync(link)
                if spec.stock is not None:
                    link.remote = TASKS.spawn(f"rigdesk-bridge-{name}", self._remote(link, spec.stock))
                link.task = TASKS.spawn(f"rigdesk-{name}", self._held(link))
                if spec.lane_token_file is not None:
                    link.lane = TASKS.spawn(f"rigdesk-lane-{name}", self._lane_held(link))
                self._linked(link)
        self.host.forget(PC, self._pcs.keys())
        self.host.forget(PC_HEALTH, self._pcs.keys())

    def _linked(self, link: PcLink) -> None:
        """The PC's link as it is now, told to Rig, which publishes a change of it and prints it in rig status."""
        why = link.why or ("its agent API answers" if link.state == "up" else f"connecting to {link.spec.url}")
        self.host.report(PC, link.name, link.state, why, {"client": link.spec.client})

    # --- standing grants, rigdesk's own fact ---

    async def _read_grants(self) -> None:
        """Grants are source facts: a doc that cannot be read stops the plugin, rather than starting with every grant gone."""
        doc = await self.host.docs.get(GRANTS_DOC)
        if doc is None:
            return
        try:
            self._grants = Grants.model_validate(doc.body)
        except ValidationError as broken:
            raise RuntimeError(f"rigdesk's grants doc (revision {doc.revision}) is unreadable: {broken}") from broken
        self._grants_revision = doc.revision

    async def _save_grants(self, grants: Grants) -> None:
        """Written before it is applied, and only over the revision read: a second writer is refused loudly, never overwritten."""
        doc = await self.host.docs.put(GRANTS_DOC, grants.model_dump(mode="json"), expect=self._grants_revision)
        self._grants, self._grants_revision = grants, doc.revision

    def granted(self, pc: str) -> tuple[str, ...]:
        return self._grants.pcs.get(pc, ())

    async def assign(self, pc: str, grantee: str) -> bool:
        """Grants one agent or graph a standing grant to a PC; False when it held one already. On a RigDesk PC its sessions start through /join,
        which the PC's own switch still admits."""
        async with self._granting:
            link = self._pcs.get(pc)
            if link is None:
                raise DeskRefused(f"no RigDesk PC named {pc} is set up in Rig")
            held = self.granted(pc)
            if grantee in held:
                return False
            await self._save_grants(Grants(pcs=self._grants.pcs | {pc: (*held, grantee)}, shell=self._grants.shell))
            return True

    async def unassign(self, pc: str, grantee: str) -> bool:
        """Takes one grant away; a session held through it ends now. False when there was none."""
        async with self._granting:
            held = self.granted(pc)
            if grantee not in held:
                return False
            left = tuple(each for each in held if each != grantee)
            await self._save_grants(Grants(pcs={name: grantees for name, grantees in (self._grants.pcs | {pc: left}).items() if grantees}, shell=self._grants.shell))
        session = self._sessions.get(pc)
        if session is not None and session.via == "assigned" and not _grants(left, session.agent, session.graph):
            if self._link(pc).spec.stock is None:
                await self._ungranted(session)
            else:
                await self._unassigned(pc)
        return True

    async def _prune_grants(self) -> None:
        """A standing grant exists only on a PC set up in Rig, and a shell grant only on a RigDesk PC: a PC removed from settings,
        or a stock PC now, loses its grants."""
        async with self._granting:
            kept = {pc: grantees for pc, grantees in self._grants.pcs.items() if pc in self._pcs}
            shell = {pc: levels for pc, levels in self._grants.shell.items() if (link := self._pcs.get(pc)) is not None and link.spec.stock is None}
            if kept != self._grants.pcs or shell != self._grants.shell:
                gone = sorted((set(self._grants.pcs) - set(kept)) | (set(self._grants.shell) - set(shell)))
                await self._save_grants(Grants(pcs=kept, shell=shell))
                _LOG.warning("rigdesk_grants_pruned", f"RigDesk: {', '.join(gone)} left Rig's settings or changed kind, so their grants were removed", pcs=", ".join(gone))

    def shell_granted(self, pc: str) -> dict[str, ShellLevel]:
        return self._grants.shell.get(pc, {})

    def shell_level(self, pc: str, agent: str, graph: str) -> ShellLevel | None:
        """The most this agent may run as on a PC's agent shell, by its own grant or its graph's; None with neither."""
        granted = self.shell_granted(pc)
        levels = {granted.get(f"agent:{agent}"), granted.get(f"graph:{graph}")}
        return "system" if "system" in levels else "user" if "user" in levels else None

    async def allow_shell(self, pc: str, grantee: str, level: ShellLevel | None) -> bool:
        """Lets one agent or graph run commands on a RigDesk PC's agent shell as the user, or as SYSTEM, or takes that away (None);
        False when it stood so already."""
        async with self._granting:
            link = self._pcs.get(pc)
            if link is None:
                raise DeskRefused(f"no RigDesk PC named {pc} is set up in Rig")
            if link.spec.stock is not None:
                raise DeskRefused(f"{pc} runs stock RustDesk, which has no agent shell: every connection to it opens a session its person sees")
            granted = self.shell_granted(pc)
            if granted.get(grantee) == level:
                return False
            changed = {each: held for each, held in granted.items() if each != grantee}
            if level is not None:
                changed[grantee] = level
            await self._save_grants(Grants(pcs=self._grants.pcs, shell={name: levels for name, levels in (self._grants.shell | {pc: changed}).items() if levels}))
            return True

    # --- the ID server's health ---

    async def _probing(self) -> None:
        """Every HEALTH_EVERY_S, and at once when the id_server setting moves, each daemon is asked for a TCP connection."""
        while True:
            await asyncio.gather(*(self._probed(listener) for listener in self._listeners))
            self._probe_now.clear()
            try:
                await asyncio.wait_for(self._probe_now.wait(), HEALTH_EVERY_S)
            except TimeoutError:
                pass

    async def _probed(self, listener: Listener) -> None:
        try:
            _, writer = await asyncio.wait_for(asyncio.open_connection(listener.host, listener.port), PROBE_LIMIT_S)
        except TimeoutError:
            state, why = "down", f"no TCP connection within {PROBE_LIMIT_S:.0f} s"
        except OSError as failed:
            state, why = "down", failed.strerror or failure(failed)
        else:
            writer.close()
            state, why = "up", None
        if state != listener.state:
            listener.since_ms = now_ms()
            if state == "down":
                _LOG.warning("rigdesk_id_server_down", f"RigDesk ID server: {listener.role} at {listener.address} is down: {why}", role=listener.role)
        listener.state, listener.why, listener.checked_ms = state, why, now_ms()
        if listener in self._listeners:
            self.host.report(RENDEZVOUS, listener.role, state, why or f"it takes connections at {listener.address}", {"address": listener.address})

    def id_server(self) -> dict[str, Any] | None:
        """The ID server and its daemons' health, for the page and rig status; None with no id_server set."""
        if self._id_server is None:
            return None
        return {"host": self._id_server.host, "listeners": [{"role": each.role, "address": each.address, "state": each.state, "why": each.why,
                                                             "since_ms": each.since_ms, "checked_ms": each.checked_ms} for each in self._listeners]}

    def install_config(self) -> str:
        """RustDesk's config string for our ID server, as `rustdesk --config` takes it: base64url of {host, key} reversed.
        Stock RustDesk splits the string at '--' before decoding it, so JSON padding is added until the encoding holds none."""
        if self._id_server is None:
            raise DeskRefused("no id_server is set in plugins.rigdesk, so there is no ID server to put a PC on")
        try:
            key = Path(self._id_server.key_file).expanduser().read_text().strip()
        except OSError as unread:
            raise DeskRefused(f"the ID server's key file {self._id_server.key_file} could not be read: {unread}") from unread
        shape = json.dumps({"host": self._id_server.host, "key": key, "api": "", "relay": ""}, separators=(",", ":"))
        for pad in range(16):
            config = base64.urlsafe_b64encode((shape + " " * pad).encode()).decode().rstrip("=")[::-1]
            if "--" not in config:
                return config
        raise DeskRefused("no config string for this ID server avoids '--', which RustDesk splits on")

    # --- adding a stock PC by its RustDesk ID ---

    @staticmethod
    def _free(port: int) -> bool:
        with socket.socket() as probe:
            try:
                probe.bind(("127.0.0.1", port))
            except OSError:
                return False
        return True

    @classmethod
    def _first_free(cls, taken: set[int | None]) -> int:
        for port in STOCK_PORTS:
            if port not in taken and cls._free(port):
                return port
        raise DeskRefused(f"no loopback port in {STOCK_PORTS.start}-{STOCK_PORTS.stop - 1} is free for its rigdesk-bridge")

    def _keep_secret(self, name: str, kind: str, value: str) -> str:
        path = SECRETS / f"{name}.{kind}"
        real = path.expanduser()
        real.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        replace_file(real, value.encode(), new_mode=0o600)
        return str(path)

    async def added(self, name: str, rustdesk_id: str, password: str, client: str) -> DeskPc:
        """A stock PC on our ID server, ready for settings: its password and a fresh bridge token kept in 0600 files, and a loopback port no PC uses.
        Settings are the caller's to write; once they hold it, the next reconcile starts its rigdesk-bridge."""
        settings = await asyncio.to_thread(self.host.settings, RigdeskSettings)
        if settings.id_server is None:
            raise DeskRefused("no id_server is set in plugins.rigdesk: a PC named by RustDesk ID is reached through it")
        if name in settings.pcs:
            raise DeskRefused(f"a PC named {name} is already set up")
        same = next((other for other, pc in settings.pcs.items() if pc.stock is not None and pc.stock.id == rustdesk_id), None)
        if same is not None:
            raise DeskRefused(f"RustDesk ID {rustdesk_id} is already set up as {same}")
        taken = {urlsplit(pc.url).port for pc in settings.pcs.values()} | {pc.stock.viewer_port for pc in settings.pcs.values() if pc.stock is not None}
        port = await asyncio.to_thread(self._first_free, taken)
        password_file = await asyncio.to_thread(self._keep_secret, name, "password", password)
        token_file = await asyncio.to_thread(self._keep_secret, name, "token", secrets.token_urlsafe(32))
        return DeskPc(url=f"http://127.0.0.1:{port}", token_file=token_file, client=client, stock=StockRemote(id=rustdesk_id, password_file=password_file))

    @staticmethod
    def _replaced_secret(path: Path, value: str) -> None:
        path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        if path.exists():
            path.chmod(0o600)
        replace_file(path, value.encode(), new_mode=0o600)

    async def set_password(self, pc: str, password: str) -> str:
        """Replaces a stock PC's saved password in the file its entry names, and restarts its rigdesk-bridge so it logs in with it now. A session an
        agent holds there ends with the bridge, and the agent's next desk call says so."""
        link = self._pcs.get(pc)
        if link is None or pc in self._adhoc:
            raise DeskRefused(f"no PC named {pc} is set up in Rig's settings")
        stock = link.spec.stock
        if stock is None:
            raise DeskRefused(f"{pc} is a RigDesk PC: Rig reaches it with its token, so it keeps no password")
        path = stock.password_file
        await asyncio.to_thread(self._replaced_secret, Path(path).expanduser(), password)
        old = link.remote
        if old is not None:
            old.cancel()
            await asyncio.gather(old, return_exceptions=True)
        held = self._sessions.pop(pc, None)
        if held is not None:
            self._unsent.pop(pc, None)
            await asyncio.to_thread(self._write_state)
            await self._ended(held, f"RigDesk {pc}: the operator replaced its saved password, which restarts its bridge, so your session there ended. "
                                    "desk/join or desk/request it again.")
        # A settings change that replaced the link, or another call, started the bridge already, and it reads the new file.
        if self._pcs.get(pc) is link and link.remote is old:
            link.remote = TASKS.spawn(f"rigdesk-bridge-{pc}", self._remote(link, stock))
        _LOG.info("rigdesk_password_set", f"RigDesk {pc}: the operator replaced its saved password, and its rigdesk-bridge restarted", pc=pc)
        return path

    # --- rigdesk-bridge, for a stock PC ---

    def _remote_log(self, name: str) -> Any:
        path = self._dir / f"bridge-{name}.log"
        path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        if path.exists() and path.stat().st_size > REMOTE_LOG_BYTES:
            path.replace(path.with_suffix(".log.1"))
        return open(path, "ab")  # noqa: SIM115

    def _route(self, stock: StockRemote) -> list[str]:
        """The bridge's arguments for reaching the PC: its tailnet address, or its ID through our ID server."""
        if stock.peer is not None:
            return ["--peer", stock.peer]
        assert stock.id is not None and self._id_server is not None, "settings validation pairs an id with an id_server"
        return ["--id", stock.id, "--id-server", self._id_server.host, "--key-file", str(Path(self._id_server.key_file).expanduser())]

    async def _remote(self, link: PcLink, stock: StockRemote) -> None:
        """Keeps one rigdesk-bridge running for a stock PC, restarting it when it exits; its output goes to bridge-<pc>.log here."""
        argv = [str(Path(self._bridge_bin).expanduser()) if "/" in self._bridge_bin else self._bridge_bin,
                *self._route(stock), *([] if stock.password_file is None else ["--password-file", str(Path(stock.password_file).expanduser())]),
                "--token-file", str(Path(link.spec.token_file).expanduser()), "--port", str(urlsplit(link.spec.url).port),
                "--journal", str(self._dir / f"bridge-{link.name}.journal")]
        if stock.viewer_port is not None and stock.viewer_password_file is not None:
            argv += ["--viewer-port", str(stock.viewer_port), "--viewer-password-file", str(Path(stock.viewer_password_file).expanduser())]
        for server in stock.mcp_servers:
            argv += ["--mcp-server", server.url] + ([] if server.token_file is None else ["--mcp-token-file", str(Path(server.token_file).expanduser())])
        tries = 0
        while True:
            started = time.monotonic()
            log = await asyncio.to_thread(self._remote_log, link.name)
            try:
                process = await asyncio.create_subprocess_exec(*argv, stdin=asyncio.subprocess.DEVNULL, stdout=log, stderr=log, start_new_session=True)
            except OSError as failed:
                log.close()
                link.remote_why = f"rigdesk-bridge could not start ({argv[0]}): {failed}"
                _LOG.warning("rigdesk_bridge_unstarted", f"RigDesk {link.name}: {link.remote_why}", pc=link.name)
            else:
                log.close()
                link.remote_why = None
                try:
                    code = await process.wait()
                except asyncio.CancelledError:
                    await self._stopped(process)
                    raise
                link.remote_why = f"rigdesk-bridge exited with code {code}; see bridge-{link.name}.log in the plugin's state directory"
                _LOG.warning("rigdesk_bridge_exited", f"RigDesk {link.name}: {link.remote_why}", pc=link.name)
            tries = 0 if time.monotonic() - started > 60 else tries + 1
            await asyncio.sleep(RETRY_S[min(tries, len(RETRY_S) - 1)])

    @staticmethod
    async def _stopped(process: asyncio.subprocess.Process) -> None:
        if process.returncode is not None:
            return
        process.terminate()
        try:
            await asyncio.wait_for(asyncio.shield(process.wait()), 5)
        except TimeoutError:
            process.kill()

    # --- one PC ---

    @staticmethod
    def _token(token_file: str) -> str:
        try:
            token = Path(token_file).expanduser().read_text().strip()
        except OSError as unread:
            raise DeskRefused(f"its token file {token_file} could not be read: {unread}") from unread
        if not token or any(ord(char) < 0x21 or ord(char) > 0x7e for char in token):
            raise DeskRefused(f"its token file {token_file} is empty or holds characters a token cannot")
        return token

    def _headers(self, link: PcLink, session: str | None = None) -> dict[str, str]:
        headers = {"Authorization": f"Bearer {self._token(link.spec.token_file)}"}
        return headers | ({"X-Desk-Session": session} if session else {})

    @staticmethod
    def _shell_reachable(link: PcLink) -> None:
        """Refuses while the process serving the PC's agent shell is not reached: on Windows the agent API; a Linux PC's lane is root's own
        process, up while the desktop's is not (nobody signed in, the user's RigDesk restarting), and a lane that does not answer fails at
        the request."""
        if link.spec.lane_token_file is None and link.state != "up":
            raise DeskRefused(f"RigDesk PC {link.name} is down: {link.why or 'not reached yet'}")

    def _shell_at(self, link: PcLink) -> tuple[str, dict[str, str]]:
        """Where the PC's agent shell answers, and with which token: a Linux PC's management lane, or the agent API itself on Windows."""
        if link.spec.lane_token_file is None:
            return link.spec.url, self._headers(link)
        return f"{link.spec.url}/lane", {"Authorization": f"Bearer {self._token(link.spec.lane_token_file)}"}

    def _client(self) -> aiohttp.ClientSession:
        if self._http is None:
            raise DeskRefused("the RigDesk plugin is still starting")
        return self._http

    async def _held(self, link: PcLink) -> None:
        tries = 0
        while True:
            try:
                await self._connected(link)
                tries = 0
                why = "its event stream ended"
            except asyncio.CancelledError:
                raise
            except DeskRefused as refused:
                why = str(refused)
            except TimeoutError as failed:
                asked, limit_s = link.asking
                said = str(failed).strip()
                why = f"no answer from the PC's agent API to {asked} in {limit_s:g} s" + (f" ({said})" if said else "")
            except Exception as failed:  # noqa: BLE001
                why = f"{link.asking[0]} failed: {failure(failed)}"
            if link.state != "down":
                link.down_since_ms = now_ms()
                _LOG.warning("rigdesk_pc_down", f"RigDesk PC {link.name} is down: {why}", pc=link.name)
            link.state, link.why = "down", why
            self._linked(link)
            self._resync(link)
            await asyncio.sleep(RETRY_S[min(tries, len(RETRY_S) - 1)])
            tries += 1

    async def _connected(self, link: PcLink) -> None:
        http = self._client()
        link.asking = ("GET /health", 15.0)
        async with http.get(f"{link.spec.url}/health", headers=self._headers(link), timeout=aiohttp.ClientTimeout(total=15)) as answer:
            if answer.status != 200:
                raise DeskRefused(f"/health answered HTTP {answer.status}" + (": the token is refused" if answer.status == 401 else ""))
            health = await answer.json()
        link.standing = health.get("standing")
        link.os = health["os"] if isinstance(health.get("os"), str) else None
        link.host = health["host"] if isinstance(health.get("host"), str) else None
        if link.spec.lane_token_file is None:
            link.shell = _object(health["shell"]) if isinstance(health.get("shell"), dict) else None
        link.join = _object(health["join"]) if isinstance(health.get("join"), dict) else None
        down = health.get("down")
        boot = str(health.get("boot", ""))
        if boot != self._boots.get(link.name):
            await self._rebooted(link.name, boot)
        link.asking = ("tools/list on /mcp", 15.0)
        await self._listed(link)
        # The agent API beats every 15 s, so a stream silent for three beats is dead even when a proxy keeps the socket open.
        link.asking = ("GET /events", EVENTS_SILENT_S)
        headers = self._headers(link) | {"Last-Event-ID": str(self._last.get(link.name, 0)), "Accept": "text/event-stream"}
        async with http.get(f"{link.spec.url}/events", headers=headers, timeout=aiohttp.ClientTimeout(total=None, sock_read=EVENTS_SILENT_S)) as stream:
            if stream.status != 200:
                raise DeskRefused(f"/events answered HTTP {stream.status}")
            link.state, link.why, link.down_since_ms = "up", None, None
            self._reached(link, down if isinstance(down, str) else None)
            self._linked(link)
            self._resync(link)
            if link.spec.stock is None and link.spec.lane_token_file is None:
                TASKS.spawn(f"rigdesk-samples-{link.name}", self._catch_up(link, None))
            data: list[str] = []
            async for raw in stream.content:
                line = raw.decode("utf-8", "replace").rstrip("\r\n")
                if line.startswith("data:"):
                    data.append(line[5:].lstrip())
                elif not line and data:
                    try:
                        event = json.loads("\n".join(data))
                    except ValueError:
                        event = None
                    data = []
                    if isinstance(event, dict):
                        await self._event(link, event)

    async def _lane_held(self, link: PcLink) -> None:
        """A Linux PC's agent shell is root's management lane, apart from the desktop user's agent API: its switch is read from the lane's
        /health and kept by the lane's switch events, and its health samples are read there; while the lane does not answer, the shell
        reads as closed and says why."""
        tries = 0
        while True:
            try:
                url, headers = self._shell_at(link)
                async with self._client().get(f"{url}/health", headers=headers, timeout=aiohttp.ClientTimeout(total=15)) as answer:
                    if answer.status != 200:
                        raise DeskRefused(f"/lane/health answered HTTP {answer.status}" + (": the lane token is refused" if answer.status == 401 else ""))
                    health = _object(await answer.json(content_type=None))
                link.shell = _object(health["shell"]) if isinstance(health.get("shell"), dict) else None
                tries = 0
                async with self._client().get(f"{url}/events", headers=headers | {"Accept": "text/event-stream"},
                                              timeout=aiohttp.ClientTimeout(total=None, sock_read=EVENTS_SILENT_S)) as stream:
                    if stream.status != 200:
                        raise DeskRefused(f"/lane/events answered HTTP {stream.status}")
                    TASKS.spawn(f"rigdesk-samples-{link.name}", self._catch_up(link, None))
                    async for raw in stream.content:
                        line = raw.decode("utf-8", "replace").rstrip("\r\n")
                        if not line.startswith("data:"):
                            continue
                        try:
                            event = _object(json.loads(line[5:].lstrip()))
                        except ValueError:
                            continue
                        if event.get("type") == "shell_switch":
                            link.shell = {"open": event.get("open") is True, "code": event.get("code"), "words": event.get("words")}
                        elif event.get("type") == "health":
                            TASKS.spawn(f"rigdesk-samples-{link.name}", self._catch_up(link, int(event.get("sample") or 0)))
                why = "its event stream ended"
            except asyncio.CancelledError:
                raise
            except Exception as failed:  # noqa: BLE001
                why = str(failed) if isinstance(failed, DeskRefused) else failure(failed)
            if link.shell is None or link.shell.get("code") != "lane_down":
                _LOG.warning("rigdesk_lane_down", f"RigDesk {link.name}: its management lane does not answer: {why}", pc=link.name)
            link.shell = {"open": False, "code": "lane_down", "words": f"its management lane at /lane does not answer: {why}"}
            await asyncio.sleep(RETRY_S[min(tries, len(RETRY_S) - 1)])
            tries += 1

    async def _rebooted(self, pc: str, boot: str) -> None:
        """A PC whose agent host restarted lost its sessions and numbers its events afresh."""
        known = pc in self._boots
        self._boots[pc], self._last[pc], self._seen[pc] = boot, 0, 0
        self._unsent.pop(pc, None)
        held = self._sessions.pop(pc, None)
        if (link := self._pcs.get(pc)) is not None:
            self._resync(link)
        await asyncio.to_thread(self._write_state)
        if known and held is not None:
            _LOG.info("rigdesk_session_ended", f"RigDesk {pc}: RigDesk restarted, so {held.title}'s session there ended", pc=pc)
            await self._ended(held, f"RigDesk {pc}: RigDesk restarted there, so your session ended. Desk calls there are refused until you join it again.")

    async def _listed(self, link: PcLink) -> None:
        """desk/* offers every tool any PC offers: a stock PC has fewer than a RigDesk PC, and refuses the ones it lacks."""
        body = {"jsonrpc": "2.0", "id": 1, "method": "tools/list"}
        async with self._client().post(f"{link.spec.url}/mcp", json=body, headers=self._headers(link), timeout=aiohttp.ClientTimeout(total=15)) as answer:
            listed = (await answer.json()).get("result", {}).get("tools", [])
        fresh = {tool["name"]: tool for tool in listed if isinstance(tool, dict) and isinstance(tool.get("name"), str) and tool["name"] not in NATIVE}
        merged = self._catalog | fresh
        if fresh and merged != self._catalog:
            self._catalog = merged
            self._revision += 1
            await asyncio.to_thread(self._write_state)

    def _reached(self, link: PcLink, down: str | None) -> None:
        """A bridge says whether its stock PC answers: the bridge may be up while the PC is off, and the PC is what counts."""
        if down is None:
            if link.state == "down":
                _LOG.info("rigdesk_pc_up", f"RigDesk PC {link.name} answers again", pc=link.name)
            link.state, link.why, link.down_since_ms = "up", None, None
            self._linked(link)
            return
        if link.state != "down":
            link.down_since_ms = now_ms()
            _LOG.warning("rigdesk_pc_down", f"RigDesk PC {link.name} is down: {down}", pc=link.name)
            self._resync(link)
        link.state, link.why = "down", down
        self._linked(link)

    async def _event(self, link: PcLink, event: dict[str, Any]) -> None:
        pc, kind, event_id = link.name, event.get("type"), int(event.get("id") or 0)
        if kind == "reach":
            down = event.get("down")
            self._reached(link, down if isinstance(down, str) else None)
        standing = dict(link.standing or {})
        if kind == "claimed":
            standing |= {"owner": event.get("owner"), "holder": "agent"}
        elif kind == "holder":
            standing |= {"holder": event.get("holder"), "since_ms": event.get("at_ms")}
        elif kind == "revoked":
            standing |= {"owner": None, "holder": "human"}
        link.standing = standing
        held = self._sessions.get(pc)
        if kind == "revoked" and held is not None and held.agent == event.get("agent_id"):
            self._sessions.pop(pc, None)
            self._resync(link)
        if kind == "server":
            self._resync(link, None if event.get("found") else (str(event.get("name") or ""), str(event.get("why") or "it stopped")))
        if kind == "mcp":
            self._resync(link)
        if kind == "shell_switch" and link.spec.lane_token_file is None:
            link.shell = {"open": event.get("open") is True, "code": event.get("code"), "words": event.get("words")}
        if kind == "health":
            TASKS.spawn(f"rigdesk-samples-{link.name}", self._catch_up(link, int(event.get("sample") or 0)))
        if kind == "offer":
            prompts.answered(self, link, event)
        if held is not None and (kind != "revoked" or held.agent == event.get("agent_id")):
            key = f"{self._boots.get(pc, '')}:{event_id}" if event_id else f"{self._boots.get(pc, '')}:gap:{now_ms()}"
            if kind == "message":
                if await self._message(link, held, event, key):
                    self._unsent.get(pc, {}).pop(event_id, None)
                elif event_id and self._sessions.get(pc) is held:
                    self._unsent.setdefault(pc, {})[event_id] = event
                elif event_id:
                    _LOG.warning("rigdesk_message_unsent", f"RigDesk {pc}: a message from the human was never told: the session it was for ended", pc=pc)
            elif kind == "holder" and event.get("by") == "stop" and now_ms() - int(event.get("at_ms") or 0) < STOP_FRESH_MS:
                await self._interrupt(held)
            elif (said := words_of(pc, event)) is not None:
                await (self._ended(held, said) if kind == "revoked" else self._tell(held, said, key))
        if event_id:
            self._seen[pc] = max(self._seen.get(pc, 0), event_id)
            self._resume_at(pc)
            await asyncio.to_thread(self._write_state)

    def _resume_at(self, pc: str) -> None:
        """The event id a reconnect resumes after: the newest handled, but never past a message its agent was not told."""
        unsent = self._unsent.get(pc)
        self._last[pc] = min([max(self._last.get(pc, 0), self._seen.get(pc, 0))] if not unsent else [self._seen.get(pc, 0), min(unsent) - 1])

    # --- health samples, read after the last held, judged here ---

    async def _catch_up(self, link: PcLink, told: int | None) -> None:
        """Reads the PC's samples after the newest held: on each reconnect, and on each health event. A sample id at or under the newest
        held means the PC's ids began again (a reinstall), so its whole day is read and kept by its clock. A Linux PC takes them on its
        management lane, as root."""
        async with link.sampling:
            newest = self._history.newest(link.name)
            after = 0 if newest is None or (told is not None and told <= newest.id) else newest.id
            try:
                url, headers = self._shell_at(link)
                async with self._client().get(f"{url}/samples?after={after}", headers=headers,
                                              timeout=aiohttp.ClientTimeout(total=SAMPLES_LIMIT_S)) as answer:
                    if answer.status == 404:
                        link.samples_why = "this PC's RigDesk takes no health samples: update RigDesk there"
                        return
                    if answer.status != 200:
                        raise DeskRefused(f"/samples answered HTTP {answer.status}")
                    said = await answer.json(content_type=None)
            except (DeskRefused, aiohttp.ClientError, TimeoutError, OSError, ValueError) as failed:
                link.samples_why = f"its health samples could not be read: {failure(failed)}"
                _LOG.warning("rigdesk_samples_unread", f"RigDesk {link.name}: {link.samples_why}", pc=link.name)
                return
            samples, skipped = samples_of(said)
            link.samples_why = f"{len(skipped)} samples did not read: {skipped[0]}" if skipped else None
            await asyncio.to_thread(self._history.add, link.name, samples, now_ms())
        await self._judge()

    def judged(self, pc: str) -> Health | None:
        """The PC's health now, from its newest sample; None while Rig holds none for it."""
        link, newest = self._pcs.get(pc), self._history.newest(pc)
        if link is None or newest is None:
            return None
        return health(newest, self._thresholds, now_ms(), link.state == "up")

    async def _judge(self) -> None:
        """Each PC's health as judged now, told to Rig, which holds it, publishes a change of it and prints it in rig status; a PC Rig holds
        no sample of is not told."""
        for pc, link in sorted(self._pcs.items()):
            if (judged := self.judged(pc)) is not None:
                self.host.report(PC_HEALTH, pc, judged.state, judged.said if judged.failing else "every check passes", {"client": link.spec.client})

    def history(self, pc: str) -> list[Any]:
        return list(self._history.of(pc))

    # --- the agent shell, which needs no session ---

    async def shell(self, pc: str, agent: str, graph: str, title: str, asked: dict[str, Any]) -> tuple[int, dict[str, Any]]:
        """One request to a RigDesk PC's agent shell, as the calling agent: the PC's HTTP status and its answer, {ok, text} or {error, code}."""
        link = self._link(pc)
        self._shell_reachable(link)
        timeout_s = asked.get("timeout_s")
        limit_s = CALL_LIMIT_S + (timeout_s if isinstance(timeout_s, int) and 0 < timeout_s <= SHELL_LIMIT_S else 60)
        body = {"agent": {"id": agent, "title": title, "graph": graph}} | asked
        url, headers = self._shell_at(link)
        async with self._client().post(f"{url}/shell", json=body, headers=headers, timeout=aiohttp.ClientTimeout(total=limit_s)) as answer:
            said = await answer.json(content_type=None)
            return answer.status, _object(said)

    async def shell_ran(self, pc: str) -> list[dict[str, Any]]:
        """The PC's newest agent-shell commands from its own journal, newest first: who ran what, as whom, and how it ended."""
        link = self._link(pc)
        self._shell_reachable(link)
        url, headers = self._shell_at(link)
        async with self._client().get(f"{url}/log?n={SHELL_LOG_READ}", headers=headers, timeout=aiohttp.ClientTimeout(total=15)) as answer:
            if answer.status != 200:
                raise DeskRefused(f"{pc}'s journal answered HTTP {answer.status}")
            entries = _objects(_object(await answer.json(content_type=None)).get("entries"))
        ran = [entry for entry in entries if entry.get("what") == "shell"][-SHELL_LOG_SHOWN:]
        return [{"at_ms": int(entry.get("at_ms") or 0), "who": str(entry.get("who") or ""), "graph": _object(entry.get("detail")).get("graph"),
                 "as": _object(entry.get("detail")).get("as"), "command": str(_object(entry.get("detail")).get("command") or ""),
                 "outcome": str(entry.get("outcome") or ""), "took_ms": _object(entry.get("detail")).get("took_ms"), "job": _object(entry.get("detail")).get("job")}
                for entry in reversed(ran)]

    async def _resend(self) -> None:
        """The human's messages that could not be told (the agent's graph stopped, its seat closed) are told again while its session lasts."""
        for pc, unsent in list(self._unsent.items()):
            link, held = self._pcs.get(pc), self._sessions.get(pc)
            if link is None or held is None:
                _LOG.warning("rigdesk_message_unsent", f"RigDesk {pc}: {len(unsent)} of the human's messages were never told: the session they were for ended", pc=pc)
                unsent.clear()
            for event_id, event in sorted(unsent.items()):
                if link is None or held is None or link.state != "up":
                    break
                if not await self._message(link, held, event, f"{self._boots.get(pc, '')}:{event_id}"):
                    break
                unsent.pop(event_id)
            if not unsent:
                self._unsent.pop(pc, None)
            before = self._last.get(pc)
            self._resume_at(pc)
            if self._last.get(pc) != before:
                await asyncio.to_thread(self._write_state)

    async def _ended(self, held: Held, words: str) -> None:
        """An agent's session ended without its asking: it is sent nothing, and its next desk call's refusal gives these words."""
        self._ended_why[(held.agent, held.graph)] = f"{words} (It ended at {_clock(now_ms())}.)"

    def ended_why(self, agent: str, graph: str) -> str | None:
        """Why this agent's last session ended without its asking, while it has not joined again."""
        return self._ended_why.get((agent, graph))

    async def _message(self, link: PcLink, held: Held, event: dict[str, Any], key: str) -> bool:
        images: list[tuple[str, Path | None]] = []
        for image in event.get("attachments") or []:
            try:
                images.append((image, await self._fetched(link, held, str(image))))
            except DeskRefused:
                images.append((image, None))
        if not await self._tell(held, message_words(link.name, str(event.get("text") or ""), images), key):
            return False
        ack = f"{link.spec.url}/attachments/-/ack?upto={int(event.get('id') or 0)}"
        async with self._client().post(ack, headers=self._headers(link, held.session), timeout=aiohttp.ClientTimeout(total=15)):
            pass
        return True

    async def _seat(self, held: Held) -> SeatId:
        seat = await self.host.seat_of(AgentId(held.agent), GraphName(held.graph), "rigdesk")
        return SeatId(GraphName(held.graph), held.node) if isinstance(seat, RefusedOutcome) else seat  # type: ignore[arg-type]

    async def _interrupt(self, held: Held) -> None:
        """Stop ends the owning agent's turn at once, as the operator's interrupt does; an agent between turns has none to end."""
        seat = await self._seat(held)
        kernel = self.host.kernel(seat)
        if kernel is None:
            _LOG.warning("rigdesk_stop_unseated", f"RigDesk {held.pc}: Stop found no running agent at {held.graph}/{held.node} to interrupt", pc=held.pc)
            return
        try:
            done = await kernel.inject(seat, "interrupt", None, None, None, "interrupt")
        except Exception as failed:  # noqa: BLE001
            _LOG.warning("rigdesk_stop_failed", f"RigDesk {held.pc}: interrupting {held.graph}/{held.node} failed: {failure(failed)}", pc=held.pc)
            return
        if isinstance(done, RefusedOutcome):
            _LOG.warning("rigdesk_stop_refused", f"RigDesk {held.pc}: {held.graph}/{held.node} was not interrupted: {done.rendered}", pc=held.pc)

    async def _tell(self, held: Held, words: str, key: str, timing: Literal["steer", "queue"] = "steer") -> bool:
        seat = await self._seat(held)
        try:
            verdict = await self.host.admit(seat, PluginWords(words, f"rigdesk:{held.pc}", key=f"{held.pc}:{key}", timing=timing, headline=f"RigDesk {held.pc}"))
        except Exception as failed:  # noqa: BLE001
            _LOG.warning("rigdesk_tell_failed", f"RigDesk {held.pc}: telling {held.graph}/{held.node} failed: {failure(failed)}", pc=held.pc)
            return False
        if isinstance(verdict, RefusedOutcome):
            _LOG.warning("rigdesk_tell_refused", f"RigDesk {held.pc}: {held.graph}/{held.node} was not told: {verdict.rendered}", pc=held.pc)
            return False
        return True

    # --- the PC's own MCP servers, for the agent holding it ---

    def _resync(self, link: PcLink, gone: tuple[str, str] | None = None) -> None:
        TASKS.spawn(f"rigdesk-servers-{link.name}", self._sync_servers(link, gone))

    async def _sync_servers(self, link: PcLink, gone: tuple[str, str] | None) -> None:
        """Makes the holder's view of the PC's MCP servers current: each new one opened under its session, each gone one dropped, and the holder
        told of both. Rig's switch turned off closes the holder's servers and tells it; with no holder, or a new one, those opened before go unsaid."""
        async with link.syncing:
            held = self._sessions.get(link.name)
            stale = [name for name, server in link.servers.items() if held is None or not link.spec.mcp or link.state == "down" or server.session != held.session]
            closed = [server for server in (link.servers.pop(name) for name in stale) if held is not None and server.session == held.session]
            if held is None or not link.spec.mcp or link.state == "down":
                link.mcp_on, link.mcp_why = None, (None if link.spec.mcp else "off in Rig's settings for this PC") if link.state != "down" else f"the PC is down: {link.why}"
                self._revision += bool(stale)
                if held is not None and closed:
                    names = ", ".join(f"{server.key}/*" for server in closed)
                    why = (f"{link.name} is down ({link.why}), so its MCP servers closed for you: {names} are not there until it is back"
                           if link.state == "down" else f"the operator turned MCP off for {link.name} in Rig, so its MCP servers closed for you: {names} are no longer there")
                    await self._tell(held, f"RigDesk {link.name}: {why}.", f"servers-off:{now_ms()}", "queue")
                return
            dropped: list[tuple[PcServer, str]] = []
            if gone is not None and (server := link.servers.pop(gone[0], None)) is not None:
                dropped.append((server, gone[1]))
            try:
                listed = await self._listed_servers(link)
            except (DeskRefused, aiohttp.ClientError, TimeoutError, OSError, ValueError) as failed:
                link.mcp_on, link.mcp_why = None, f"its MCP servers could not be listed: {failed}"
                listed = {name: server.says for name, server in link.servers.items()}
            dropped += [(link.servers.pop(name), "the PC no longer offers it") for name in [name for name in link.servers if name not in listed]]
            for server, _ in dropped:
                link.server_faults.pop(server.name, None)
            opened: list[PcServer] = []
            for name, says in listed.items():
                if name in link.servers:
                    continue
                try:
                    server = await self._open_server(link, held, name, says)
                except (DeskRefused, aiohttp.ClientError, TimeoutError, OSError, ValueError) as failed:
                    link.mcp_why = f"MCP server {name} could not be opened: {failed}"
                    _LOG.warning("rigdesk_server_unopened", f"RigDesk {link.name}: {link.mcp_why}", pc=link.name)
                    continue
                link.servers[name] = server
                link.server_faults.pop(name, None)
                opened.append(server)
            self._revision += bool(stale or dropped or opened)
        for server, why in dropped:
            await self._tell(held, f"RigDesk {link.name}: the MCP server {server.name} is gone ({why}); its tools {server.key}/* are no longer there.",
                             f"server-gone:{server.name}:{server.since_ms}", "queue")
        for server in opened:
            await self._tell(held, self._server_words(server), f"server:{server.name}:{server.since_ms}", "queue")

    @staticmethod
    def _server_words(server: PcServer) -> str:
        names = ", ".join(f"{server.key}/{tool['name']}" for tool in server.tools) or "none yet"
        return (f"RigDesk {server.pc}: the MCP server {server.name} ({server.says}) runs there, and its tools are yours while you hold {server.pc}, "
                f"by these exact names (tool_load gives one in full, call_tool runs it): {names}. A call acts on the PC as your desk calls do: "
                "the person there goes first, and Block refuses it.")

    async def _listed_servers(self, link: PcLink) -> dict[str, str]:
        """The servers the PC offers now, by name, with their own words; a RigDesk without the route, or a bridge, offers none."""
        async with self._client().get(f"{link.spec.url}/servers", headers=self._headers(link), timeout=aiohttp.ClientTimeout(total=MCP_LIMIT_S)) as answer:
            if answer.status == 404:
                link.mcp_on, link.mcp_why = False, "this PC's RigDesk is older than MCP servers"
                return {}
            if answer.status != 200:
                raise DeskRefused(f"/servers answered HTTP {answer.status}")
            said = _object(await answer.json(content_type=None))
        link.mcp_on = said.get("on") is True
        link.mcp_why = said.get("why") if isinstance(said.get("why"), str) else None if link.mcp_on else "MCP is off on the PC, by its own switch"
        listed: dict[str, str] = {}
        for server in _objects(said.get("servers")):
            name = server.get("name")
            if not isinstance(name, str) or not MCP_KEY.match(f"{link.name}.{name}"):
                _LOG.warning("rigdesk_server_unnamed", f"RigDesk {link.name}: skipped an MCP server whose name {name!r} cannot be a Rig server name", pc=link.name)
                continue
            listed[name] = str(server.get("says") or name)
        return listed

    async def _open_server(self, link: PcLink, held: Held, name: str, says: str) -> PcServer:
        """MCP's opening: initialize, initialized, then every page of tools/list, all under the holder's PC session."""
        hello = {"protocolVersion": MCP_PROTOCOL, "capabilities": {}, "clientInfo": {"name": "rig-rigdesk", "version": "0.1.0"}}
        reply, sid = await self._mcp_post(link, held, name, None, MCP_PROTOCOL, "initialize", hello, MCP_LIMIT_S)
        protocol = str(reply.get("protocolVersion") or MCP_PROTOCOL)
        await self._mcp_post(link, held, name, sid, protocol, "notifications/initialized", None, MCP_LIMIT_S)
        tools: list[dict[str, Any]] = []
        cursor: str | None = None
        for _ in range(20):
            page, _ = await self._mcp_post(link, held, name, sid, protocol, "tools/list", {"cursor": cursor} if cursor else {}, MCP_LIMIT_S)
            tools += [tool for tool in _objects(page.get("tools")) if isinstance(tool.get("name"), str) and MCP_TOOL.match(tool["name"])]
            cursor = page.get("nextCursor") if isinstance(page.get("nextCursor"), str) else None
            if cursor is None:
                break
        return PcServer(link.name, name, says, tuple(tools), sid, protocol, held.session, now_ms())

    async def _mcp_post(self, link: PcLink, held: Held, name: str, sid: str | None, protocol: str, method: str, params: dict[str, Any] | None,
                        limit_s: float) -> tuple[dict[str, Any], str | None]:
        """One MCP message through the PC to its server: the result, and the server's MCP session id; a notification answers an empty result.
        A streamable HTTP server answers a request as JSON or as an event stream holding the reply."""
        notice = method.startswith("notifications/")
        asked = next(self._ids)
        body: dict[str, Any] = {"jsonrpc": "2.0", "method": method} | ({} if notice else {"id": asked}) | ({} if params is None else {"params": params})
        headers = self._headers(link, held.session) | {"Accept": "application/json, text/event-stream", "MCP-Protocol-Version": protocol}
        headers |= {"Mcp-Session-Id": sid} if sid else {}
        async with self._client().post(f"{link.spec.url}/servers/{name}/mcp", json=body, headers=headers, timeout=aiohttp.ClientTimeout(total=limit_s)) as answer:
            text = await answer.text()
            sid = answer.headers.get("Mcp-Session-Id") or sid
            if answer.status == 404 and sid and "no MCP server" not in text:
                raise McpSessionGone(text[:300])
            if answer.status not in (200, 202):
                refused = McpAnswered if answer.status in (403, 404) else DeskRefused
                raise refused(f"{link.name}.{name} answered HTTP {answer.status}" + (f": {text[:300]}" if text else ""))
            streamed = answer.content_type == "text/event-stream"
        if notice:
            return {}, sid
        replies = [json.loads(line[5:].strip()) for line in text.splitlines() if line.startswith("data:") and line[5:].strip()] if streamed else [json.loads(text)]
        reply = next((each for each in _objects(replies) if each.get("id") == asked), None)
        if reply is None:
            raise DeskRefused(f"{link.name}.{name} sent no answer to {method}")
        if "error" in reply:
            raise McpAnswered(f"{link.name}.{name}: {_object(reply['error']).get('message', 'error')}")
        return _object(reply.get("result")), sid

    async def mcp_call(self, held: Held, key: str, tool: str, arguments: dict[str, Any]) -> dict[str, Any]:
        """A tool of one of the held PC's MCP servers; a server that forgot its MCP session is opened again once, and the call retried."""
        link = self._link(held.pc)
        held.used_ms = now_ms()
        server = link.servers.get(key.removeprefix(f"{held.pc}."))
        if server is None or server.key != key or server.session != held.session:
            raise DeskRefused(f"{key} is not open on {held.pc} now; the MCP servers there reach you when they run")
        params = {"name": tool, "arguments": arguments}
        try:
            try:
                result, _ = await self._mcp_post(link, held, server.name, server.sid, server.protocol, "tools/call", params, MCP_CALL_LIMIT_S)
            except McpSessionGone:
                async with link.syncing:
                    server = link.servers[server.name] = await self._open_server(link, held, server.name, server.says)
                result, _ = await self._mcp_post(link, held, server.name, server.sid, server.protocol, "tools/call", params, MCP_CALL_LIMIT_S)
        except McpAnswered:
            raise
        except (DeskRefused, aiohttp.ClientError, TimeoutError, OSError) as failed:
            words = str(failed) if isinstance(failed, DeskRefused) else f"MCP server {server.name} on {held.pc} did not answer {tool}: {failure(failed)}"
            link.server_faults[server.name] = (words, now_ms())
            self._revision += 1
            raise DeskRefused(words) from failed
        if link.server_faults.pop(server.name, None) is not None:
            self._revision += 1
        return await self._answered(held.pc, f"{server.name}-{tool}", result) | {"server": server.name}

    async def _answered(self, pc: str, what: str, result: dict[str, Any]) -> dict[str, Any]:
        """An MCP tool result as the agent reads it: text joined, images kept here as files, any other block as its JSON."""
        texts: list[str] = []
        images: list[dict[str, Any]] = []
        for index, block in enumerate(_objects(result.get("content"))):
            if block.get("type") == "text":
                texts.append(str(block.get("text", "")))
            elif block.get("type") == "image" and isinstance(block.get("data"), str):
                try:
                    data = base64.b64decode(block["data"], validate=False)
                except binascii.Error:
                    continue
                path = await asyncio.to_thread(self._kept, "shots", f"{pc}-{what}-{now_ms()}-{index}.png", data)
                images.append({"file": str(path), "bytes": len(data)})
            else:
                texts.append(json.dumps(block))
        structured = result.get("structuredContent")
        return {"pc": pc, "text": "\n".join(texts), "images": images, "is_error": result.get("isError") is True} | ({"structured": structured} if structured is not None else {})

    # --- files ---

    def _kept(self, folder: str, name: str, data: bytes) -> Path:
        where = self._dir / folder
        where.mkdir(mode=0o700, parents=True, exist_ok=True)
        path = where / name
        replace_file(path, data, new_mode=0o600)
        kept = sorted(where.iterdir(), key=lambda file: file.stat().st_mtime)
        for old in kept[:-FILES_KEPT]:
            old.unlink(missing_ok=True)
        return path

    async def _fetched(self, link: PcLink, held: Held, image: str) -> Path:
        if not re.fullmatch(r"[A-Za-z0-9_-]{1,64}", image):
            raise DeskRefused(f"{image!r} is not an attachment id")
        local = self._dir / "attachments" / f"{link.name}-{image}.png"
        if local.exists():
            return local
        async with self._client().get(f"{link.spec.url}/attachments/{image}", headers=self._headers(link, held.session), timeout=aiohttp.ClientTimeout(total=30)) as answer:
            if answer.status != 200:
                raise DeskRefused(f"the PC has no image {image}: it answered HTTP {answer.status}")
            data = await answer.read()
        return await asyncio.to_thread(self._kept, "attachments", f"{link.name}-{image}.png", data)

    # --- what the tools ask ---

    def catalog(self) -> dict[str, dict[str, Any]]:
        return self._catalog

    def tools(self, build: Callable[[Desks], tuple[Any, ...]]) -> tuple[Any, ...]:
        """Rebuilt when the desk catalog or any PC's MCP servers move, each of which bumps the revision."""
        if self._tools_of != self._revision:
            self._tools, self._tools_of = build(self), self._revision
        return self._tools

    def pc_servers(self) -> tuple[PcServer, ...]:
        return tuple(server for link in self._pcs.values() for server in link.servers.values())

    def pc_server(self, key: str) -> PcServer | None:
        return next((server for server in self.pc_servers() if server.key == key), None)

    @property
    def for_fleet(self) -> bool:
        return self._for_fleet

    def held_by(self, agent: str, graph: str) -> Held | None:
        return next((held for held in self._sessions.values() if held.agent == agent and held.graph == graph), None)

    def _link(self, pc: str) -> PcLink:
        link = self._pcs.get(pc)
        if link is None:
            raise DeskRefused(f"RigDesk PC {pc} is no longer set up in Rig")
        return link

    async def join(self, link_text: str, agent: str, graph: str, node: str, title: str, via: Literal["link", "assigned"] = "link", approved: bool = False) -> Held:
        invite = LINK.match(link_text.strip())
        device = None if invite is None else INVITE_HOST.match(invite["host"])
        if invite is not None and device is not None:
            return await self._join_invite(device["id"], Invited(invite["grant"], invite["secret"]), agent, graph, node, title)
        settings = await asyncio.to_thread(self.host.settings, RigdeskSettings)
        found = pc_of_link(link_text, dict(settings.pcs), {name: link.host for name, link in self._pcs.items() if link.host})
        if isinstance(found, str):
            raise DeskRefused(found)
        pc, grant, secret = found
        mine = self.held_by(agent, graph)
        if mine is not None:
            raise DeskRefused(f"you already own the session on {mine.pc}; desk/leave it before joining another" if mine.pc != pc else f"you already own the session on {pc}")
        self._refuse_if_invited(pc)
        link = self._link(pc)
        if not approved and self.shell_granted(pc) and await self._invite_open(pc):
            raise InviteAsk(pc)
        body = {"grant": grant, "secret": secret, "agent": {"id": agent, "title": title, "graph": graph}}
        async with self._client().post(f"{link.spec.url}/claim", json=body, headers=self._headers(link), timeout=aiohttp.ClientTimeout(total=15)) as answer:
            said = await answer.json(content_type=None)
            if answer.status != 200:
                raise DeskRefused(f"{pc} refused the link: {said.get('error', f'HTTP {answer.status}') if isinstance(said, dict) else answer.status}")
        if not isinstance(said, dict) or not isinstance(said.get("session"), str):
            raise DeskRefused(f"{pc} accepted the link but sent no session token")
        replaced = self._sessions.get(pc)
        held = self._sessions[pc] = Held(pc, agent, graph, node, title, said["session"], now_ms(), via)
        self._ended_why.pop((agent, graph), None)
        link.standing = said.get("standing")
        await asyncio.to_thread(self._write_state)
        self._resync(link)
        if replaced is not None and replaced.agent != agent:
            _LOG.info("rigdesk_session_ended", f"RigDesk {pc}: shared with {title}, so {replaced.title}'s session there ended", pc=pc)
            await self._ended(replaced, f"RigDesk {pc}: the desktop was shared with {title}, so your session there ended.")
        return held

    def assigned_pcs(self, agent: str, graph: str) -> tuple[str, ...]:
        """The PCs this agent, or its graph, holds a standing grant to, by name."""
        return tuple(sorted(name for name in self._pcs if _grants(self.granted(name), agent, graph)))

    async def _unassigned(self, pc: str) -> None:
        """The operator took the grant away: its session ends now, and the revoke event gives the agent's next desk call why."""
        try:
            await self.viewer(pc, "revoke")
        except (DeskRefused, aiohttp.ClientError, TimeoutError, OSError) as failed:
            _LOG.warning("rigdesk_unassign_failed", f"RigDesk {pc}: ending the unassigned session failed: {failed}", pc=pc)

    async def _ungranted(self, held: Held) -> None:
        """The operator took a RigDesk PC's grant away: the session it started ends now, and its agent's next desk call says why, since no viewer revokes it."""
        try:
            await self.leave(held)
        except (DeskRefused, aiohttp.ClientError, TimeoutError, OSError) as failed:
            _LOG.warning("rigdesk_unassign_failed", f"RigDesk {held.pc}: ending the unassigned session failed: {failed}", pc=held.pc)
        await self._ended(held, f"RigDesk {held.pc}: the operator took away your standing access to {held.pc}, so your session there ended. "
                                "desk/request(pc, why) asks them again.")

    async def join_approved(self, pc: str, agent: str, graph: str, node: str, title: str, ask: str = "", via: Literal["link", "assigned"] = "link") -> Held:
        """The operator accepted this agent's request to join a PC: on a stock PC a share link is minted and claimed for it, as the human would;
        a RigDesk PC is asked for the session itself."""
        mine = self.held_by(agent, graph)
        if mine is not None:
            raise DeskRefused(f"{title} already owns the session on {mine.pc}; it must desk/leave it first")
        other = self._sessions.get(pc)
        if other is not None and other.agent != agent:
            raise DeskRefused(f"{other.title} holds {pc} now")
        self._refuse_if_invited(pc)
        if self._link(pc).spec.stock is None:
            return await self._join_operator(pc, agent, graph, node, title, ask, via)
        self.person_answered.discard(ask)
        minted = await self.viewer(pc, "mint")
        return await self.join(minted["link"], agent, graph, node, title)

    async def join_switch(self, pc: str) -> dict[str, Any] | None:
        """A RigDesk PC's switch for sessions the operator starts, read from it now: {open, code, words}; None for a RigDesk without /join."""
        link = self._link(pc)
        async with self._client().get(f"{link.spec.url}/health", headers=self._headers(link), timeout=aiohttp.ClientTimeout(total=15)) as answer:
            if answer.status != 200:
                raise DeskRefused(f"{pc}'s /health answered HTTP {answer.status}")
            health = _object(await answer.json(content_type=None))
        link.join = _object(health["join"]) if isinstance(health.get("join"), dict) else None
        return link.join

    async def _invite_open(self, pc: str) -> bool:
        """Whether the grant open on a RigDesk PC now is an invite its person made, read from its /health; a build without invites says nothing, so False."""
        link = self._link(pc)
        async with self._client().get(f"{link.spec.url}/health", headers=self._headers(link), timeout=aiohttp.ClientTimeout(total=15)) as answer:
            if answer.status != 200:
                raise DeskRefused(f"{pc}'s /health answered HTTP {answer.status}")
            return _object(await answer.json(content_type=None)).get("invite_open") is True

    async def _join_operator(self, pc: str, agent: str, graph: str, node: str, title: str, ask: str, via: Literal["link", "assigned"] = "link") -> Held:
        """Rig's bearer asks a RigDesk PC for its session for this agent, with no share link; the PC's own switch admits it."""
        link = self._link(pc)
        body = {"agent": {"id": agent, "title": title, "graph": graph}, "ask": ask}
        if ask in self.person_answered:
            self.person_answered.discard(ask)
            body["answered_by"] = "person"
        async with self._client().post(f"{link.spec.url}/join", json=body, headers=self._headers(link), timeout=aiohttp.ClientTimeout(total=15)) as answer:
            said = _object(await answer.json(content_type=None)) if answer.content_type == "application/json" else {}
            if answer.status == 404 and said.get("code") != "no_join":
                raise DeskRefused(f"{pc}'s RigDesk is too old for sessions started from Rig (it has no /join); update it, or ask the human there for a share link")
            if answer.status != 200:
                raise DeskRefused(f"{pc} refused the session: {said.get('error') or f'HTTP {answer.status}'}")
        session = said.get("session")
        if not isinstance(session, str):
            raise DeskRefused(f"{pc} accepted the session but sent no session token")
        held = self._sessions[pc] = Held(pc, agent, graph, node, title, session, now_ms(), via)
        self._ended_why.pop((agent, graph), None)
        link.standing = said.get("standing")
        await asyncio.to_thread(self._write_state)
        self._resync(link)
        return held

    async def join_assigned(self, pc: str, agent: str, graph: str, node: str, title: str, ask: str = "standing grant") -> Held:
        """Joins a PC this agent is assigned, as a share link would, unless another agent holds it now; a RigDesk PC is asked for the session itself,
        and journals ask."""
        if pc not in self.assigned_pcs(agent, graph):
            raise DeskRefused(f"{pc} is not assigned to you: ask the human there for a share link, or the operator with desk/request(pc, why)")
        mine = self.held_by(agent, graph)
        if mine is not None:
            raise DeskRefused(f"you already own the session on {mine.pc}; desk/leave it before joining another" if mine.pc != pc else f"you already own the session on {pc}")
        other = self._sessions.get(pc)
        if other is not None and other.agent != agent:
            frees = f"they leave or after {IDLE_LEAVE_MS // 60_000} minutes without a desk call" if other.via == "assigned" else "they leave or the human ends that share link"
            raise DeskRefused(f"{pc} is assigned to you, but {other.title} holds it now; it frees when {frees}")
        self._refuse_if_invited(pc)
        if self._link(pc).spec.stock is None:
            return await self._join_operator(pc, agent, graph, node, title, ask, via="assigned")
        minted = await self.viewer(pc, "mint")
        return await self.join(minted["link"], agent, graph, node, title, via="assigned")

    async def _idle_leaves(self) -> None:
        """A session held through a standing grant ends after IDLE_LEAVE_MS without a desk call, so other assigned agents can take the PC; it joins again with desk/join(pc)."""
        cutoff = now_ms() - IDLE_LEAVE_MS
        for held in [held for held in self._sessions.values() if held.via == "assigned" and max(held.used_ms, held.since_ms) < cutoff]:
            try:
                await self.leave(held)
            except (DeskRefused, aiohttp.ClientError, TimeoutError, OSError) as failed:
                _LOG.warning("rigdesk_idle_leave_failed", f"RigDesk {held.pc}: leaving the idle session failed: {failed}", pc=held.pc)
            _LOG.info("rigdesk_session_ended", f"RigDesk {held.pc}: {held.title} left after {IDLE_LEAVE_MS // 60_000} idle minutes", pc=held.pc)
            await self._ended(held, f"RigDesk {held.pc}: you left {held.pc} after {IDLE_LEAVE_MS // 60_000} minutes without a desk call, "
                                    "so other agents assigned to it can use it.")

    async def leave(self, held: Held) -> bool:
        if held.pc in self._adhoc:
            await self._adhoc_closed(held.pc, None)
            return True
        if self._sessions.get(held.pc) is held:
            self._sessions.pop(held.pc)
            self._unsent.pop(held.pc, None)
        await asyncio.to_thread(self._write_state)
        link = self._pcs.get(held.pc)
        if link is None:
            return True
        self._resync(link)
        async with self._client().post(f"{link.spec.url}/leave", headers=self._headers(link, held.session), timeout=aiohttp.ClientTimeout(total=15)) as answer:
            return answer.status == 200 and bool((await answer.json(content_type=None)).get("left"))

    async def attachment(self, held: Held, image: str) -> Path:
        return await self._fetched(self._link(held.pc), held, image)

    async def call(self, held: Held, tool: str, arguments: dict[str, Any]) -> dict[str, Any]:
        link = self._link(held.pc)
        held.used_ms = now_ms()
        if link.state != "up":
            raise DeskRefused(f"RigDesk PC {held.pc} is down: {link.why or 'not reached yet'}")
        body = {"jsonrpc": "2.0", "id": next(self._ids), "method": "tools/call", "params": {"name": tool, "arguments": arguments}}
        async with self._client().post(f"{link.spec.url}/mcp", json=body, headers=self._headers(link, held.session), timeout=aiohttp.ClientTimeout(total=_call_limit_s(tool, arguments))) as answer:
            if answer.status != 200:
                raise DeskRefused(f"{held.pc} answered HTTP {answer.status}")
            reply = await answer.json()
        if "error" in reply:
            raise DeskRefused(f"{held.pc}: {reply['error'].get('message', 'error')}")
        return await self._answered(held.pc, tool, reply.get("result", {}))

    async def viewer(self, pc: str, verb: str) -> dict[str, Any]:
        """The human's share and holder buttons for a stock PC, which has no RigDesk viewer: Rig's page stands in for it."""
        link = self._link(pc)
        if link.spec.stock is None:
            raise DeskRefused(f"{pc} runs RigDesk: its human shares and takes input back in the RigDesk viewer")
        if verb not in VIEWER_VERBS:
            raise DeskRefused(f"no viewer verb {verb!r}")
        async with self._client().post(f"{link.spec.url}/viewer/{verb}", headers=self._headers(link), timeout=aiohttp.ClientTimeout(total=15)) as answer:
            if answer.status != 200:
                raise DeskRefused(f"{pc}'s rigdesk-bridge answered HTTP {answer.status}")
            said = await answer.json(content_type=None)
        if verb == "mint":
            return {"link": f"rigdesk://{pc}/{said['grant']}#{said['secret']}"}
        link.standing = said.get("standing") or link.standing
        return {"changed": bool(said.get("changed"))}

    async def frame(self, pc: str) -> bytes:
        """A PNG of the PC's screen now, for the operator; the PC journals it."""
        link = self._link(pc)
        if link.state != "up":
            raise DeskRefused(f"RigDesk PC {pc} is down: {link.why or 'not reached yet'}")
        async with self._client().get(f"{link.spec.url}/frame", headers=self._headers(link), timeout=aiohttp.ClientTimeout(total=30)) as answer:
            if answer.status != 200:
                said = (await answer.text())[:300]
                raise DeskRefused(f"{pc} gave no screenshot: HTTP {answer.status}" + (f": {said}" if said else "")
                                  + ("; its RigDesk is older than the /frame route" if answer.status == 404 else ""))
            return await answer.read()

    # --- what the page and pin show ---

    def pcs(self) -> list[dict[str, Any]]:
        rows = []
        for name, link in sorted(self._pcs.items(), key=lambda item: (item[1].spec.client, item[0])):
            standing = link.standing or {}
            owner = standing.get("owner") or None
            held = self._sessions.get(name)
            rows.append({"name": name, "client": link.spec.client, "url": link.spec.url, "stock": link.spec.stock is not None, "os": link.os, "state": link.state, "why": link.remote_why or link.why,
                         "down_since_ms": link.down_since_ms, "holder": standing.get("holder") if owner else None,
                         "owner": None if owner is None else {"id": owner.get("agent_id"), "title": owner.get("title"), "graph": owner.get("graph")},
                         "owned_here": held is not None and owner is not None and held.agent == owner.get("agent_id"), "assigned": self.granted(name),
                         "shell": None if link.spec.stock is not None else {"switch": link.shell, "granted": self.shell_granted(name)},
                         "health": self._health_row(name, link), "adhoc": self._adhoc_row(name),
                         "password_file": None if link.spec.stock is None or name in self._adhoc else link.spec.stock.password_file,
                         "mcp": {"allowed": link.spec.mcp, "on": link.mcp_on, "why": link.mcp_why,
                                 "servers": [{"name": server.name, "key": server.key, "says": server.says, "tools": [tool["name"] for tool in server.tools],
                                              "since_ms": server.since_ms, "failing": link.server_faults.get(server.name, (None,))[0],
                                              "failing_since_ms": link.server_faults.get(server.name, (None, None))[1]} for server in link.servers.values()]}})
        return rows

    def _health_row(self, name: str, link: PcLink) -> dict[str, Any] | None:
        """A PC's health as its row shows it: the verdict, the newest sample's headline figures and age; None for a stock PC."""
        if link.spec.stock is not None:
            return None
        newest, judged = self._history.newest(name), self.judged(name)
        figures = None if newest is None else newest.figures
        return {"state": None if judged is None else judged.state, "failing": [] if judged is None else [asdict(check) for check in judged.failing],
                "why": link.samples_why, "sample_ms": None if newest is None else newest.at_ms, "rigdesk": None if newest is None else newest.rigdesk,
                "figures": None if figures is None else {
                    "volumes": [{"name": volume.name, "free_pct": round(volume.free_pct, 1), "free_mb": volume.free_mb, "total_mb": volume.total_mb} for volume in figures.volumes],
                    "ram_used_pct": round(100 * (1 - figures.ram.free_mb / figures.ram.total_mb), 1) if figures.ram.total_mb else None,
                    "cpu_pct": figures.cpu.load_pct, "build": figures.os.build, "uptime_s": figures.uptime_s, "pending_reboot": figures.pending_reboot}}

    def top_rights(self, pc: str) -> str:
        """Who the grant level system runs as on this PC, in its own OS's word: root on Linux, SYSTEM on Windows and while it is unknown."""
        link = self._pcs.get(pc)
        return "root" if link is not None and link.os == "linux" else "SYSTEM"

    def entries(self) -> tuple[PcEntry, ...]:
        rows: list[PcEntry] = []
        for name, link in sorted(self._pcs.items()):
            held = self._sessions.get(name)
            rows.append(PcEntry(name=name, client=link.spec.client, stock=link.spec.stock is not None, state=link.state, why=link.remote_why or link.why,
                                assigned=self.granted(name), held_by=None if held is None else PcHolder(agent_id=held.agent, title=held.title, graph=held.graph),
                                shell=link.shell, os=link.os, health=self.judged(name), join=link.join, adhoc=self._adhoc_row(name)))
        return tuple(rows)

    def session_of(self, graph: str, agent: str) -> dict[str, Any] | None:
        held = self.held_by(agent, graph)
        if held is None:
            return None
        link = self._pcs.get(held.pc)
        standing = (link.standing if link is not None else None) or {}
        return {"pc": held.pc, "state": "missing" if link is None else link.state, "holder": standing.get("holder"),
                "since_ms": standing.get("since_ms"), "joined_ms": held.since_ms}


    # --- ad-hoc joins: a PC by its RustDesk ID and the password the operator handed the agent (see adhoc) ---

    def _adhoc_row(self, name: str) -> dict[str, str] | None:
        kept = self._adhoc.get(name)
        return None if kept is None else {"id": kept.id, "via": kept.words}

    @staticmethod
    async def _adhoc_routes(target: adhoc.Target, settings: RigdeskSettings, key_file: Path, busy: set[str]) -> list[adhoc.Route]:
        """The ID server the ID names; or, for a bare ID, each PC of ours on the tailnet that answers and no other agent holds (a RigDesk PC, or
        one reached by peer, such as RigDesk on a phone), then RustDesk's public server."""
        if target.host is not None:
            return [adhoc.Route(("--id", target.id, "--id-server", target.host, "--key-file", str(key_file)), f"the ID server {target.host}")]
        ours = {name: (pc.stock.peer if pc.stock is not None else urlsplit(pc.url).hostname) for name, pc in settings.pcs.items()
                if name not in busy and (pc.stock is None or pc.stock.peer is not None)}
        peers = {name: peer for name, peer in ours.items() if peer}
        up = await asyncio.gather(*(adhoc.answers(*adhoc.host_port(peer)) for peer in peers.values()))
        return [*(adhoc.Route(("--peer", peer, "--login-id", target.id), f"{name} over the tailnet", name) for (name, peer), reached in zip(peers.items(), up, strict=True) if reached),
                adhoc.Route(("--id", target.id, "--id-server", adhoc.PUBLIC_HOST, "--key-file", str(key_file)), f"RustDesk's public server {adhoc.PUBLIC_HOST}")
                if settings.connect_server is None else
                adhoc.Route(("--id", target.id, "--id-server", settings.connect_server.host, "--key-file", str(key_file)), f"the ID server {settings.connect_server.host}")]

    @staticmethod
    def _invite_route(target: adhoc.Target, settings: RigdeskSettings, busy: dict[str, str]) -> adhoc.Route:
        """The one address an invite goes to: the entry declaring the link's ID, whose node the tailnet vouches for. A link can name any ID, so no other PC
        is connected to at all, and without such an entry nothing is."""
        named = {name: pc.stock.peer for name, pc in settings.pcs.items() if pc.stock is not None and pc.stock.peer is not None and pc.stock.login_id == target.id}
        if len(named) != 1:
            found = f"{', '.join(sorted(named))} all declare it, so which device it is cannot be told" if named else "no entry declares it"
            raise DeskRefused(f"{target.id} was not dialed: an invite is redeemed only through the entry in Rig's settings that declares the device's RustDesk ID, and {found}. "
                              f"The operator sets stock.login_id to {target.id} on that device's entry under plugins.rigdesk.pcs, beside its stock.peer; "
                              "an invite still open stays open until it lapses")
        name, peer = next(iter(named.items()))
        if name in busy:
            raise DeskRefused(f"{busy[name]} holds {name} now, so its invite was not redeemed; an invite still open stays open until it lapses")
        assert peer is not None
        return adhoc.Route(("--peer", peer, "--login-id", target.id), f"{name} over the tailnet", name)

    def _refuse_if_invited(self, pc: str) -> None:
        """The device shows one agent, the invite's, and its End reaches only that one, so its settings entry takes nobody else while the invite's session lives."""
        for name, kept in self._adhoc.items():
            if kept.device == pc:
                held = self._sessions.get(name)
                who = f"{held.title} holds" if held is not None else "an agent is joining"
                raise DeskRefused(f"{who} {pc} now, through an invite the person there made")

    @staticmethod
    def _invite_refusal(target: adhoc.Target, route: adhoc.Route, said: adhoc.Outcome) -> str:
        if said != ("refused", adhoc.NOT_THIS_PC):
            return adhoc.refusal(target, route, said)
        return (f"{route.rigdesk} did not show it holds this invite as {target.id}, so nothing of the invite was sent there: the invite may have lapsed, been used or been revoked, "
                f"the device may run a RigDesk too old to show it, or stock.login_id on {route.rigdesk} may name another device")

    async def connect(self, given_id: str, password: str, agent: str, graph: str, node: str, title: str, approved: bool = False) -> tuple[Held, str]:
        """Joins a PC by its RustDesk ID and a password, with no settings entry: our RigDesk PCs that answer are tried first over the tailnet
        under that ID (one that is not it says so before it looks at the password), then the public ID server, or the server the ID names.
        The session is this agent's until it leaves or the PC ends it."""
        target = adhoc.target_of(given_id)
        if isinstance(target, str):
            raise DeskRefused(target)
        mine = self.held_by(agent, graph)
        if mine is not None:
            raise DeskRefused(f"you already own the session on {mine.pc}; desk/leave it before connecting to another")
        name = f"id-{target.id}"
        settings = await asyncio.to_thread(self.host.settings, RigdeskSettings)
        if name in self._pcs or name in self._adhoc or name in settings.pcs:
            other = self._sessions.get(name)
            raise DeskRefused(f"{other.title if other is not None else 'another agent'} holds {target.id} now" if name not in settings.pcs
                              else f"{name} is set up in Rig's settings: desk/request or desk/join reaches it")
        kept = self._adhoc[name] = AdHoc(target.id)
        try:
            return await self._adhoc_joined(name, kept, target, password, settings, agent, graph, node, title, approved)
        except BaseException:
            await self._adhoc_closed(name, None)
            raise

    async def pair_key(self) -> str:
        """Rig's public pairing key, base64, made on first asking: the device keeps it and accepts only invites proved with its private half, which
        stays in a 0600 file here."""
        settings = await asyncio.to_thread(self.host.settings, RigdeskSettings)
        bridge = str(Path(settings.bridge_bin).expanduser()) if "/" in settings.bridge_bin else settings.bridge_bin
        await asyncio.to_thread(SECRETS.expanduser().mkdir, mode=0o700, parents=True, exist_ok=True)
        try:
            process = await asyncio.create_subprocess_exec(bridge, "--pair-key", str((SECRETS / PAIR_KEY).expanduser()), stdin=asyncio.subprocess.DEVNULL,
                                                           stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
        except OSError as failed:
            raise DeskRefused(f"rigdesk-bridge could not start ({bridge}): {failed}") from failed
        out, err = await asyncio.wait_for(process.communicate(), 15)
        if process.returncode != 0:
            raise DeskRefused(f"rigdesk-bridge gave no pairing key: {err.decode('utf-8', 'replace').strip() or f'exit code {process.returncode}'}")
        return out.decode().strip()

    async def _join_invite(self, device: str, invite: Invited, agent: str, graph: str, node: str, title: str) -> Held:
        """Redeems a device's invite for this agent, which Rig names: the device spends the invite on the first login that proves Rig's key, so the first
        agent to arrive owns the session and any later one is refused there; an invite the person made for one agent refuses every other."""
        target = adhoc.target_of(device)
        if isinstance(target, str):
            raise DeskRefused(target)
        mine = self.held_by(agent, graph)
        if mine is not None:
            raise DeskRefused(f"you already own the session on {mine.pc}; desk/leave it before joining another")
        if not await asyncio.to_thread((SECRETS / PAIR_KEY).expanduser().exists):
            raise DeskRefused("this Rig has no pairing key yet, so no device can have been paired with it: open RigDesk on the device while it reaches Rig, then invite again")
        name = f"id-{target.id}"
        settings = await asyncio.to_thread(self.host.settings, RigdeskSettings)
        if name in self._pcs or name in self._adhoc or name in settings.pcs:
            other = self._sessions.get(name)
            raise DeskRefused(f"{other.title if other is not None else 'another agent'} holds {target.id} now" if name not in settings.pcs
                              else f"{name} is set up in Rig's settings under the name an invite's session would take")
        kept = self._adhoc[name] = AdHoc(target.id, invited=True)
        try:
            held, _ = await self._adhoc_joined(name, kept, target, invite, settings, agent, graph, node, title, True)
            return held
        except BaseException:
            await self._adhoc_closed(name, None)
            raise

    def connect_asked(self, ask: str, given_id: str, password: str, agent: str, seat: SeatId, route: tuple[str, dict[str, str]]) -> None:
        """Holds a desk/connect's password, in memory only, while the operator decides; it lapses unanswered after CONNECT_ASK_S, and the
        operator's request, whose Accept goes to route, is withdrawn."""
        asked = self._asked[ask] = AskedConnect(given_id, password, agent, route)
        asked.expiry = TASKS.spawn(f"rigdesk-connect-ask-{ask}", self._connect_lapsed(ask, seat))

    def connect_dropped(self, ask: str) -> None:
        asked = self._asked.pop(ask, None)
        if asked is not None and asked.expiry is not None:
            asked.expiry.cancel()

    async def connect_approved(self, ask: str, agent: str, graph: str, node: str, title: str) -> tuple[Held, str]:
        """The operator's Accept: the asked desk/connect dials out now, with the password held for it."""
        asked = self._asked.pop(ask, None)
        if asked is None:
            raise DeskRefused(f"its password was dropped, as the request went unanswered for {CONNECT_ASK_S // 60} minutes or Rig restarted, so nothing was dialed; "
                              "the agent can desk/connect again")
        if asked.expiry is not None:
            asked.expiry.cancel()
        return await self.connect(asked.target, asked.password, agent, graph, node, title, approved=True)

    async def _connect_lapsed(self, ask: str, seat: SeatId) -> None:
        await asyncio.sleep(CONNECT_ASK_S)
        asked = self._asked.pop(ask, None)
        if asked is None or not await self.host.route_ended(seat, *asked.route):
            return
        words = (f"RigDesk: your desk/connect to {asked.target} lapsed: the operator did not approve it within {CONNECT_ASK_S // 60} minutes, so nothing was dialed, "
                 "its password is gone and the operator's request is withdrawn. desk/connect again if you still need it.")
        lapsed = Held(f"id-{asked.target}", asked.agent, seat.graph, seat.node, "", f"lapsed-{ask}", now_ms(), "adhoc")
        await self._tell(lapsed, words, f"lapsed:{ask}", "queue")

    # --- the PC's own prompt for an agent's desk/request (prompts.py) ---

    def held_on(self, pc: str) -> Held | None:
        return self._sessions.get(pc)

    def offer_at(self, pc: str) -> tuple[str, dict[str, str]]:
        link = self._link(pc)
        return link.spec.url, self._headers(link)

    @property
    def http(self) -> aiohttp.ClientSession:
        return self._client()

    async def tell_seat(self, pc: str, agent: str, seat: SeatId, words: str, key: str) -> bool:
        return await self._tell(Held(pc, agent, seat.graph, seat.node, "", key, now_ms()), words, key, "queue")

    async def _adhoc_joined(self, name: str, kept: AdHoc, target: adhoc.Target, password: str | Invited, settings: RigdeskSettings,
                            agent: str, graph: str, node: str, title: str, approved: bool) -> tuple[Held, str]:
        secret = f"adhoc-{target.id}"
        if isinstance(password, Invited):
            claim = json.dumps({"grant": password.grant, "secret": password.secret, "agent_id": agent, "agent_title": title or agent})
            password_file = Path(await asyncio.to_thread(self._keep_secret, secret, "invite", claim)).expanduser()
            login = ("--invite-file", str(password_file), "--pair-key-file", str((SECRETS / PAIR_KEY).expanduser()))
        else:
            password_file = Path(await asyncio.to_thread(self._keep_secret, secret, "password", password)).expanduser()
            login = ("--password-file", str(password_file))
        token_file = Path(await asyncio.to_thread(self._keep_secret, secret, "token", secrets.token_urlsafe(32))).expanduser()
        server = settings.connect_server
        key_file = Path(await asyncio.to_thread(self._keep_secret, secret, "key", target.key or (server.key if server is not None else adhoc.PUBLIC_KEY))).expanduser()
        kept.files = (password_file, token_file, key_file)
        busy = {pc: held.title for pc, held in self._sessions.items() if pc in settings.pcs and held.agent != agent}
        routes = [self._invite_route(target, settings, busy)] if isinstance(password, Invited) else await self._adhoc_routes(target, settings, key_file, set(busy))
        if isinstance(password, Invited):
            kept.device = routes[0].rigdesk
        # Only our own PCs are dialed unasked; any other waits for the operator's approval, and once given, ours are not tried again.
        outside = [] if approved else [route for route in routes if route.rigdesk is None]
        if not isinstance(password, Invited):
            routes = [route for route in routes if (route.rigdesk is None) == approved]
        unasked = "".join(f"; {pc} was not tried, as {holder} holds it" for pc, holder in sorted(busy.items()))
        bridge = str(Path(settings.bridge_bin).expanduser()) if "/" in settings.bridge_bin else settings.bridge_bin
        tried: list[str] = []
        for route in routes:
            taken = ({urlsplit(pc.url).port for pc in settings.pcs.values()} | {urlsplit(link.spec.url).port for link in self._pcs.values()}
                     | {each.port for each in self._adhoc.values()})
            # Picked and kept with no await between, so a connect running beside this one cannot pick the same port.
            port = kept.port = self._first_free(taken)
            log = await asyncio.to_thread(self._remote_log, name)
            try:
                process = await asyncio.create_subprocess_exec(bridge, *route.argv, *login, "--token-file", str(token_file),
                                                               "--port", str(port), "--once", "--journal", str(self._dir / f"bridge-{name}.journal"),
                                                               stdin=asyncio.subprocess.DEVNULL, stdout=asyncio.subprocess.PIPE, stderr=log, start_new_session=True)
            except OSError as failed:
                raise DeskRefused(f"rigdesk-bridge could not start ({bridge}): {failed}") from failed
            finally:
                log.close()
            kept.process = process
            said = await adhoc.outcome(process)
            if said[0] == "logged in":
                kept.words = route.words
                break
            await self._stopped(process)
            kept.process = None
            if isinstance(password, Invited):
                raise DeskRefused(self._invite_refusal(target, route, said))
            if route.rigdesk is not None and (said[0] == "failed" or said[1] == adhoc.NOT_THIS_PC):
                tried.append(route.rigdesk)
                continue
            raise DeskRefused(adhoc.refusal(target, route, said) + (f" (it is not {', '.join(tried)} either)" if tried else "") + unasked)
        else:
            if outside:
                raise ConnectAsk(target.id, outside[0].words, tuple(tried))
            raise DeskRefused(f"{target.id} could not be reached")
        assert kept.process is not None
        kept_password = None if isinstance(password, Invited) else str(password_file)
        stock = StockRemote(peer=route.argv[1], password_file=kept_password) if route.rigdesk is not None else StockRemote(id=target.id, password_file=kept_password)
        link = self._pcs[name] = PcLink(name, DeskPc(url=f"http://127.0.0.1:{port}", token_file=str(token_file), stock=stock, mcp=False))
        link.task = TASKS.spawn(f"rigdesk-{name}", self._held(link))
        kept.watch = TASKS.spawn(f"rigdesk-adhoc-{name}", self._adhoc_watch(name, kept.process))
        self._linked(link)
        for _ in range(50):
            if link.state == "up":
                break
            await asyncio.sleep(0.2)
        minted = await self.viewer(name, "mint")
        grant = LINK.match(minted["link"])
        assert grant is not None, "a minted link is a share link"
        body = {"grant": grant["grant"], "secret": grant["secret"], "agent": {"id": agent, "title": title, "graph": graph}}
        async with self._client().post(f"{link.spec.url}/claim", json=body, headers=self._headers(link), timeout=aiohttp.ClientTimeout(total=15)) as answer:
            claimed = _object(await answer.json(content_type=None))
            if answer.status != 200 or not isinstance(claimed.get("session"), str):
                raise DeskRefused(f"{name}'s rigdesk-bridge refused the session: {claimed.get('error') or f'HTTP {answer.status}'}")
        other = self.held_by(agent, graph)
        if other is not None:
            raise DeskRefused(f"you came to own the session on {other.pc} while this connect ran; desk/leave it before connecting to another")
        held = self._sessions[name] = Held(name, agent, graph, node, title, claimed["session"], now_ms(), "adhoc")
        self._ended_why.pop((agent, graph), None)
        link.standing = claimed.get("standing")
        await asyncio.to_thread(self._write_state)
        _LOG.info("rigdesk_adhoc_joined", f"RigDesk {name}: {title} joined RustDesk ID {target.id} through {kept.words}", pc=name)
        return held, kept.words

    async def _adhoc_watch(self, name: str, process: asyncio.subprocess.Process) -> None:
        try:
            why = await adhoc.ended(process)
        except asyncio.CancelledError:
            await self._stopped(process)
            raise
        await self._adhoc_closed(name, why)

    async def _adhoc_closed(self, name: str, why: str | None) -> None:
        """An ad-hoc PC gone: its bridge stopped, its files deleted, and with why, its agent's next desk call says it; without why, its agent left or never joined."""
        kept = self._adhoc.pop(name, None)
        if kept is None:
            return
        if kept.watch is not None and kept.watch is not asyncio.current_task():
            kept.watch.cancel()
        if kept.process is not None:
            await self._stopped(kept.process)
        link = self._pcs.pop(name, None)
        if link is not None and link.task is not None:
            link.task.cancel()
        held = self._sessions.pop(name, None)
        self._unsent.pop(name, None)
        for path in kept.files:
            path.unlink(missing_ok=True)
        await asyncio.to_thread(self._write_state)
        self.host.forget(PC, self._pcs.keys())
        if held is not None and why is not None:
            _LOG.info("rigdesk_session_ended", f"RigDesk {name}: {held.title}'s ad-hoc session ended: {why}", pc=name)
            again = ("An invite works once: the person at the device can make a new one." if kept.invited
                     else "desk/connect with its ID and a password joins it again; a one-time password works for one session only.")
            await self._ended(held, f"RigDesk {name}: your session on RustDesk ID {kept.id} ended: {why}. {again}")

    async def _adhoc_idle(self) -> None:
        cutoff = now_ms() - adhoc.IDLE_MS
        for held in [held for name, held in self._sessions.items() if name in self._adhoc and max(held.used_ms, held.since_ms) < cutoff]:
            await self._adhoc_closed(held.pc, f"{adhoc.IDLE_MS // 60_000} minutes went by without a desk call")

    async def _adhoc_end_all(self) -> None:
        for name in list(self._adhoc):
            kept = self._adhoc.pop(name)
            if kept.process is not None:
                await self._stopped(kept.process)
            for path in kept.files:
                path.unlink(missing_ok=True)

    async def _adhoc_restarted(self) -> None:
        """Ad-hoc sessions do not outlive Rig: the bridges stopped with it, and their files are deleted here."""
        for path in await asyncio.to_thread(lambda: list(SECRETS.expanduser().glob("adhoc-*"))):
            path.unlink(missing_ok=True)
        if self._adhoc_lost:
            await asyncio.to_thread(self._write_state)
        for held in self._adhoc_lost:
            await self._ended(held, f"RigDesk {held.pc}: Rig restarted, so your ad-hoc session there ended. desk/connect with its ID and a password joins it again.")
        self._adhoc_lost = []


NATIVE = frozenset({"join", "leave", "attachment", "request", "pcs", "shell", "connect"})
