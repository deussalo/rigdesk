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
import time
from collections.abc import Callable
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Annotated, Any, Literal, TypedDict
from urllib.parse import urlsplit

import aiohttp
from pydantic import AfterValidator, ConfigDict, Field, StringConstraints, model_validator

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
FILES_KEPT = 300
STOP_FRESH_MS = 60_000  # a Stop replayed after Rig was away is told, not acted on: the turn it meant is long gone
REMOTE_LOG_BYTES = 4 * 1024 * 1024
VIEWER_VERBS = frozenset({"mint", "hand", "human", "stop", "block", "unblock", "revoke"})
LINK = re.compile(r"^rigdesk://(?P<host>[A-Za-z0-9.-]{1,253})/(?P<grant>[0-9a-f]{16,128})#(?P<secret>[0-9a-f]{16,128})$")

type PcName = Annotated[str, StringConstraints(pattern=r"^[A-Za-z0-9_.-]{1,64}$")]


def _web_address(url: str) -> str:
    parts = urlsplit(url)
    if parts.scheme not in ("http", "https") or not parts.hostname:
        raise ValueError(f"{url!r} is not an http or https URL")
    if parts.username or parts.password:
        raise ValueError("a URL holds no credentials: name a token_file")
    return url.rstrip("/")


type WebAddress = Annotated[str, AfterValidator(_web_address)]


class StockRemote(Shape):
    model_config = ConfigDict(defer_build=True, json_schema_extra={"description": "A PC running stock RustDesk with direct IP access on: Rig runs rigdesk-bridge for it, which logs in with the PC's permanent password."})
    peer: Annotated[str, StringConstraints(pattern=r"^[A-Za-z0-9.:\[\]-]{1,255}$"), Field(description="The PC on the tailnet: its MagicDNS name or 100.x address, with :port if not 21118.")]
    password_file: Annotated[str, Field(description="A file readable by this user alone holding the PC's RustDesk permanent password.")]
    viewer_port: Annotated[int | None, Field(ge=1, le=65535, description="Where the bridge relays its session to RigDesk GUIs, on loopback (publish it with tailscale serve --tcp); unset, no GUI can attach.")] = None
    viewer_password_file: Annotated[str | None, Field(description="A file readable by this user alone holding the password GUIs log in to the bridge with.")] = None

    @model_validator(mode="after")
    def _viewer_whole(self) -> StockRemote:
        if (self.viewer_port is None) != (self.viewer_password_file is None):
            raise ValueError("viewer_port and viewer_password_file go together")
        return self


class DeskPc(Shape):
    model_config = ConfigDict(defer_build=True, json_schema_extra={"description": "A PC running RigDesk: where its agent API is and the file holding its bearer token."})
    url: Annotated[WebAddress, Field(description="Its agent API, published by tailscale serve: https://<pc>.<tailnet>.ts.net; for a stock PC, http://127.0.0.1:<port> where Rig runs rigdesk-bridge.")]
    token_file: Annotated[str, Field(description="A file readable by this user alone holding the PC's agent-API token.")]
    client: Annotated[str, Field(description="The client this PC belongs to.")] = ""
    stock: Annotated[StockRemote | None, Field(description="Set for a PC running stock RustDesk instead of RigDesk.")] = None
    assigned: Annotated[list[str], Field(description='Who holds a standing grant to this stock PC, as "agent:<id>" or "graph:<name>": '
                                                     "they join on their first desk call, with no share link, and again after any restart.")] = []

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


class PcHolder(TypedDict):
    agent_id: str
    title: str
    graph: str


# One RigDesk PC as another plugin reads it through the provision rigdesk.pcs. assigned is its standing grant (plugins.rigdesk.pcs.<name>.assigned,
# "agent:<id>" or "graph:<name>"), which only a stock PC honours; held_by is the agent that owns its session now, by a link or the grant.
# The pcs plugin copies PcHolder and PcEntry exactly and matches the schema, so no docstrings and never change one side alone.
class PcEntry(TypedDict):
    name: str
    client: str
    stock: bool
    state: Literal["connecting", "up", "down"]
    why: str | None
    assigned: tuple[str, ...]
    held_by: PcHolder | None


def _grants(assigned: list[str], agent: str, graph: str) -> bool:
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
    if kind == "revoked" and event.get("reason") != "the agent left":
        return f"RigDesk {pc}: your session ended at {at}: {event.get('reason', 'revoked')}. Ask the human for a new link to join again."
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
        self._catalog: dict[str, dict[str, Any]] = {}
        self._tools: tuple[Any, ...] = ()
        self._tools_of: object = None
        self._sessions: dict[str, Held] = {}
        self._boots: dict[str, str] = {}
        self._last: dict[str, int] = {}
        self._http: aiohttp.ClientSession | None = None
        self._watch: asyncio.Task[None] | None = None
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
        self._http = aiohttp.ClientSession(timeout=aiohttp.ClientTimeout(total=None, sock_connect=10))
        self._watch = TASKS.spawn("rigdesk-settings", self._watching())

    async def end(self, _ending: LiveEnding) -> None:
        tasks = [task for task in (self._watch, *(link.task for link in self._pcs.values()), *(link.remote for link in self._pcs.values())) if task is not None]
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
                await self._idle_leaves()
            except Exception as failed:  # noqa: BLE001
                _LOG.warning("rigdesk_settings_unread", f"RigDesk settings could not be read, PCs stay as they are: {type(failed).__name__}: {failed}")
            await asyncio.sleep(SETTINGS_EVERY_S)

    def _reconcile(self, settings: RigdeskSettings) -> None:
        self._for_fleet = settings.for_fleet
        moved = settings.bridge_bin != self._bridge_bin
        self._bridge_bin = settings.bridge_bin
        for name, link in self._pcs.items():
            spec = settings.pcs.get(name)
            if spec is not None and spec != link.spec and spec.model_copy(update={"assigned": link.spec.assigned}) == link.spec:
                link.spec = spec
                held = self._sessions.get(name)
                if held is not None and held.via == "assigned" and not _grants(spec.assigned, held.agent, held.graph):
                    TASKS.spawn(f"rigdesk-unassign-{name}", self._unassigned(name))
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

    # --- rigdesk-bridge, for a stock PC ---

    def _remote_log(self, name: str) -> Any:
        path = self._dir / f"bridge-{name}.log"
        path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        if path.exists() and path.stat().st_size > REMOTE_LOG_BYTES:
            path.replace(path.with_suffix(".log.1"))
        return open(path, "ab")  # noqa: SIM115

    async def _remote(self, link: PcLink, stock: StockRemote) -> None:
        """Keeps one rigdesk-bridge running for a stock PC, restarting it when it exits; its output goes to bridge-<pc>.log here."""
        argv = [str(Path(self._bridge_bin).expanduser()) if "/" in self._bridge_bin else self._bridge_bin,
                "--peer", stock.peer, "--password-file", str(Path(stock.password_file).expanduser()),
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
        if known and held is not None and self.assigned_pc(held.agent, held.graph) != pc:
            await self._tell(held, f"RigDesk {pc}: RigDesk restarted (on the PC or the bridge), so your session there ended. Ask the human for a new link to join again.", f"boot:{boot}")

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
            await self._tell(replaced, f"RigDesk {pc}: the human shared the PC with another agent, so your session there ended.", f"replaced:{held.since_ms}")
        return held

    def assigned_pc(self, agent: str, graph: str) -> str | None:
        """The stock PC this agent, or its graph, holds a standing grant to."""
        return next((name for name, link in self._pcs.items() if link.spec.stock is not None and _grants(link.spec.assigned, agent, graph)), None)

    async def _unassigned(self, pc: str) -> None:
        """The operator took the grant away: its session ends now, and the agent is told by the revoke event."""
        try:
            await self.viewer(pc, "revoke")
        except (DeskRefused, aiohttp.ClientError, TimeoutError, OSError) as failed:
            _LOG.warning("rigdesk_unassign_failed", f"RigDesk {pc}: ending the unassigned session failed: {failed}", pc=pc)

    async def assigned_join(self, agent: str, graph: str, node: str, title: str) -> Held | None:
        """Joins the PC this agent is assigned, as a share link would, unless another agent holds it now; None with no grant."""
        pc = self.assigned_pc(agent, graph)
        if pc is None:
            return None
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
            await self._tell(held, f"RigDesk {held.pc}: you made no desk call for {IDLE_LEAVE_MS // 60_000} minutes, so you left the PC for other agents; your next desk call joins it again if it is free.",
                             f"idle:{held.since_ms}")

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
        async with self._client().post(f"{link.spec.url}/mcp", json=body, headers=self._headers(link, held.session), timeout=aiohttp.ClientTimeout(total=CALL_LIMIT_S)) as answer:
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
                         "owned_here": held is not None and owner is not None and held.agent == owner.get("agent_id")})
        return rows

    def entries(self) -> tuple[PcEntry, ...]:
        """What the provision rigdesk.pcs answers: each PC, its grant and who holds it."""
        rows: list[PcEntry] = []
        for name, link in sorted(self._pcs.items()):
            held = self._sessions.get(name)
            rows.append(PcEntry(name=name, client=link.spec.client, stock=link.spec.stock is not None, state=link.state, why=link.remote_why or link.why,
                                assigned=tuple(link.spec.assigned), held_by=None if held is None else PcHolder(agent_id=held.agent, title=held.title, graph=held.graph)))
        return tuple(rows)

    def session_of(self, graph: str, agent: str) -> dict[str, Any] | None:
        held = self.held_by(agent, graph)
        if held is None:
            return None
        link = self._pcs.get(held.pc)
        standing = (link.standing if link is not None else None) or {}
        return {"pc": held.pc, "state": "missing" if link is None else link.state, "holder": standing.get("holder"),
                "since_ms": standing.get("since_ms"), "joined_ms": held.since_ms}


NATIVE = frozenset({"join", "leave", "attachment"})
