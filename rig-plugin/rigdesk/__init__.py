"""RigDesk: hands a RigDesk PC's session to a Rig agent. A share link from the viewer, given to an agent, lets it call desk/join(link);
from then on desk/<tool> drives that PC through the PC's own agent API, and what the human does there reaches the agent as Rig messages.
The fork's source: https://github.com/deussalo/rigdesk (AGPL-3.0); Rig talks to it over HTTP only."""
from __future__ import annotations

import asyncio
import os
from collections.abc import Mapping
from http import HTTPStatus
from pathlib import Path
from typing import Annotated, Any, Literal, TypedDict

import aiohttp
from pydantic import BaseModel, ConfigDict, Field, StringConstraints, TypeAdapter

from core.ask_said import Confirmed, Then
from core.base import ToolAnswer, View, new_id, replace_file
from core.boundary import BranchTool, NoParams, Params, StatusLine
from core.kernel_api import RigReach, mcp_capability, plugin_capability
from core.contrib import ContributionDescriptor, RouteContribution, ToolBranch
from core.endpoint import Request, endpoint
from core.model import GraphName, NodeId, Refused, RefusedOutcome, SeatId
from core.mcp_call import Answer, Call, ToolDef, ToolInput, gloss_of
from core.plugin_host import Live, PluginHost
from core.provided import Provision, provider
from core.policy import OPERATOR
from core.refusal import PluginCode

from .desks import ID, NATIVE, Desks, DeskRefused, Grantee, PcEntry, RigdeskSettings

BRANCH = "desk"
CAPABILITY = mcp_capability(BRANCH)
REFUSED = PluginCode(f"{ID}.refused")
DOWN = PluginCode(f"{ID}.pc_down")
UNSHARED = PluginCode(f"{ID}.no_session")


# The pcs plugin copies PcEntry and PcHolder verbatim; Provided.ask matches the rendered schema exactly, docstring included, so never change one side alone.
PCS = Provision("rigdesk.pcs", TypeAdapter(type(None)), TypeAdapter(tuple[PcEntry, ...]))
NO_SESSION = ("you own no RigDesk session and no PC is assigned to you: desk/pcs lists the PCs; ask the operator to let you join one with desk/request(pc, why), "
              "or call desk/join with a share link the human at the PC gives you")


class GrantChange(TypedDict):
    """One grant the pcs plugin's page gives or takes away, for the operator: grantee is "agent:<id>" or "graph:<name>"."""
    pc: str
    grantee: str
    granted: bool


class GrantDone(TypedDict):
    changed: bool


class GrantRefused(TypedDict):
    why: str


# The pcs plugin copies GrantChange, GrantDone and GrantRefused verbatim, as it does PcEntry.
ASSIGN = Provision("rigdesk.assign", TypeAdapter(GrantChange), TypeAdapter(GrantDone | GrantRefused))
REQUEST_CONFIRMED = Provision("rigdesk.request_confirmed", TypeAdapter(Confirmed), TypeAdapter(str))


type RequestKind = Literal["join", "assign"]


class Requested(TypedDict):
    pc: str
    agent: str
    graph: str
    kind: RequestKind


REQUESTED: TypeAdapter[Requested] = TypeAdapter(Requested)


class JoinInput(ToolInput):
    link: str | None = Field(default=None, description="The share link the human gave you: rigdesk://<pc>/<grant>#<secret>. It works once.")
    pc: str | None = Field(default=None, description="Instead of a link: a stock PC assigned to you, by name.")


class LeaveInput(ToolInput):
    pass


class RequestInput(ToolInput):
    pc: str = Field(description="The stock RustDesk PC you need, by its name in Rig; desk/pcs lists them.")
    why: str = Field(min_length=1, max_length=1000, description="What you need it for, in a sentence or two; the operator reads it before deciding.")
    kind: RequestKind = Field(default="join", description="join: one session, now, until you leave or the human ends it. "
                                                           "assign: a standing grant, so you join it by name with desk/join(pc), across restarts, until unassigned.")


class PcsInput(ToolInput):
    pass


class AttachmentInput(ToolInput):
    id: str = Field(description="An image id from the human's message.")


class PassInput(ToolInput):
    model_config = ConfigDict(extra="allow", frozen=True, defer_build=True)


class DeskAnswer(ToolAnswer):
    model_config = ConfigDict(extra="allow", frozen=True, defer_build=True)


def _who(call: Call) -> tuple[str, str, str, str]:
    place = call.contract.place
    return str(place.agent), str(call.graph), str(call.node), place.label or str(call.node)


def _refused(call: Call, refused: Exception) -> Answer:
    if isinstance(refused, DeskRefused):
        return call.refused(REFUSED, str(refused))
    if isinstance(refused, aiohttp.ClientError | TimeoutError | OSError):
        return call.refused(DOWN, f"the RigDesk PC did not answer: {type(refused).__name__}: {refused}", None)
    raise refused


async def _join(desks: Desks, call: Call, given: JoinInput) -> Answer:
    agent, graph, node, title = _who(call)
    if (given.link is None) == (given.pc is None):
        return call.refused("invalid_params", "desk/join takes a share link or the name of a PC assigned to you, not both")
    try:
        held = (await desks.join(given.link, agent, graph, node, title) if given.link is not None
                else await desks.join_assigned(str(given.pc), agent, graph, node, title))
    except Exception as failed:  # noqa: BLE001
        return _refused(call, failed)
    return {"pc": held.pc, "joined": True, "said": (f"You own {held.pc}'s session and hold its input. desk/<tool> acts there; the human sees each call live. "
                                                    "Their own input goes first: while they use the PC your actions are refused, and work again once they pause "
                                                    "for 3 s. If they press Stop you are told, and every call is refused until they resume you.")}


async def _leave(desks: Desks, call: Call, _given: LeaveInput) -> Answer:
    agent, graph, _, _ = _who(call)
    held = desks.held_by(agent, graph)
    if held is None:
        return call.refused(UNSHARED, NO_SESSION)
    try:
        left = await desks.leave(held)
    except Exception as failed:  # noqa: BLE001
        return _refused(call, failed)
    return {"pc": held.pc, "left": True, "pc_had_session": left}


def _held_words(entry: PcEntry, agent: str) -> str | None:
    held = entry["held_by"]
    return None if held is None else "you" if held["agent_id"] == agent else held["title"]


async def _pcs(desks: Desks, call: Call, _given: PcsInput) -> Answer:
    agent, graph, _, _ = _who(call)
    mine = set(desks.assigned_pcs(agent, graph))
    return {"pcs": [{"name": entry["name"], "client": entry["client"], "state": entry["state"],
                     "reached_by": "desk/request (join or assign), or a share link" if entry["stock"] else "a share link from the human in the RigDesk viewer only",
                     "assigned_to_you": entry["name"] in mine, "held_by": _held_words(entry, agent)} for entry in desks.entries()],
            "said": "desk/join(pc) joins a PC assigned to you; desk/request(pc, why, kind) asks the operator to let you join one now or assign it to you."}


async def _request(desks: Desks, call: Call, given: RequestInput) -> Answer:
    agent, graph, _, title = _who(call)
    known = {entry["name"]: entry for entry in desks.entries()}
    entry = known.get(given.pc)
    if entry is None:
        return call.refused(REFUSED, f"no RigDesk PC named {given.pc} is set up in Rig; desk/pcs lists those that are")
    if not entry["stock"]:
        return call.refused(REFUSED, f"{given.pc} runs RigDesk: only the human in its viewer shares it; ask them for a share link and call desk/join with it")
    assigned = given.pc in desks.assigned_pcs(agent, graph)
    if given.kind == "assign" and assigned:
        return call.refused(REFUSED, f"{given.pc} is already assigned to you: desk/join(pc) joins it")
    if given.kind == "join" and assigned:
        return call.refused(REFUSED, f"{given.pc} is assigned to you, so you need no approval: desk/join(pc) joins it")
    held = desks.held_by(agent, graph)
    if given.kind == "join" and held is not None and held.pc == given.pc:
        return call.refused(REFUSED, f"you already own the session on {given.pc}")
    seat, asked = call.contract.place.seat, new_id("desk")
    lets = ("Accept joins it to that PC now, for one session: until it leaves, or you end the share." if given.kind == "join" else
            "Accept lets it act on that PC with no share link, across restarts, until you unassign it on the PCs page.")
    wants = "to join" if given.kind == "join" else "a standing grant to"
    reason = f"{title} ({agent}, in {graph}) asks {wants} RigDesk PC {given.pc}:\n\n{given.why.strip()}\n\n{lets}"[:2000]
    state = await desks.host.confirm(seat, asked, reason, call.kernel.activation_id,
                                     Then(provision=REQUEST_CONFIRMED.name, given={"pc": given.pc, "agent": agent, "graph": graph, "kind": given.kind}))
    return {"pc": given.pc, "kind": given.kind, "ask": asked, "state": state,
            "said": "Nothing is granted yet: the operator is asked to Accept, and the decision arrives as your next input."}


async def _request_confirmed(host: PluginHost, asked: Confirmed) -> str:
    """The operator's Accept: the agent that asked joins the PC now, or is assigned it, if it still stands at its seat and the PC is still stock."""
    given = REQUESTED.validate_python(asked["given"])
    seat = SeatId(GraphName(asked["graph"]), NodeId(asked["node"]))
    identity = await host.roster.identity(seat)
    if identity is None or identity.agent != given["agent"] or seat in await host.roster.closed((seat,)):
        raise Refused(REFUSED, f"{given['agent']} no longer stands at {seat.graph}/{seat.node}, so nothing was done on {given['pc']}")
    desks = host.live(Desks)
    try:
        if given["kind"] == "join":
            held = await desks.join_approved(given["pc"], given["agent"], given["graph"], seat.node, identity.label)
            return f"You joined {held.pc}: its session is yours until you desk/leave or the human ends the share. desk/<tool> acts there."
        changed = await desks.assign(given["pc"], f"agent:{given['agent']}")
    except DeskRefused as refused:
        raise Refused(REFUSED, f"{refused}; nothing was done") from refused
    except (aiohttp.ClientError, TimeoutError, OSError) as failed:
        raise Refused(DOWN, f"{given['pc']} did not answer: {type(failed).__name__}: {failed}; nothing was done") from failed
    return f"{'Assigned' if changed else 'Already assigned'} {given['pc']} to you: desk/join(pc) joins it, with no share link, until the operator unassigns it on the PCs page."


async def _assign(host: PluginHost, change: GrantChange) -> GrantDone | GrantRefused:
    desks = host.live(Desks)
    try:
        changed = await (desks.assign if change["granted"] else desks.unassign)(change["pc"], change["grantee"])
    except DeskRefused as refused:
        return GrantRefused(why=str(refused))
    return GrantDone(changed=changed)


async def _attachment(desks: Desks, call: Call, given: AttachmentInput) -> Answer:
    agent, graph, _, _ = _who(call)
    held = desks.held_by(agent, graph)
    if held is None:
        return call.refused(UNSHARED, NO_SESSION)
    try:
        path = await desks.attachment(held, given.id)
    except Exception as failed:  # noqa: BLE001
        return _refused(call, failed)
    return {"pc": held.pc, "id": given.id, "file": str(path), "bytes": path.stat().st_size, "mime": "image/png"}


def _passed(desks: Desks, tool: str, listed: dict[str, Any]) -> ToolDef:
    async def run(call: Call, given: ToolInput) -> Answer:
        agent, graph, node, title = _who(call)
        held = desks.held_by(agent, graph)
        try:
            held = held or await desks.assigned_join(agent, graph, node, title)
            if held is None:
                return call.refused(UNSHARED, NO_SESSION)
            return await desks.call(held, tool, dict(given.model_extra or {}))
        except Exception as failed:  # noqa: BLE001
            return _refused(call, failed)
    words = str(listed.get("description") or tool)
    schema = listed.get("inputSchema") if isinstance(listed.get("inputSchema"), dict) else {"type": "object"}
    read_only = bool((listed.get("annotations") or {}).get("readOnlyHint"))
    return ToolDef(f"{BRANCH}/{tool}", f"On the RigDesk PC you joined: {words}", PassInput, DeskAnswer, run, run, read_only=read_only, plugin=plugin_capability(ID),
                   denied_alone=(CAPABILITY, mcp_capability(BRANCH, tool)), upstream_schema=schema, gloss=gloss_of(words))


def _native[I: ToolInput](desks: Desks, tool: str, words: str, shape: type[I], handler: Any, read_only: bool = False) -> ToolDef:
    async def run(call: Call, given: ToolInput) -> Answer:
        if not isinstance(given, shape):
            return call.refused("invalid_params", f"{BRANCH}/{tool} takes {shape.__name__}")
        return await handler(desks, call, given)
    return ToolDef(f"{BRANCH}/{tool}", words, shape, DeskAnswer, run, run, read_only=read_only, plugin=plugin_capability(ID),
                   denied_alone=(CAPABILITY, mcp_capability(BRANCH, tool)), gloss=gloss_of(words))


def _built(desks: Desks) -> tuple[ToolDef, ...]:
    return (_native(desks, "join", "Join a RigDesk PC with the share link its human gave you (it works once), or a stock PC assigned to you by name; you then own its session alone.",
                    JoinInput, _join),
            _native(desks, "leave", "Leave the RigDesk PC you joined; its session ends.", LeaveInput, _leave),
            _native(desks, "pcs", "Every RigDesk PC in Rig: its client, whether it is up, whether it is assigned to you, and who holds it now.", PcsInput, _pcs, read_only=True),
            _native(desks, "request", "Ask the operator to let you join a stock RustDesk PC now (kind join), or to assign it to you (kind assign).", RequestInput, _request),
            _native(desks, "attachment", "An image the human sent you from the RigDesk viewer, as a PNG file you can read.", AttachmentInput, _attachment, read_only=True),
            *(_passed(desks, name, listed) for name, listed in desks.catalog().items() if name not in NATIVE))


def _tools(host: PluginHost) -> tuple[ToolDef, ...]:
    """A branch tool's run is given the Call alone, so each closes over the live Desks it reaches; rebuilt only when the catalog changes."""
    return host.live(Desks).tools(_built)


def _why_missing(host: PluginHost, name: str, listed: bool) -> str | None:
    branch, slash, tool = name.partition("/")
    if branch != BRANCH or not slash:
        return None
    if not host.live(Desks).catalog():
        return f"no RigDesk PC has been reached yet, so its tools are not known; desk/join works, and {name} shows once a PC answers"
    return f"RigDesk PCs offer no tool {tool!r}" + ("; tool_load `desk/` lists the ones they do" if listed else "")


def _capabilities(host: PluginHost) -> Mapping[str, str]:
    return {CAPABILITY: "Join a RigDesk PC with a share link and use its tools."}


def _reach(host: PluginHost) -> Mapping[str, RigReach]:
    return {BRANCH: "listed" if host.live(Desks).for_fleet else "withheld"}


def _listing(host: PluginHost, capability: str) -> tuple[BranchTool, ...]:
    if capability != CAPABILITY:
        return ()
    return tuple(BranchTool(capability=mcp_capability(BRANCH, defined.name.split("/", 1)[1]), name=defined.name.split("/", 1)[1], about=defined.description,
                            says_read_only=defined.read_only, held=None) for defined in _tools(host))


async def _decide(_host: PluginHost, _capability: str, _tools_named: tuple[str, ...]) -> tuple[str, ...]:
    return ()


class PcRow(View):
    name: str
    client: str
    url: str
    stock: bool = Field(description="A stock RustDesk PC, driven through rigdesk-bridge on this host; its share and holder buttons are on this page.")
    state: str
    why: str | None
    down_since_ms: int | None
    holder: str | None = Field(description="human, agent or blocked while an agent owns the session; null with none.")
    owner: dict[str, str | None] | None = Field(description="The agent owning the session: id, title and graph.")
    owned_here: bool = Field(description="The owner joined through this Rig.")
    assigned: tuple[str, ...] = Field(description='Who holds a standing grant to it, "agent:<id>" or "graph:<name>"; a stock PC only.')


class ListenerRow(View):
    role: Literal["hbbs", "hbbr"]
    address: str
    state: Literal["unchecked", "up", "down"] = Field(description="up when it took a TCP connection at the last probe, every 30 s.")
    why: str | None
    since_ms: int | None = Field(description="When it entered this state.")
    checked_ms: int | None


class IdServerRow(View):
    host: str
    listeners: tuple[ListenerRow, ...]
    install: str | None = Field(description="The one administrator PowerShell line that puts a Windows PC on this ID server with stock RustDesk; null when the key file is unreadable.")
    install_why: str | None = Field(description="Why there is no install line.")


class PcsView(View):
    pcs: tuple[PcRow, ...]
    id_server: IdServerRow | None = Field(description="Our RustDesk ID server and its health; null with no id_server set.")
    source: str


# The script's own commit on GitHub, so the line runs exactly what was reviewed; a new script is a new commit here and its hash.
STOCK_SCRIPT = "https://raw.githubusercontent.com/deussalo/rigdesk/d62a30eda697a3cfb734c1cfb10ce72eff769348/install/stock-rustdesk.ps1"
STOCK_SCRIPT_SHA256 = "991779686CACC64E7D6312184E211E3BD45121C3A49DCF464AFE5226B4B12A3F"


def install_line(config: str) -> str:
    return ("[Net.ServicePointManager]::SecurityProtocol='Tls12'; $f=\"$env:TEMP\\stock-rustdesk.ps1\"; "
            f"iwr -UseBasicParsing {STOCK_SCRIPT} -OutFile $f; "
            f"if ((Get-FileHash $f -Algorithm SHA256).Hash -ne '{STOCK_SCRIPT_SHA256}') {{ throw 'stock-rustdesk.ps1 checksum mismatch' }}; "
            f"& ([scriptblock]::Create((Get-Content -Raw $f))) -Config '{config}'; Remove-Item $f")


def _id_server_row(desks: Desks) -> IdServerRow | None:
    server = desks.id_server()
    if server is None:
        return None
    try:
        install, why = install_line(desks.install_config()), None
    except DeskRefused as refused:
        install, why = None, str(refused)
    return IdServerRow(host=server["host"], listeners=tuple(ListenerRow(**each) for each in server["listeners"]), install=install, install_why=why)


@endpoint(NoParams, OPERATOR)
async def get_pcs(request: Request[NoParams]) -> PcsView:
    desks = request.host().live(Desks)
    return PcsView(pcs=tuple(PcRow(**row) for row in desks.pcs()), id_server=await asyncio.to_thread(_id_server_row, desks), source="https://github.com/deussalo/rigdesk")


class SessionParams(Params):
    graph: str
    agent: str


class SessionView(View):
    session: dict[str, Any] | None = Field(description="The RigDesk session this agent owns: pc, state, holder, since_ms, joined_ms; null with none.")


@endpoint(SessionParams, OPERATOR)
async def get_session(request: Request[SessionParams]) -> SessionView:
    return SessionView(session=request.host().live(Desks).session_of(request.params.graph, request.params.agent))


type ViewerVerb = Literal["mint", "hand", "human", "stop", "block", "unblock", "revoke"]


class ViewerBody(BaseModel):
    model_config = ConfigDict(frozen=True, extra="forbid", defer_build=True)
    pc: str
    verb: ViewerVerb


class ViewerParams(Params):
    body: ViewerBody


class ViewerDone(View):
    link: str | None = Field(default=None, description="For mint: the share link to give an agent, rigdesk://<pc>/<grant>#<secret>; it works once.")
    changed: bool | None = Field(default=None, description="For the holder verbs: whether the standing changed.")


@endpoint(ViewerParams, OPERATOR)
async def post_viewer(request: Request[ViewerParams]) -> ViewerDone | RefusedOutcome:
    """A stock PC's viewer buttons: mint a share link, hand input to the agent, take it back (human), stop the agent, block, unblock or revoke the share."""
    body = request.params.body
    try:
        return ViewerDone(**await request.host().live(Desks).viewer(body.pc, body.verb))
    except DeskRefused as refused:
        return request.refused(REFUSED, str(refused))
    except (aiohttp.ClientError, TimeoutError, OSError) as failed:
        return request.refused(DOWN, f"{body.pc}'s rigdesk-bridge did not answer: {type(failed).__name__}: {failed}")


class AssignBody(BaseModel):
    model_config = ConfigDict(frozen=True, extra="forbid", defer_build=True)
    pc: str
    grantee: Grantee
    granted: bool


class AssignParams(Params):
    body: AssignBody


class AssignDone(View):
    changed: bool = Field(description="false when the grant already stood as asked.")


@endpoint(AssignParams, OPERATOR)
async def post_assign(request: Request[AssignParams]) -> AssignDone | RefusedOutcome:
    """Gives or takes away one standing grant to a stock PC, as the PCs page does: from this page, to the agent sharing it now."""
    body = request.params.body
    desks = request.host().live(Desks)
    try:
        changed = await (desks.assign if body.granted else desks.unassign)(body.pc, body.grantee)
    except DeskRefused as refused:
        return request.refused(REFUSED, str(refused))
    return AssignDone(changed=changed)


TOKENS = Path("~/.rig/rigdesk")

type PcName = Annotated[str, StringConstraints(pattern=r"^[a-z0-9][a-z0-9-]{0,62}$")]
type Token = Annotated[str, StringConstraints(pattern=r"^[A-Za-z0-9_-]{32,128}$")]


class TokenBody(BaseModel):
    model_config = ConfigDict(frozen=True, extra="forbid", defer_build=True)
    pc: PcName
    token: Token


class TokenParams(Params):
    body: TokenBody


class TokenKept(View):
    token_file: str = Field(description="Where the PC's agent-API token is kept, readable by this user alone: name it as the PC's token_file in plugins.rigdesk.pcs.")


def _kept(path: Path, token: str) -> None:
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    if path.exists():
        os.chmod(path, 0o600)
    replace_file(path, token.encode(), new_mode=0o600)


@endpoint(TokenParams, OPERATOR)
async def post_token(request: Request[TokenParams]) -> TokenKept:
    """Keeps a PC's agent-API token, the one its install line sets, in a 0600 file of its own; a second call for the same PC replaces it, as rerunning the line rotates it."""
    body = request.params.body
    path = TOKENS / f"{body.pc}.token"
    await asyncio.to_thread(_kept, path.expanduser(), body.token)
    return TokenKept(token_file=str(path))


type RustdeskId = Annotated[str, StringConstraints(pattern=r"^[0-9]{6,32}$")]
type RustdeskPassword = Annotated[str, StringConstraints(pattern=r"^[\x21-\x7e]{6,128}$")]


class AddBody(BaseModel):
    model_config = ConfigDict(frozen=True, extra="forbid", defer_build=True)
    name: PcName
    id: RustdeskId
    password: RustdeskPassword
    client: Annotated[str, StringConstraints(max_length=64)] = ""


class AddParams(Params):
    body: AddBody


class PcAdded(View):
    name: str
    entry: dict[str, Any] = Field(description="Its plugins.rigdesk.pcs entry, to write under its name; once settings hold it, Rig starts its rigdesk-bridge within seconds.")


@endpoint(AddParams, OPERATOR)
async def post_pc(request: Request[AddParams]) -> PcAdded | RefusedOutcome:
    """Adds a stock RustDesk PC by its ID and permanent password, as its install line printed them: keeps the password and a fresh bridge token in 0600 files
    and answers its settings entry on a free loopback port. The page writes the entry; the password never reaches settings."""
    body = request.params.body
    try:
        added = await request.host().live(Desks).added(body.name, body.id, body.password, body.client.strip())
    except DeskRefused as refused:
        return request.refused(REFUSED, str(refused))
    return PcAdded(name=body.name, entry=added.model_dump(mode="json", exclude_none=True))


async def _status(host: PluginHost) -> tuple[StatusLine, ...]:
    desks = host.live(Desks)
    server = desks.id_server()
    lines = () if server is None else (StatusLine(text=f"RigDesk ID server {server['host']}: " + ", ".join(
        f"{each['role']} {each['state']}" + (f" ({each['why']})" if each["why"] else "") for each in server["listeners"])),)
    return (*lines, *(StatusLine(text=f"RigDesk {row['name']}: {row['state']}" + (f" ({row['why']})" if row["why"] else ""))
                      for row in desks.pcs() if row["state"] == "down"))


async def _provided(host: PluginHost, _asked: None) -> tuple[PcEntry, ...]:
    return host.live(Desks).entries()


CONTRIBUTION = ContributionDescriptor(
    id=ID, release="0.1.0", live=Live(Desks, Desks.settle, Desks.end), settings=RigdeskSettings, status=_status,
    tool_branch=ToolBranch(_tools, _why_missing, _capabilities, _reach, _listing, _decide),
    refusal_codes={REFUSED: HTTPStatus.CONFLICT, DOWN: HTTPStatus.SERVICE_UNAVAILABLE, UNSHARED: HTTPStatus.FORBIDDEN},
    provisions=(provider(PCS, _provided), provider(ASSIGN, _assign), provider(REQUEST_CONFIRMED, _request_confirmed)),
    routes=(RouteContribution("GET", "pcs", get_pcs), RouteContribution("GET", "session", get_session), RouteContribution("POST", "token", post_token),
            RouteContribution("POST", "viewer", post_viewer), RouteContribution("POST", "assign", post_assign),
            RouteContribution("POST", "pc", post_pc)))
