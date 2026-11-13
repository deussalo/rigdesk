"""The RigDesk PCs as Rig holds them: one /events stream per PC, the session each Rig agent claimed, and the calls it makes there.

The PC decides who may act; this side only carries. A PC's events reach the agent owning its session as Rig messages, keyed by the PC's
boot and event id, so a reconnect never delivers one twice. Images the human sends are copied here before the PC is told they arrived,
since the PC frees them then."""
from __future__ import annotations

import asyncio
import base64
import binascii
import json
import re
import secrets
import socket
import time
from collections.abc import Callable
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Annotated, Any, Literal, TypedDict
from urllib.parse import urlsplit

import aiohttp
from pydantic import AfterValidator, BaseModel, ConfigDict, Field, StringConstraints, ValidationError, model_validator

from core.base import event_log, now_ms, replace_file
from core.model import AgentId, GraphName, RefusedOutcome, SeatId
from core.node_data import Shape
from core.plugin_host import LiveEnding, PluginHost, PluginWords
from core.tasks import TASKS

ID = "rigdesk"
_LOG = event_log(__name__)
RETRY_S = (1.0, 2.0, 5.0, 10.0, 30.0)
SETTINGS_EVERY_S = 5.0
IDLE_LEAVE_MS = 5 * 60_000
CALL_LIMIT_S = 120.0
SHELL_LIMIT_S = 600


def _call_limit_s(tool: str, arguments: dict[str, Any]) -> float:
    """A shell command may run up to its own limit, which the PC enforces; the call waits that long on top."""
    asked = arguments.get("timeout_s") if tool == "shell" else None
    return CALL_LIMIT_S + (asked if isinstance(asked, int) and 0 < asked <= SHELL_LIMIT_S else 60 if tool == "shell" else 0)
FILES_KEPT = 300
STOP_FRESH_MS = 60_000  # a Stop replayed after Rig was away is told, not acted on: the turn it meant is long gone
REMOTE_LOG_BYTES = 4 * 1024 * 1024
HEALTH_EVERY_S = 30.0
PROBE_LIMIT_S = 5.0
HBBS_PORT = 21116  # RustDesk's rendezvous port when the host names none; hbbr listens on the next one up
STOCK_PORTS = range(21120, 21200)
SECRETS = Path("~/.rig/rigdesk")
VIEWER_VERBS = frozenset({"mint", "hand", "human", "stop", "block", "unblock", "revoke"})
LINK = re.compile(r"^rigdesk://(?P<host>[A-Za-z0-9.-]{1,253})/(?P<grant>[0-9a-f]{16,128})#(?P<secret>[0-9a-f]{16,128})$")

type PcName = Annotated[str, StringConstraints(pattern=r"^[A-Za-z0-9_.-]{1,64}$")]
type Grantee = Annotated[str, StringConstraints(pattern=r"^(agent|graph):[A-Za-z0-9_.:-]{1,128}$")]
GRANTS_DOC = "grants"


def _web_address(url: str) -> str:
    parts = urlsplit(url)
    if parts.scheme not in ("http", "https") or not parts.hostname:
        raise ValueError(f"{url!r} is not an http or https URL")
    if parts.username or parts.password:
        raise ValueError("a URL holds no credentials: name a token_file")
    return url.rstrip("/")


type WebAddress = Annotated[str, AfterValidator(_web_address)]


class StockRemote(Shape):
    model_config = ConfigDict(defer_build=True, json_schema_extra={"description": "A PC running stock RustDesk: Rig runs rigdesk-bridge for it, which logs in with the PC's permanent password, "
                                                                                 "by direct IP over the tailnet (peer) or by its RustDesk ID through the ID server (id)."})
    peer: Annotated[Annotated[str, StringConstraints(pattern=r"^[A-Za-z0-9.:\[\]-]{1,255}$")] | None, Field(description="The PC on the tailnet: its MagicDNS name or 100.x address, with :port if not 21118.")] = None
    id: Annotated[Annotated[str, StringConstraints(pattern=r"^[A-Za-z0-9_-]{6,32}$")] | None, Field(description="The PC's RustDesk ID, digits without spaces, for a PC registered with the id_server instead of on the tailnet.")] = None
    password_file: Annotated[str, Field(description="A file readable by this user alone holding the PC's RustDesk permanent password.")]
    viewer_port: Annotated[int | None, Field(ge=1, le=65535, description="Where the bridge relays its session to RigDesk GUIs, on loopback (publish it with tailscale serve --tcp); unset, no GUI can attach.")] = None
    viewer_password_file: Annotated[str | None, Field(description="A file readable by this user alone holding the password GUIs log in to the bridge with.")] = None

    @model_validator(mode="after")
    def _viewer_whole(self) -> StockRemote:
        if (self.peer is None) == (self.id is None):
            raise ValueError("a stock PC has a peer (direct IP over the tailnet) or an id (through the ID server), not both")
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


class DeskPc(Shape):
    model_config = ConfigDict(defer_build=True, json_schema_extra={"description": "A PC running RigDesk: where its agent API is and the file holding its bearer token."})
    url: Annotated[WebAddress, Field(description="Its agent API, published by tailscale serve: https://<pc>.<tailnet>.ts.net; for a stock PC, http://127.0.0.1:<port> where Rig runs rigdesk-bridge.")]
    token_file: Annotated[str, Field(description="A file readable by this user alone holding the PC's agent-API token.")]
    client: Annotated[str, Field(description="The client this PC belongs to.")] = ""
    stock: Annotated[StockRemote | None, Field(description="Set for a PC running stock RustDesk instead of RigDesk; who may act on it with no share link is set on the PCs page.")] = None

    @model_validator(mode="after")
    def _stock_is_local(self) -> DeskPc:
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
    via: Literal["link", "assigned"] = "link"
    used_ms: int = 0


class DeskRefused(Exception):
    """A call that cannot go through, with the words the agent reads."""


class Grants(BaseModel):
    """Who holds a standing grant to each stock PC: rigdesk's own doc, written only by Desks.assign and Desks.unassign, on an operator's act."""
    model_config = ConfigDict(frozen=True, extra="forbid")
    pcs: dict[PcName, tuple[Grantee, ...]] = {}


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


# One RigDesk PC as another plugin reads it through the provision rigdesk.pcs. assigned is who holds a standing grant to it ("agent:<id>" or
# "graph:<name>", in rigdesk's grants doc), which only a stock PC has; held_by is the agent that owns its session now, by a link or the grant.
# The pcs plugin copies PcHolder and PcEntry exactly and matches the schema, so no docstrings and never change one side alone.
class PcEntry(TypedDict):
    name: str
    client: str
    stock: bool
    state: Literal["connecting", "up", "down"]
    why: str | None
    assigned: tuple[str, ...]
    held_by: PcHolder | None


def _grants(assigned: tuple[str, ...], agent: str, graph: str) -> bool:
    return bool({f"agent:{agent}", f"graph:{graph}"} & set(assigned))


def pc_of_link(link: str, pcs: dict[str, DeskPc]) -> tuple[str, str, str] | str:
    """The PC a share link names, and its grant and secret; or why it names none. The host is the PC's name, its URL's host, or that host's first label."""
    matched = LINK.match(link.strip())
    if matched is None:
        return "that is not a RigDesk share link: one reads rigdesk://<pc>/<grant>#<secret>, copied with Share with agent in the viewer"
    host = matched["host"].lower()
    for name, spec in pcs.items():
        url_host = (urlsplit(spec.url).hostname or "").lower()
        if host in (name.lower(), url_host, url_host.split(".")[0]):
            return name, matched["grant"], matched["secret"]
    return f"no RigDesk PC named {host} is set up in Rig (plugins.rigdesk.pcs); the operator adds it there"


def _clock(at_ms: int) -> str:
    return time.strftime("%H:%M:%S", time.localtime(at_ms / 1000))


def words_of(pc: str, event: dict[str, Any]) -> str | None:
    """What the owning agent is told of one event from its PC; None for one it is not told of."""
    kind, at = event.get("type"), _clock(int(event.get("at_ms") or now_ms()))
    if kind == "holder":
        holder, by = event.get("holder"), event.get("by", "")
        if by == "share":
            return None
        if by == "stop":
            return (f"RigDesk {pc}: the human pressed Stop at {at}. Stop what you are doing there and wait for them; "
                    "every call there is refused until they resume you.")
        if holder == "agent" and by == "unblock":
            return f"RigDesk {pc}: the human resumed you at {at}. You may act there again; look first, the screen may have changed."
        if (holder == "human" and by != "unblock") or (holder == "agent" and by == "quiet"):
            # Input passes to the human while they use the PC and back once they pause; a call in between is refused with when to retry.
            return None
        if holder == "agent":
            return f"RigDesk {pc}: the human handed input to you at {at}. You may act there again."
        if holder == "blocked":
            return f"RigDesk {pc}: the human blocked you at {at}. Every call there, reads included, is refused until they unblock you."
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
        self._tools_of: object = None
        self._sessions: dict[str, Held] = {}
        self._boots: dict[str, str] = {}
        self._last: dict[str, int] = {}
        self._http: aiohttp.ClientSession | None = None
        self._watch: asyncio.Task[None] | None = None
        self._probe: asyncio.Task[None] | None = None
        self._probe_now = asyncio.Event()
        self._listeners: tuple[Listener, ...] = ()
        self._grants = Grants()
        self._grants_revision = 0
        self._granting = asyncio.Lock()
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

    async def end(self, _ending: LiveEnding) -> None:
        tasks = [task for task in (self._watch, self._probe, *(link.task for link in self._pcs.values()), *(link.remote for link in self._pcs.values())) if task is not None]
        for task in tasks:
            task.cancel()
        await asyncio.gather(*tasks, return_exceptions=True)
        if self._http is not None:
            await self._http.close()

    async def _watching(self) -> None:
        while True:
            try:
                settings = await asyncio.to_thread(self.host.settings, RigdeskSettings)
                self._reconcile(settings)
                await self._prune_grants()
                await self._idle_leaves()
            except Exception as failed:  # noqa: BLE001
                _LOG.warning("rigdesk_settings_unread", f"RigDesk settings could not be read, PCs stay as they are: {type(failed).__name__}: {failed}")
            await asyncio.sleep(SETTINGS_EVERY_S)

    def _reconcile(self, settings: RigdeskSettings) -> None:
        self._for_fleet = settings.for_fleet
        moved = (settings.bridge_bin, settings.id_server) != (self._bridge_bin, self._id_server)
        self._bridge_bin = settings.bridge_bin
        if settings.id_server != self._id_server:
            self._listeners = () if settings.id_server is None else tuple(Listener(role, host, port) for role, host, port in settings.id_server.listeners())
            self._probe_now.set()
        self._id_server = settings.id_server
        for name in [name for name, link in self._pcs.items() if settings.pcs.get(name) != link.spec or (moved and link.spec.stock is not None)]:
            link = self._pcs.pop(name)
            for task in (link.task, link.remote):
                if task is not None:
                    task.cancel()
        for name, spec in settings.pcs.items():
            if name not in self._pcs:
                link = self._pcs[name] = PcLink(name, spec)
                if spec.stock is not None:
                    link.remote = TASKS.spawn(f"rigdesk-bridge-{name}", self._remote(link, spec.stock))
                link.task = TASKS.spawn(f"rigdesk-{name}", self._held(link))

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
        """Grants one agent or graph a standing grant to a stock PC; False when it held one already."""
        async with self._granting:
            link = self._pcs.get(pc)
            if link is None:
                raise DeskRefused(f"no RigDesk PC named {pc} is set up in Rig")
            if link.spec.stock is None:
                raise DeskRefused(f"{pc} runs RigDesk: it is shared through its viewer with a share link, and takes no standing grant")
            held = self.granted(pc)
            if grantee in held:
                return False
            await self._save_grants(Grants(pcs=self._grants.pcs | {pc: (*held, grantee)}))
            return True

    async def unassign(self, pc: str, grantee: str) -> bool:
        """Takes one grant away; a session held through it ends now. False when there was none."""
        async with self._granting:
            held = self.granted(pc)
            if grantee not in held:
                return False
            left = tuple(each for each in held if each != grantee)
            await self._save_grants(Grants(pcs={name: grantees for name, grantees in (self._grants.pcs | {pc: left}).items() if grantees}))
        session = self._sessions.get(pc)
        if session is not None and session.via == "assigned" and not _grants(left, session.agent, session.graph):
            await self._unassigned(pc)
        return True

    async def _prune_grants(self) -> None:
        """A grant exists only on a stock PC set up in Rig: one removed from settings, or no longer stock, loses its grants."""
        async with self._granting:
            kept = {pc: grantees for pc, grantees in self._grants.pcs.items() if (link := self._pcs.get(pc)) is not None and link.spec.stock is not None}
            if kept != self._grants.pcs:
                gone = sorted(set(self._grants.pcs) - set(kept))
                await self._save_grants(Grants(pcs=kept))
                _LOG.warning("rigdesk_grants_pruned", f"RigDesk: {', '.join(gone)} left Rig's settings or stopped being stock, so their grants were removed", pcs=", ".join(gone))

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

    @staticmethod
    async def _probed(listener: Listener) -> None:
        try:
            _, writer = await asyncio.wait_for(asyncio.open_connection(listener.host, listener.port), PROBE_LIMIT_S)
        except TimeoutError:
            state, why = "down", f"no TCP connection within {PROBE_LIMIT_S:.0f} s"
        except OSError as failed:
            state, why = "down", failed.strerror or f"{type(failed).__name__}: {failed}"
        else:
            writer.close()
            state, why = "up", None
        if state != listener.state:
            listener.since_ms = now_ms()
            if state == "down":
                _LOG.warning("rigdesk_id_server_down", f"RigDesk ID server: {listener.role} at {listener.address} is down: {why}", role=listener.role)
        listener.state, listener.why, listener.checked_ms = state, why, now_ms()

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
                *self._route(stock), "--password-file", str(Path(stock.password_file).expanduser()),
                "--token-file", str(Path(link.spec.token_file).expanduser()), "--port", str(urlsplit(link.spec.url).port),
                "--journal", str(self._dir / f"bridge-{link.name}.journal")]
        if stock.viewer_port is not None and stock.viewer_password_file is not None:
            argv += ["--viewer-port", str(stock.viewer_port), "--viewer-password-file", str(Path(stock.viewer_password_file).expanduser())]
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

    def _token(self, spec: DeskPc) -> str:
        try:
            token = Path(spec.token_file).expanduser().read_text().strip()
        except OSError as unread:
            raise DeskRefused(f"its token file {spec.token_file} could not be read: {unread}") from unread
        if not token or any(ord(char) < 0x21 or ord(char) > 0x7e for char in token):
            raise DeskRefused(f"its token file {spec.token_file} is empty or holds characters a token cannot")
        return token

    def _headers(self, link: PcLink, session: str | None = None) -> dict[str, str]:
        headers = {"Authorization": f"Bearer {self._token(link.spec)}"}
        return headers | ({"X-Desk-Session": session} if session else {})

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
            except Exception as failed:  # noqa: BLE001
                why = f"{type(failed).__name__}: {failed}"
            if link.state != "down":
                link.down_since_ms = now_ms()
                _LOG.warning("rigdesk_pc_down", f"RigDesk PC {link.name} is down: {why}", pc=link.name)
            link.state, link.why = "down", why
            await asyncio.sleep(RETRY_S[min(tries, len(RETRY_S) - 1)])
            tries += 1

    async def _connected(self, link: PcLink) -> None:
        http = self._client()
        async with http.get(f"{link.spec.url}/health", headers=self._headers(link), timeout=aiohttp.ClientTimeout(total=15)) as answer:
            if answer.status != 200:
                raise DeskRefused(f"/health answered HTTP {answer.status}" + (": the token is refused" if answer.status == 401 else ""))
            health = await answer.json()
        link.standing = health.get("standing")
        boot = str(health.get("boot", ""))
        if boot != self._boots.get(link.name):
            await self._rebooted(link.name, boot)
        await self._listed(link)
        headers = self._headers(link) | {"Last-Event-ID": str(self._last.get(link.name, 0)), "Accept": "text/event-stream"}
        async with http.get(f"{link.spec.url}/events", headers=headers, timeout=aiohttp.ClientTimeout(total=None, sock_read=None)) as stream:
            if stream.status != 200:
                raise DeskRefused(f"/events answered HTTP {stream.status}")
            link.state, link.why, link.down_since_ms = "up", None, None
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

    async def _rebooted(self, pc: str, boot: str) -> None:
        """A PC whose agent host restarted lost its sessions and numbers its events afresh."""
        known = pc in self._boots
        self._boots[pc], self._last[pc] = boot, 0
        held = self._sessions.pop(pc, None)
        await asyncio.to_thread(self._write_state)
        if known and held is not None and not _grants(self.granted(pc), held.agent, held.graph):
            _LOG.info("rigdesk_session_ended", f"RigDesk {pc}: RigDesk restarted, so {held.title}'s session there ended", pc=pc)

    async def _listed(self, link: PcLink) -> None:
        """desk/* offers every tool any PC offers: a stock PC has fewer than a RigDesk PC, and refuses the ones it lacks."""
        body = {"jsonrpc": "2.0", "id": 1, "method": "tools/list"}
        async with self._client().post(f"{link.spec.url}/mcp", json=body, headers=self._headers(link), timeout=aiohttp.ClientTimeout(total=15)) as answer:
            listed = (await answer.json()).get("result", {}).get("tools", [])
        fresh = {tool["name"]: tool for tool in listed if isinstance(tool, dict) and isinstance(tool.get("name"), str) and tool["name"] not in NATIVE}
        merged = self._catalog | fresh
        if fresh and merged != self._catalog:
            self._catalog = merged
            await asyncio.to_thread(self._write_state)

    async def _event(self, link: PcLink, event: dict[str, Any]) -> None:
        pc, kind, event_id = link.name, event.get("type"), int(event.get("id") or 0)
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
        if held is not None and (kind != "revoked" or held.agent == event.get("agent_id")):
            key = f"{self._boots.get(pc, '')}:{event_id}" if event_id else f"{self._boots.get(pc, '')}:gap:{now_ms()}"
            if kind == "message":
                await self._message(link, held, event, key)
            elif (said := words_of(pc, event)) is not None:
                if kind == "holder" and event.get("by") == "stop" and now_ms() - int(event.get("at_ms") or 0) < STOP_FRESH_MS:
                    await self._interrupt(held)
                await self._tell(held, said, key)
        if event_id:
            self._last[pc] = max(self._last.get(pc, 0), event_id)
            await asyncio.to_thread(self._write_state)

    async def _message(self, link: PcLink, held: Held, event: dict[str, Any], key: str) -> None:
        images: list[tuple[str, Path | None]] = []
        for image in event.get("attachments") or []:
            try:
                images.append((image, await self._fetched(link, held, str(image))))
            except DeskRefused:
                images.append((image, None))
        if await self._tell(held, message_words(link.name, str(event.get("text") or ""), images), key):
            ack = f"{link.spec.url}/attachments/-/ack?upto={int(event.get('id') or 0)}"
            async with self._client().post(ack, headers=self._headers(link, held.session), timeout=aiohttp.ClientTimeout(total=15)):
                pass

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
            _LOG.warning("rigdesk_stop_failed", f"RigDesk {held.pc}: interrupting {held.graph}/{held.node} failed: {type(failed).__name__}: {failed}", pc=held.pc)
            return
        if isinstance(done, RefusedOutcome):
            _LOG.warning("rigdesk_stop_refused", f"RigDesk {held.pc}: {held.graph}/{held.node} was not interrupted: {done.rendered}", pc=held.pc)

    async def _tell(self, held: Held, words: str, key: str) -> bool:
        seat = await self._seat(held)
        try:
            verdict = await self.host.admit(seat, PluginWords(words, f"rigdesk:{held.pc}", key=f"{held.pc}:{key}", timing="steer", headline=f"RigDesk {held.pc}"))
        except Exception as failed:  # noqa: BLE001
            _LOG.warning("rigdesk_tell_failed", f"RigDesk {held.pc}: telling {held.graph}/{held.node} failed: {type(failed).__name__}: {failed}", pc=held.pc)
            return False
        if isinstance(verdict, RefusedOutcome):
            _LOG.warning("rigdesk_tell_refused", f"RigDesk {held.pc}: {held.graph}/{held.node} was not told: {verdict.rendered}", pc=held.pc)
            return False
        return True

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
        if self._tools_of is not self._catalog:
            self._tools, self._tools_of = build(self), self._catalog
        return self._tools

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

    async def join(self, link_text: str, agent: str, graph: str, node: str, title: str, via: Literal["link", "assigned"] = "link") -> Held:
        settings = await asyncio.to_thread(self.host.settings, RigdeskSettings)
        found = pc_of_link(link_text, dict(settings.pcs))
        if isinstance(found, str):
            raise DeskRefused(found)
        pc, grant, secret = found
        mine = self.held_by(agent, graph)
        if mine is not None:
            raise DeskRefused(f"you already own the session on {mine.pc}; desk/leave it before joining another" if mine.pc != pc else f"you already own the session on {pc}")
        link = self._link(pc)
        body = {"grant": grant, "secret": secret, "agent": {"id": agent, "title": title, "graph": graph}}
        async with self._client().post(f"{link.spec.url}/claim", json=body, headers=self._headers(link), timeout=aiohttp.ClientTimeout(total=15)) as answer:
            said = await answer.json(content_type=None)
            if answer.status != 200:
                raise DeskRefused(f"{pc} refused the link: {said.get('error', f'HTTP {answer.status}') if isinstance(said, dict) else answer.status}")
        replaced = self._sessions.get(pc)
        held = self._sessions[pc] = Held(pc, agent, graph, node, title, str(said["session"]), now_ms(), via)
        link.standing = said.get("standing")
        await asyncio.to_thread(self._write_state)
        if replaced is not None and replaced.agent != agent:
            _LOG.info("rigdesk_session_ended", f"RigDesk {pc}: shared with {title}, so {replaced.title}'s session there ended", pc=pc)
        return held

    def assigned_pcs(self, agent: str, graph: str) -> tuple[str, ...]:
        """The stock PCs this agent, or its graph, holds a standing grant to, by name."""
        return tuple(sorted(name for name, link in self._pcs.items() if link.spec.stock is not None and _grants(self.granted(name), agent, graph)))

    async def _unassigned(self, pc: str) -> None:
        """The operator took the grant away: its session ends now, and the agent is told by the revoke event."""
        try:
            await self.viewer(pc, "revoke")
        except (DeskRefused, aiohttp.ClientError, TimeoutError, OSError) as failed:
            _LOG.warning("rigdesk_unassign_failed", f"RigDesk {pc}: ending the unassigned session failed: {failed}", pc=pc)

    async def assigned_join(self, agent: str, graph: str, node: str, title: str) -> Held | None:
        """Joins the one PC this agent is assigned, on any desk call; None with no grant. With several, the agent names one with desk/join(pc)."""
        match self.assigned_pcs(agent, graph):
            case ():
                return None
            case (only,):
                return await self.join_assigned(only, agent, graph, node, title)
            case several:
                raise DeskRefused(f"you are assigned {', '.join(several)}: name the one you mean with desk/join(pc) first")

    async def join_approved(self, pc: str, agent: str, graph: str, node: str, title: str) -> Held:
        """The operator accepted this agent's request to join a stock PC: a share link is minted and claimed for it, as the human would."""
        mine = self.held_by(agent, graph)
        if mine is not None:
            raise DeskRefused(f"{title} already owns the session on {mine.pc}; it must desk/leave it first")
        other = self._sessions.get(pc)
        if other is not None and other.agent != agent:
            raise DeskRefused(f"{other.title} holds {pc} now")
        minted = await self.viewer(pc, "mint")
        return await self.join(minted["link"], agent, graph, node, title)

    async def join_assigned(self, pc: str, agent: str, graph: str, node: str, title: str) -> Held:
        """Joins a PC this agent is assigned, as a share link would, unless another agent holds it now."""
        if pc not in self.assigned_pcs(agent, graph):
            raise DeskRefused(f"{pc} is not assigned to you: ask the human there for a share link, or the operator with desk/request(pc, why)")
        mine = self.held_by(agent, graph)
        if mine is not None:
            raise DeskRefused(f"you already own the session on {mine.pc}; desk/leave it before joining another" if mine.pc != pc else f"you already own the session on {pc}")
        other = self._sessions.get(pc)
        if other is not None and other.agent != agent:
            frees = f"they leave or after {IDLE_LEAVE_MS // 60_000} minutes without a desk call" if other.via == "assigned" else "they leave or the human ends that share link"
            raise DeskRefused(f"{pc} is assigned to you, but {other.title} holds it now; it frees when {frees}")
        minted = await self.viewer(pc, "mint")
        return await self.join(minted["link"], agent, graph, node, title, via="assigned")

    async def _idle_leaves(self) -> None:
        """A session held through a standing grant ends after IDLE_LEAVE_MS without a desk call, so other assigned agents can take the PC; the next call rejoins."""
        cutoff = now_ms() - IDLE_LEAVE_MS
        for held in [held for held in self._sessions.values() if held.via == "assigned" and max(held.used_ms, held.since_ms) < cutoff]:
            try:
                await self.leave(held)
            except (aiohttp.ClientError, TimeoutError, OSError) as failed:
                _LOG.warning("rigdesk_idle_leave_failed", f"RigDesk {held.pc}: leaving the idle session failed: {failed}", pc=held.pc)
            _LOG.info("rigdesk_session_ended", f"RigDesk {held.pc}: {held.title} left after {IDLE_LEAVE_MS // 60_000} idle minutes", pc=held.pc)

    async def leave(self, held: Held) -> bool:
        link = self._link(held.pc)
        self._sessions.pop(held.pc, None)
        await asyncio.to_thread(self._write_state)
        async with self._client().post(f"{link.spec.url}/leave", headers=self._headers(link, held.session), timeout=aiohttp.ClientTimeout(total=15)) as answer:
            return answer.status == 200 and bool((await answer.json(content_type=None)).get("left"))

    async def attachment(self, held: Held, image: str) -> Path:
        return await self._fetched(self._link(held.pc), held, image)

    async def call(self, held: Held, tool: str, arguments: dict[str, Any]) -> dict[str, Any]:
        link = self._link(held.pc)
        held.used_ms = now_ms()
        if link.state != "up":
            raise DeskRefused(f"RigDesk PC {held.pc} is down: {link.why or 'not reached yet'}")
        body = {"jsonrpc": "2.0", "id": now_ms(), "method": "tools/call", "params": {"name": tool, "arguments": arguments}}
        async with self._client().post(f"{link.spec.url}/mcp", json=body, headers=self._headers(link, held.session), timeout=aiohttp.ClientTimeout(total=_call_limit_s(tool, arguments))) as answer:
            if answer.status != 200:
                raise DeskRefused(f"{held.pc} answered HTTP {answer.status}")
            reply = await answer.json()
        if "error" in reply:
            raise DeskRefused(f"{held.pc}: {reply['error'].get('message', 'error')}")
        result = reply.get("result", {})
        texts: list[str] = []
        images: list[dict[str, Any]] = []
        for index, block in enumerate(result.get("content", [])):
            if block.get("type") == "text":
                texts.append(str(block.get("text", "")))
            elif block.get("type") == "image" and isinstance(block.get("data"), str):
                try:
                    data = base64.b64decode(block["data"], validate=False)
                except binascii.Error:
                    continue
                path = await asyncio.to_thread(self._kept, "shots", f"{held.pc}-{tool}-{now_ms()}-{index}.png", data)
                images.append({"file": str(path), "bytes": len(data)})
        return {"pc": held.pc, "text": "\n".join(texts), "images": images, "is_error": result.get("isError") is True}

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

    # --- what the page and pin show ---

    def pcs(self) -> list[dict[str, Any]]:
        rows = []
        for name, link in sorted(self._pcs.items(), key=lambda item: (item[1].spec.client, item[0])):
            standing = link.standing or {}
            owner = standing.get("owner") or None
            held = self._sessions.get(name)
            rows.append({"name": name, "client": link.spec.client, "url": link.spec.url, "stock": link.spec.stock is not None, "state": link.state, "why": link.remote_why or link.why,
                         "down_since_ms": link.down_since_ms, "holder": standing.get("holder") if owner else None,
                         "owner": None if owner is None else {"id": owner.get("agent_id"), "title": owner.get("title"), "graph": owner.get("graph")},
                         "owned_here": held is not None and owner is not None and held.agent == owner.get("agent_id"), "assigned": self.granted(name)})
        return rows

    def entries(self) -> tuple[PcEntry, ...]:
        """What the provision rigdesk.pcs answers: each PC, its grant and who holds it."""
        rows: list[PcEntry] = []
        for name, link in sorted(self._pcs.items()):
            held = self._sessions.get(name)
            rows.append(PcEntry(name=name, client=link.spec.client, stock=link.spec.stock is not None, state=link.state, why=link.remote_why or link.why,
                                assigned=self.granted(name), held_by=None if held is None else PcHolder(agent_id=held.agent, title=held.title, graph=held.graph)))
        return tuple(rows)

    def session_of(self, graph: str, agent: str) -> dict[str, Any] | None:
        held = self.held_by(agent, graph)
        if held is None:
            return None
        link = self._pcs.get(held.pc)
        standing = (link.standing if link is not None else None) or {}
        return {"pc": held.pc, "state": "missing" if link is None else link.state, "holder": standing.get("holder"),
                "since_ms": standing.get("since_ms"), "joined_ms": held.since_ms}


NATIVE = frozenset({"join", "leave", "attachment", "request", "pcs"})
