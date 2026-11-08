"""RigDesk: hands a RigDesk PC's session to a Rig agent. A share link from the viewer, given to an agent, lets it call desk/join(link);
from then on desk/<tool> drives that PC through the PC's own agent API, and what the human does there reaches the agent as Rig messages.
The fork's source: https://github.com/deussalo/rigdesk (AGPL-3.0); Rig talks to it over HTTP only."""
from __future__ import annotations

from collections.abc import Mapping
from http import HTTPStatus
from typing import Any

import aiohttp
from pydantic import ConfigDict, Field

from core.base import ToolAnswer, View
from core.boundary import BranchTool, NoParams, Params, StatusLine
from core.kernel_api import mcp_capability, plugin_capability
from core.contrib import ContributionDescriptor, RouteContribution, ToolBranch
from core.endpoint import Request, endpoint
from core.mcp_call import Answer, Call, ToolDef, ToolInput, gloss_of
from core.plugin_host import Live, PluginHost
from core.policy import OPERATOR
from core.refusal import PluginCode

from .desks import ID, NATIVE, Desks, DeskRefused, RigdeskSettings

BRANCH = "desk"
CAPABILITY = mcp_capability(BRANCH)
REFUSED = PluginCode(f"{ID}.refused")
DOWN = PluginCode(f"{ID}.pc_down")
NO_SESSION = "you own no RigDesk session: ask the human at the PC for a share link (Share with agent in the viewer), then call desk/join with it"


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
                                                    "If they take input back or block you, you are told, and your actions are refused until they hand it back.")}


async def _leave(desks: Desks, call: Call, _given: LeaveInput) -> Answer:
    agent, graph, _, _ = _who(call)
    held = desks.held_by(agent, graph)
    if held is None:
        return call.refused(REFUSED, NO_SESSION)
    try:
        left = await desks.leave(held)
    except Exception as failed:  # noqa: BLE001
        return _refused(call, failed)
    return {"pc": held.pc, "left": True, "pc_had_session": left}


async def _attachment(desks: Desks, call: Call, given: AttachmentInput) -> Answer:
    agent, graph, _, _ = _who(call)
    held = desks.held_by(agent, graph)
    if held is None:
        return call.refused(REFUSED, NO_SESSION)
    try:
        path = await desks.attachment(held, given.id)
    except Exception as failed:  # noqa: BLE001
        return _refused(call, failed)
    return {"pc": held.pc, "id": given.id, "file": str(path), "bytes": path.stat().st_size, "mime": "image/png"}


def _passed(desks: Desks, tool: str, listed: dict[str, Any]) -> ToolDef:
    async def run(call: Call, given: ToolInput) -> Answer:
        agent, graph, _, _ = _who(call)
        held = desks.held_by(agent, graph)
        if held is None:
            return call.refused(REFUSED, NO_SESSION)
        try:
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


def _why_missing(host: PluginHost, name: str) -> str | None:
    branch, slash, tool = name.partition("/")
    if branch != BRANCH or not slash:
        return None
    if not host.live(Desks).catalog():
        return f"no RigDesk PC has been reached yet, so its tools are not known; desk/join works, and {name} shows once a PC answers"
    return f"RigDesk PCs offer no tool {tool!r}; tool_load `desk/` lists the ones they do"


def _capabilities(host: PluginHost) -> Mapping[str, str]:
    return {CAPABILITY: "Join a RigDesk PC with a share link and use its tools."}


def _opened(host: PluginHost) -> frozenset[str]:
    return frozenset({CAPABILITY}) if host.live(Desks).for_fleet else frozenset()


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


async def _status(host: PluginHost) -> tuple[StatusLine, ...]:
    return tuple(StatusLine(text=f"RigDesk {row['name']}: {row['state']}" + (f" ({row['why']})" if row["why"] else ""))
                 for row in host.live(Desks).pcs() if row["state"] == "down")


CONTRIBUTION = ContributionDescriptor(
    id=ID, release="0.1.0", live=Live(Desks, Desks.settle, Desks.end), settings=RigdeskSettings, status=_status,
    tool_branch=ToolBranch(_tools, _why_missing, _capabilities, _opened, _listing, _decide),
    refusal_codes={REFUSED: HTTPStatus.CONFLICT, DOWN: HTTPStatus.SERVICE_UNAVAILABLE},
    routes=(RouteContribution("GET", "pcs", get_pcs), RouteContribution("GET", "session", get_session)))
