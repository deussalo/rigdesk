"""RigDesk: hands a RigDesk PC's session to a Rig agent. A share link from the viewer, given to an agent, lets it call desk/join(link);
from then on desk/<tool> drives that PC through the PC's own agent API, and what the human does there reaches the agent as Rig messages.
The fork's source: https://github.com/deussalo/rigdesk (AGPL-3.0); Rig talks to it over HTTP only."""
from __future__ import annotations

import asyncio
import os
from collections.abc import Mapping
from http import HTTPStatus
from pathlib import Path
from typing import Annotated, Any, Literal

import aiohttp
from pydantic import BaseModel, ConfigDict, Field, StringConstraints, TypeAdapter

from core.base import ToolAnswer, View, replace_file
from core.boundary import BranchTool, NoParams, Params, StatusLine
from core.kernel_api import RigReach, mcp_capability, plugin_capability
from core.contrib import ContributionDescriptor, RouteContribution, ToolBranch
from core.endpoint import Request, endpoint
from core.model import RefusedOutcome
from core.mcp_call import Answer, Call, ToolDef, ToolInput, gloss_of
from core.plugin_host import Live, PluginHost
from core.provided import Provision, provider
from core.policy import OPERATOR
from core.refusal import PluginCode

from .desks import ID, NATIVE, Desks, DeskRefused, PcEntry, RigdeskSettings

BRANCH = "desk"
CAPABILITY = mcp_capability(BRANCH)
REFUSED = PluginCode(f"{ID}.refused")
DOWN = PluginCode(f"{ID}.pc_down")
UNSHARED = PluginCode(f"{ID}.no_session")
PCS = Provision("rigdesk.pcs", TypeAdapter(type(None)), TypeAdapter(tuple[PcEntry, ...]))
NO_SESSION = ("you own no RigDesk session and no PC is assigned to you: ask the human at the PC for a share link (Share with agent in the viewer) "
              "and call desk/join with it, or ask the operator to assign you a PC on the PCs page")


class JoinInput(ToolInput):
    link: str = Field(description="The share link the human gave you: rigdesk://<pc>/<grant>#<secret>. It works once.")


class LeaveInput(ToolInput):
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
    try:
        held = await desks.join(given.link, agent, graph, node, title)
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
    return (_native(desks, "join", "Join a RigDesk PC with the share link its human gave you; you then own its session alone. The link works once.", JoinInput, _join),
            _native(desks, "leave", "Leave the RigDesk PC you joined; its session ends.", LeaveInput, _leave),
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


class PcsView(View):
    pcs: tuple[PcRow, ...]
    source: str


@endpoint(NoParams, OPERATOR)
async def get_pcs(request: Request[NoParams]) -> PcsView:
    return PcsView(pcs=tuple(PcRow(**row) for row in request.host().live(Desks).pcs()), source="https://github.com/deussalo/rigdesk")


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


async def _status(host: PluginHost) -> tuple[StatusLine, ...]:
    return tuple(StatusLine(text=f"RigDesk {row['name']}: {row['state']}" + (f" ({row['why']})" if row["why"] else ""))
                 for row in host.live(Desks).pcs() if row["state"] == "down")


async def _provided(host: PluginHost, _asked: None) -> tuple[PcEntry, ...]:
    return host.live(Desks).entries()


CONTRIBUTION = ContributionDescriptor(
    id=ID, release="0.1.0", live=Live(Desks, Desks.settle, Desks.end), settings=RigdeskSettings, status=_status,
    tool_branch=ToolBranch(_tools, _why_missing, _capabilities, _reach, _listing, _decide),
    refusal_codes={REFUSED: HTTPStatus.CONFLICT, DOWN: HTTPStatus.SERVICE_UNAVAILABLE, UNSHARED: HTTPStatus.FORBIDDEN},
    provisions=(provider(PCS, _provided),),
    routes=(RouteContribution("GET", "pcs", get_pcs), RouteContribution("GET", "session", get_session), RouteContribution("POST", "token", post_token),
            RouteContribution("POST", "viewer", post_viewer)))
