"""RigDesk: hands a RigDesk PC's session to a Rig agent. A share link from the viewer, given to an agent, lets it call desk/join(link);
from then on desk/<tool> drives that PC through the PC's own agent API, and what the human does there reaches the agent as Rig messages.
The fork's source: https://github.com/deussalo/rigdesk (AGPL-3.0); Rig talks to it over HTTP only."""
from __future__ import annotations

import asyncio
import base64
import json
import os
from collections.abc import Mapping
from http import HTTPStatus
from pathlib import Path
from typing import Annotated, Any, Literal, NotRequired, TypedDict, cast

import aiohttp
from pydantic import BaseModel, ConfigDict, Field, StringConstraints, TypeAdapter

from core.ask_said import Confirmed, Then
from core.base import JsonObject, ToolAnswer, View, as_object, new_id, now_ms, replace_file
from core.boundary import BranchTool, NoParams, Params
from core.kernel_api import RigReach, mcp_capability, plugin_capability
from core.contrib import ContributionDescriptor, RouteContribution, ToolBranch
from core.endpoint import Request, endpoint
from core.model import AgentId, Choice, Choices, GraphName, NodeId, Refused, RefusedOutcome, SeatId
from core.mcp_call import TOOL_LIMIT_S, Answer, Call, ImageContent, TextContent, ToolContent, ToolDef, ToolInput, gloss_of
from core.plugin_host import Live, PluginHost
from core.provided import Provision, provider
from core.policy import OPERATOR
from core.refusal import PluginCode

from . import adhoc
from .desks import CALL_LIMIT_S, CONNECT_ASK_S, ID, MCP_CALL_LIMIT_S, NATIVE, REPORTED, SHELL_LIMIT_S, ConnectAsk, Desks, DeskRefused, Grantee, Held, PcEntry, PcServer, RigdeskSettings, ShellLevel, failure
from .health import figures_json, series
from .shots import inline
from .desks import PcName as DeskName

BRANCH = "desk"
CAPABILITY = mcp_capability(BRANCH)
REFUSED = PluginCode(f"{ID}.refused")
DOWN = PluginCode(f"{ID}.pc_down")
UNSHARED = PluginCode(f"{ID}.no_session")
SHELL_NOT_ALLOWED = PluginCode(f"{ID}.shell_not_allowed")
SYSTEM_NOT_GRANTED = PluginCode(f"{ID}.system_not_granted")
# The PC's own refusals on its agent shell, each passed on under its own code.
PC_SHELL_CODES: dict[str, HTTPStatus] = {"shell_off": HTTPStatus.FORBIDDEN, "shell_paused": HTTPStatus.FORBIDDEN, "blocked": HTTPStatus.FORBIDDEN,
                                         "jobs_full": HTTPStatus.TOO_MANY_REQUESTS, "no_job": HTTPStatus.NOT_FOUND}


NO_SESSION = ("you hold no RigDesk session: desk/join(pc) joins a PC assigned to you (desk/pcs lists every PC and which are yours); given a PC's "
              "RustDesk ID and its password (permanent or one-time), desk/connect(id, password) joins it yourself; for another, "
              "ask the operator with desk/request(pc, why), or call desk/join with a share link the human at the PC gives you. A desk call never joins one by itself")


REQUEST_CONFIRMED = Provision("rigdesk.request_confirmed", TypeAdapter(Confirmed), TypeAdapter(str))
CONNECT_CONFIRMED = Provision("rigdesk.connect_confirmed", TypeAdapter(Confirmed), TypeAdapter(str))
# A RigDesk PC's ask: "always" joins and grants that agent the PC, so its later desk/request or desk/join(pc) joins with no ask.
RIGDESK_CHOICES = Choices(choices=(Choice(id="accept", label="Join once", tone="allow"), Choice(id="always", label="Always allow", tone="allow"),
                                   Choice(id="deny", label="Deny", tone="deny")), safe_choice="deny")


type RequestKind = Literal["join", "assign"]


class Requested(TypedDict):
    pc: str
    agent: str
    graph: str
    kind: RequestKind
    ask: NotRequired[str]


REQUESTED: TypeAdapter[Requested] = TypeAdapter(Requested)


class ConnectAsked(TypedDict):
    agent: str
    graph: str
    id: str
    ask: str


CONNECT_ASKED: TypeAdapter[ConnectAsked] = TypeAdapter(ConnectAsked)


class JoinInput(ToolInput):
    link: str | None = Field(default=None, description="The share link the human gave you: rigdesk://<pc>/<grant>#<secret>. It works once.")
    pc: str | None = Field(default=None, description="Instead of a link: a PC assigned to you, by name.")


class LeaveInput(ToolInput):
    pass


class ConnectInput(ToolInput):
    id: str = Field(min_length=6, max_length=400, description="The PC's RustDesk ID as the operator gave it: the digits it shows (spaces allowed); "
                                                              "ID@host?key=<key> for a PC on a self-hosted ID server; or the ID one of our RigDesk PCs shows.")
    password: str = Field(min_length=1, max_length=128, description="The PC's permanent password, or the one-time password it shows now, as the operator gave it. "
                                                                    "Rig keeps it in a file only Rig's user reads while the session lives, and never repeats it.")
    why: str = Field(min_length=1, max_length=1000, description="What you need the PC for, in a sentence or two: unless it is one of our RigDesk PCs, the operator "
                                                                "reads it before deciding whether Rig dials it.")


class RequestInput(ToolInput):
    pc: str = Field(description="The PC you need, by its name in Rig; desk/pcs lists them. A RigDesk PC takes it only while its own switch allows sessions started from Rig.")
    why: str = Field(min_length=1, max_length=1000, description="What you need it for, in a sentence or two; the operator reads it before deciding.")
    kind: RequestKind = Field(default="join", description="join: one session, now, until you leave or the human ends it. "
                                                           "assign: a standing grant, so you join it by name with desk/join(pc), across restarts, until unassigned.")


class PcsInput(ToolInput):
    pc: str | None = Field(default=None, description="One PC by name: its entry with its newest health sample in full.")


class ShellInput(ToolInput):
    command: str | None = Field(default=None, min_length=1, max_length=32 * 1024, description="What to run: in PowerShell on Windows, in bash on Linux. Omit it to read a job.")
    timeout_s: int | None = Field(default=None, ge=1, le=SHELL_LIMIT_S, description="How long it may run before the PC ends it; 60 by default. A job runs up to a day.")
    pc: str | None = Field(default=None, description="A RigDesk PC by name: runs there with no session and without touching its desktop, if the operator "
                                                     "allowed you its shell (desk/pcs says where). Omitted: the PC you joined.")
    run_as: Literal["system", "user"] | None = Field(default=None, alias="as", description="system runs it with the highest local rights, off every desktop: "
                                                                                           "SYSTEM on Windows, root on Linux; user as the signed-in user, refused with nobody "
                                                                                           "signed in. Default: the most you are allowed.")
    background: bool = Field(default=False, description="Start it as a job and answer its id at once; it outlives the call and Rig's restarts, a reboot ends it. "
                                                        "As system only; a PC runs at most 8.")
    job: str | None = Field(default=None, pattern=r"^j-[0-9a-f]{8}$", description="A job's id: answers its state and its output so far, instead of running anything.")


class AttachmentInput(ToolInput):
    id: str = Field(description="An image id from the human's message.")


class PassInput(ToolInput):
    model_config = ConfigDict(extra="allow", frozen=True, defer_build=True)


class DeskAnswer(ToolAnswer):
    model_config = ConfigDict(extra="allow", frozen=True, defer_build=True)


def _who(call: Call) -> tuple[str, str, str]:
    return str(call.contract.place.agent), str(call.graph), str(call.node)


def _refused(call: Call, refused: Exception) -> Answer:
    if isinstance(refused, DeskRefused):
        return call.refused(REFUSED, str(refused))
    if isinstance(refused, aiohttp.ClientError | TimeoutError | OSError):
        return call.refused(DOWN, f"the RigDesk PC did not answer: {failure(refused)}", None)
    raise refused


async def _join(desks: Desks, call: Call, given: JoinInput) -> Answer:
    agent, graph, node = _who(call)
    if (given.link is None) == (given.pc is None):
        return call.refused("invalid_params", "desk/join takes a share link or the name of a PC assigned to you, not both")
    title = await call.kernel.caller_title()
    try:
        held = (await desks.join(given.link, agent, graph, node, title) if given.link is not None
                else await desks.join_assigned(str(given.pc), agent, graph, node, title))
    except Exception as failed:  # noqa: BLE001
        return _refused(call, failed)
    return {"pc": held.pc, "joined": True, "said": (f"You own {held.pc}'s session and hold its input. desk/<tool> acts there; the human sees each call live. "
                                                    "Their own input goes first: while they use the PC your actions are refused, and work again once they pause "
                                                    "for 3 s. If they press Stop you are told, and every call is refused until they resume you.")}


async def _connect(desks: Desks, call: Call, given: ConnectInput) -> Answer:
    agent, graph, node = _who(call)
    title = await call.kernel.caller_title()
    try:
        held, reached = await desks.connect(given.id, given.password, agent, graph, node, title)
    except ConnectAsk as outside:
        return await _ask_connect(desks, call, given, outside, title)
    except Exception as failed:  # noqa: BLE001
        return _refused(call, failed)
    return {"pc": held.pc, "joined": True, "reached": reached, "said": _connected_words(held.pc)}


def _connected_words(pc: str) -> str:
    return (f"You own the session on {pc} and hold its input; desk/<tool> acts there. It lasts until you desk/leave, the PC ends it, or "
            f"{adhoc.IDLE_MS // 60_000} minutes pass without a desk call; a one-time password then works no more.")


async def _ask_connect(desks: Desks, call: Call, given: ConnectInput, outside: ConnectAsk, title: str) -> Answer:
    """A PC that is none of ours is dialed only once the operator accepts; its password waits in memory, never on disk, and nothing is dialed meanwhile."""
    agent, graph, _ = _who(call)
    seat, asked = call.contract.place.seat, new_id("desk")
    not_ours = f" (it is not {', '.join(outside.tried)})" if outside.tried else ""
    reason = (f"{title} ({agent}, in {graph}) asks to connect to RustDesk ID {outside.target} through {outside.where}: a PC that is none of our RigDesk PCs"
              f"{not_ours}.\n\n{given.why.strip()}\n\nAccept dials it now with the password the agent was given, for one session: until the agent leaves, the PC "
              f"ends it, or {adhoc.IDLE_MS // 60_000} idle minutes pass; what the PC shows then reaches the agent. Deny dials nothing.")[:2000]
    then = {"agent": agent, "graph": graph, "id": outside.target, "ask": asked}
    desks.connect_asked(asked, given.id, given.password, agent, seat, (CONNECT_CONFIRMED.name, then))
    try:
        state = await desks.host.confirm(seat, asked, reason, call.kernel.activation_id, Then(provision=CONNECT_CONFIRMED.name, given=then))
    except BaseException:
        desks.connect_dropped(asked)
        raise
    return {"id": outside.target, "joined": False, "ask": asked, "state": state,
            "said": (f"Nothing is dialed yet: {outside.target} is none of our PCs, so the operator is asked to approve connecting to it through {outside.where}, "
                     f"and the decision arrives as your next input. Unanswered for {CONNECT_ASK_S // 60} minutes, it lapses and nothing is dialed.")}


async def _connect_confirmed(host: PluginHost, asked: Confirmed) -> str:
    """The operator's Accept of a desk/connect: the agent that asked connects now, if it still stands at its seat."""
    given = CONNECT_ASKED.validate_python(asked["given"])
    desks = host.live(Desks)
    seat = SeatId(GraphName(asked["graph"]), NodeId(asked["node"]))
    identity = await host.roster.identity(seat)
    if identity is None or identity.agent != given["agent"] or seat in await host.roster.closed((seat,)):
        desks.connect_dropped(given["ask"])
        raise Refused(REFUSED, f"{given['agent']} no longer stands at {seat.graph}/{seat.node}, so {given['id']} was not dialed")
    try:
        held, reached = await desks.connect_approved(given["ask"], given["agent"], given["graph"], seat.node, identity.label)
    except DeskRefused as refused:
        raise Refused(REFUSED, f"{refused}") from refused
    except (aiohttp.ClientError, TimeoutError, OSError) as failed:
        raise Refused(DOWN, f"{given['id']} could not be joined: {failure(failed)}") from failed
    return f"The operator approved it, and you joined {held.pc} through {reached}. {_connected_words(held.pc)}"


async def _leave(desks: Desks, call: Call, _given: LeaveInput) -> Answer:
    agent, graph, _ = _who(call)
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


def _shell_level(desks: Desks, pc: str, agent: str, graph: str) -> ShellLevel | None:
    """The most an agent may run as on a PC's agent shell: its grant or its graph's, and the user at least on the PC whose session it holds."""
    held = desks.held_by(agent, graph)
    return desks.shell_level(pc, agent, graph) or ("user" if held is not None and held.pc == pc else None)


def _unknown(entry: PcEntry) -> str:
    """What a RigDesk PC's switches are while Rig has not heard from it: its absent answer says nothing about them."""
    return f"unknown until {entry['name']} is reachable"


def _shell_words(desks: Desks, entry: PcEntry, agent: str, graph: str) -> str:
    if entry["stock"]:
        return "none: stock RustDesk runs commands only in a joined session, as its user"
    level = _shell_level(desks, entry["name"], agent, graph)
    switch = entry["shell"]
    pc_says = (f"whether its RigDesk offers an agent shell is {_unknown(entry)}" if switch is None and entry["state"] != "up" else
               "its RigDesk offers no agent shell: install it with the agent shell on" if switch is None else "its agent shell is on" if switch.get("open")
              else f"{switch.get('words')} ({switch.get('code')})")
    top = desks.top_rights(entry["name"])
    may = {"system": f"you may run commands as {top} (as: system) or as the user", "user": "you may run commands as its signed-in user", None: "you may not run commands there"}[level]
    return f"{may}; {pc_says}"


def _reached_by(entry: PcEntry) -> str:
    if (joined := entry.get("adhoc")) is not None:
        return f"desk/connect(id, password) only: joined by its RustDesk ID through {joined['via']}, and gone when that session ends"
    if entry["stock"]:
        return "desk/request (join or assign), or a share link"
    switch = entry["join"]
    if switch is None and entry["state"] != "up":
        return f"a share link from the human in the RigDesk viewer; whether its RigDesk takes sessions started from Rig is {_unknown(entry)}"
    if switch is None:
        return "a share link from the human in the RigDesk viewer only: its RigDesk takes no sessions started from Rig"
    if switch.get("open"):
        return ("desk/request (the operator picks Join once or Always allow in Rig), desk/join(pc) while it is assigned to you, "
                "or a share link from the human in the RigDesk viewer")
    return f"a share link from the human in the RigDesk viewer only: {switch.get('words')}"


def _pc_entry(desks: Desks, entry: PcEntry, agent: str, graph: str, mine: set[str]) -> JsonObject:
    judged = entry["health"]
    return {"name": entry["name"], "client": entry["client"], "state": entry["state"],
            "reached_by": _reached_by(entry),
            "assigned_to_you": entry["name"] in mine, "held_by": _held_words(entry, agent), "shell": _shell_words(desks, entry, agent, graph),
            "health": None if judged is None else {"state": judged.state, "failing": [check.words for check in judged.failing]}}


async def _pcs(desks: Desks, call: Call, given: PcsInput) -> Answer:
    agent, graph, _ = _who(call)
    mine = set(desks.assigned_pcs(agent, graph))
    entries = desks.entries()
    if given.pc is not None:
        entry = next((each for each in entries if each["name"] == given.pc), None)
        if entry is None:
            return call.refused(REFUSED, f"no RigDesk PC named {given.pc} is set up in Rig; desk/pcs lists those that are")
        history = desks.history(given.pc)
        return {"pc": _pc_entry(desks, entry, agent, graph, mine), "sample": figures_json(history[-1]) if history else None}
    return {"pcs": [_pc_entry(desks, entry, agent, graph, mine) for entry in entries],
            "said": "desk/join(pc) joins a PC assigned to you; desk/connect(id, password) joins any PC whose RustDesk ID and password you were given; desk/request(pc, why, kind) asks the operator to let you join one now or assign it to you. "
                    "desk/shell(pc, command) runs a command on a PC whose shell you are allowed, with no session; desk/pcs(pc) gives its newest health sample."}


def _shell_asked(given: ShellInput, run_as: ShellLevel) -> JsonObject:
    if given.job is not None:
        return {"job": given.job}
    asked: JsonObject = {"command": given.command, "as": run_as, "background": given.background}
    return asked | ({} if given.timeout_s is None else {"timeout_s": given.timeout_s})


async def _shell_in_session(desks: Desks, call: Call, held: Held, given: ShellInput) -> Answer:
    """The joined PC's own desk/shell, through its session, as the user: a stock PC's, or a RigDesk older than the agent shell."""
    try:
        return await desks.call(held, "shell", {"command": given.command} | ({} if given.timeout_s is None else {"timeout_s": given.timeout_s}))
    except Exception as failed:  # noqa: BLE001
        return _refused(call, failed)


def _shell_said(text: str) -> JsonObject:
    """The PC answers a command's exit code and output, or a job's state, as a JSON object in its text: the agent reads them as fields."""
    try:
        said = json.loads(text)
    except ValueError:
        return {"text": text}
    return cast(JsonObject, said) if isinstance(said, dict) else {"text": text}


def _shell_answered(pc: str, given: ShellInput, run_as: ShellLevel, said: JsonObject) -> JsonObject:
    """What a PC's shell answered, as the agent reads it. A job ran as system whatever the read asked: a PC starts background jobs as SYSTEM or root only."""
    return {"pc": pc, "as": "system" if given.job is not None else run_as, "ok": said.get("ok") is True} | _shell_said(str(said.get("text") or ""))


async def _shell(desks: Desks, call: Call, given: ShellInput) -> Answer:
    agent, graph, _ = _who(call)
    if (given.job is None) == (given.command is None):
        return call.refused("invalid_params", "desk/shell takes a command to run, or a job to read, not both")
    held = desks.held_by(agent, graph)
    pc = given.pc if given.pc is not None else None if held is None else held.pc
    if pc is None:
        return call.refused(UNSHARED, "desk/shell(pc, command) names a PC whose shell the operator allowed you (desk/pcs says where), or runs on the PC you joined; "
                                      "you named none and hold no session")
    entry = next((each for each in desks.entries() if each["name"] == pc), None)
    if entry is None:
        return call.refused(REFUSED, f"no RigDesk PC named {pc} is set up in Rig; desk/pcs lists those that are")
    holds = held if held is not None and held.pc == pc else None
    if entry["stock"]:
        if holds is None:
            return call.refused(REFUSED, f"{pc} runs stock RustDesk, which runs commands only in a joined session: desk/join it, then call desk/shell without pc")
        if given.run_as == "system" or given.background or given.job is not None:
            return call.refused(REFUSED, f"{pc} runs stock RustDesk: commands run in its RustDesk terminal as its user, with no jobs")
        return await _shell_in_session(desks, call, holds, given)
    level = _shell_level(desks, pc, agent, graph)
    if level is None:
        return call.refused(SHELL_NOT_ALLOWED, f"the operator has not allowed you the agent shell on {pc}; they allow an agent or a graph, as the user or as {desks.top_rights(pc)}, "
                                               "on the RigDesk page. desk/pcs says where you may run one.")
    run_as = given.run_as or level
    if level != "system" and (run_as == "system" or given.background or given.job is not None):
        return call.refused(SYSTEM_NOT_GRANTED, f"on {pc} you may run commands as its signed-in user only (as: user): running as {desks.top_rights(pc)} (as: system), "
                                                f"and background jobs, need the operator's {desks.top_rights(pc)} grant on the RigDesk page")
    try:
        status, said = await desks.shell(pc, agent, graph, await call.kernel.caller_title(), _shell_asked(given, run_as))
    except Exception as failed:  # noqa: BLE001
        return _refused(call, failed)
    if status == HTTPStatus.OK:
        return _shell_answered(pc, given, run_as, said)
    code, words = str(said.get("code") or ""), str(said.get("error") or f"HTTP {status}")
    if code == "no_lane" and holds is not None and run_as == "user" and not given.background:
        return await _shell_in_session(desks, call, holds, given)
    if code in PC_SHELL_CODES:
        return call.refused(PluginCode(f"{ID}.{code}"), f"{pc}: {words}")
    return call.refused(REFUSED, f"{pc} refused the command ({code or status}): {words}")


async def _request(desks: Desks, call: Call, given: RequestInput) -> Answer:
    agent, graph, _ = _who(call)
    title = await call.kernel.caller_title()
    known = {entry["name"]: entry for entry in desks.entries()}
    entry = known.get(given.pc)
    if entry is None:
        return call.refused(REFUSED, f"no RigDesk PC named {given.pc} is set up in Rig; desk/pcs lists those that are")
    if not entry["stock"]:
        holder = entry["held_by"]
        if holder is not None and holder["agent_id"] != agent:
            return call.refused(REFUSED, f"{holder['title']} holds {given.pc} now; ask again once they leave or the share ends")
        try:
            switch = await desks.join_switch(given.pc)
        except Exception as failed:  # noqa: BLE001
            return _refused(call, failed)
        if switch is None:
            return call.refused(REFUSED, f"{given.pc}'s RigDesk is too old for sessions started from Rig; ask the human there for a share link and call desk/join with it")
        if not switch.get("open"):
            return call.refused(REFUSED, f"{given.pc}: {switch.get('words')} ({switch.get('code')}); ask the human there for a share link instead")
    assigned = given.pc in desks.assigned_pcs(agent, graph)
    if given.kind == "assign" and assigned:
        return call.refused(REFUSED, f"{given.pc} is already assigned to you: desk/join(pc) joins it")
    if given.kind == "join" and assigned and not entry["stock"]:
        return await _join(desks, call, JoinInput(pc=given.pc))
    if given.kind == "join" and assigned:
        return call.refused(REFUSED, f"{given.pc} is assigned to you, so you need no approval: desk/join(pc) joins it")
    held = desks.held_by(agent, graph)
    if given.kind == "join" and held is not None and held.pc == given.pc:
        return call.refused(REFUSED, f"you already own the session on {given.pc}")
    seat, asked = call.contract.place.seat, new_id("desk")
    lets = ("Join once starts its session on that RigDesk PC now, with no share link: until it leaves, or the person at the PC ends it from RigDesk's tray "
            "icon, where they are told who it is. Always allow does the same, and lets it join again with no ask until you unassign it on the RigDesk page."
            if not entry["stock"] else
            "Accept joins it to that PC now, for one session: until it leaves, or you end the share." if given.kind == "join" else
            "Accept lets it act on that PC with no share link, across restarts, until you unassign it on the RigDesk page.")
    wants = "to join" if given.kind == "join" else "a standing grant to"
    reason = f"{title} ({agent}, in {graph}) asks {wants} RigDesk PC {given.pc}:\n\n{given.why.strip()}\n\n{lets}"[:2000]
    if entry["stock"]:
        state = await desks.host.confirm(seat, asked, reason, call.kernel.activation_id,
                                         Then(provision=REQUEST_CONFIRMED.name, given={"pc": given.pc, "agent": agent, "graph": graph, "kind": given.kind, "ask": asked}))
    else:
        state = await desks.host.confirm(seat, asked, reason, call.kernel.activation_id,
                                         Then(provision=REQUEST_CONFIRMED.name, given={"pc": given.pc, "agent": agent, "graph": graph, "kind": "join", "ask": asked}),
                                         RIGDESK_CHOICES)
    return {"pc": given.pc, "kind": given.kind, "ask": asked, "state": state,
            "said": "Nothing is granted yet: the operator is asked to Accept, and the decision arrives as your next input."}


async def _request_confirmed(host: PluginHost, asked: Confirmed) -> str:
    """The operator's Accept: the agent that asked joins the PC now, or is assigned a stock PC, if it still stands at its seat.
    Always allow, on a RigDesk PC, assigns it the PC and joins it through that grant; a join that fails takes the new grant back."""
    given = REQUESTED.validate_python(asked["given"])
    seat = SeatId(GraphName(asked["graph"]), NodeId(asked["node"]))
    identity = await host.roster.identity(seat)
    if identity is None or identity.agent != given["agent"] or seat in await host.roster.closed((seat,)):
        raise Refused(REFUSED, f"{given['agent']} no longer stands at {seat.graph}/{seat.node}, so nothing was done on {given['pc']}")
    desks = host.live(Desks)
    try:
        if asked.get("choice") == "always":
            return await _always_allowed(desks, given, seat.node, identity.label)
        if given["kind"] == "join":
            held = await desks.join_approved(given["pc"], given["agent"], given["graph"], seat.node, identity.label, given.get("ask", ""))
            return f"You joined {held.pc}: its session is yours until you desk/leave or the human ends the share. desk/<tool> acts there."
        changed = await desks.assign(given["pc"], f"agent:{given['agent']}")
    except DeskRefused as refused:
        raise Refused(REFUSED, f"{refused}; nothing was done") from refused
    except (aiohttp.ClientError, TimeoutError, OSError) as failed:
        raise Refused(DOWN, f"{given['pc']} did not answer: {failure(failed)}; nothing was done") from failed
    return f"{'Assigned' if changed else 'Already assigned'} {given['pc']} to you: desk/join(pc) joins it, with no share link, until the operator unassigns it on the RigDesk page."


async def _always_allowed(desks: Desks, given: Requested, node: str, title: str) -> str:
    grantee = f"agent:{given['agent']}"
    changed = await desks.assign(given["pc"], grantee)
    try:
        held = await desks.join_approved(given["pc"], given["agent"], given["graph"], node, title, given.get("ask", ""), via="assigned")
    except Exception:
        if changed:
            await desks.unassign(given["pc"], grantee)
        raise
    return (f"You joined {held.pc}, and the operator allowed you it always: from now on desk/join(pc) joins it with no ask, until they unassign you "
            "on the RigDesk page. Its session is yours until you desk/leave or the person at the PC ends it.")


async def _attachment(desks: Desks, call: Call, given: AttachmentInput) -> Answer:
    agent, graph, _ = _who(call)
    held = desks.held_by(agent, graph)
    if held is None:
        return call.refused(UNSHARED, NO_SESSION)
    try:
        path = await desks.attachment(held, given.id)
    except Exception as failed:  # noqa: BLE001
        return _refused(call, failed)
    return {"pc": held.pc, "id": given.id, "file": str(path), "bytes": path.stat().st_size, "mime": "image/png"}


def _upstream(tool: str, listed: dict[str, Any]) -> tuple[str, JsonObject, bool]:
    """An MCP tool as its server lists it: its words, its input schema, and whether it says it only reads."""
    schema, notes = listed.get("inputSchema"), listed.get("annotations")
    read_only = isinstance(notes, dict) and cast(dict[str, Any], notes).get("readOnlyHint") is True
    return str(listed.get("description") or tool), as_object(cast(JsonObject, schema)) if isinstance(schema, dict) else {"type": "object"}, read_only


def _passed(desks: Desks, tool: str, listed: dict[str, Any]) -> ToolDef:
    if tool == "screenshot":
        return _shot(desks, listed)

    async def run(call: Call, given: ToolInput) -> Answer:
        agent, graph, _ = _who(call)
        held = desks.held_by(agent, graph)
        if held is None:
            return call.refused(UNSHARED, NO_SESSION)
        try:
            return await desks.call(held, tool, dict(given.model_extra or {}))
        except Exception as failed:  # noqa: BLE001
            return _refused(call, failed)
    words, schema, read_only = _upstream(tool, listed)
    return ToolDef(f"{BRANCH}/{tool}", f"On the RigDesk PC you joined: {words}", PassInput, DeskAnswer, run, run, read_only=read_only, plugin=plugin_capability(ID),
                   denied_alone=(CAPABILITY, mcp_capability(BRANCH, tool)), upstream_schema=schema, gloss=gloss_of(words),
                   limit_s=CALL_LIMIT_S + 10)


def _shot(desks: Desks, listed: dict[str, Any]) -> ToolDef:
    """desk/screenshot answers with the image, which a session shows the agent at once; the PNG is still kept as a file, as before."""
    async def run(call: Call, given: ToolInput) -> Answer:
        agent, graph, _ = _who(call)
        held = desks.held_by(agent, graph)
        if held is None:
            return call.refused(UNSHARED, NO_SESSION)
        try:
            said = await desks.call(held, "screenshot", dict(given.model_extra or {}))
        except Exception as failed:  # noqa: BLE001
            return _refused(call, failed)
        images: list[dict[str, Any]] = said.get("images") or []
        if said.get("is_error") or not images:
            return TextContent(type="text", text=str(said.get("text") or "the PC sent no screenshot"), is_error=True)
        kept = Path(images[0]["file"])
        shown = await asyncio.to_thread(lambda: inline(kept.read_bytes()))
        return ImageContent(type="image", data=shown.data, mimeType=shown.mime, note=f"{said.get('text', '')} {shown.note} Kept as {kept}.")
    _, schema, read_only = _upstream("screenshot", listed)
    words = "One display right now, shown to you in this answer. Pointer tools take x,y in this image's pixels."
    return ToolDef(f"{BRANCH}/screenshot", f"On the RigDesk PC you joined: {words}", PassInput, ToolContent, run, run, read_only=read_only, plugin=plugin_capability(ID),
                   denied_alone=(CAPABILITY, mcp_capability(BRANCH, "screenshot")), upstream_schema=schema, gloss=gloss_of(words),
                   limit_s=CALL_LIMIT_S + 10)


def _served(desks: Desks, server: PcServer, listed: dict[str, Any]) -> ToolDef:
    """One tool of an MCP server running on a PC: only the agent holding that PC may call it, under the PC's own holder and Block rules."""
    tool, key = str(listed["name"]), server.key

    async def run(call: Call, given: ToolInput) -> Answer:
        agent, graph, _ = _who(call)
        held = desks.held_by(agent, graph)
        if held is None or held.pc != server.pc:
            return call.refused(UNSHARED, f"{key}/{tool} runs on RigDesk PC {server.pc}, which you do not hold: only the agent holding it may call its MCP servers")
        try:
            return await desks.mcp_call(held, key, tool, dict(given.model_extra or {}))
        except Exception as failed:  # noqa: BLE001
            return _refused(call, failed)

    def holds(call: Call) -> bool:
        agent, graph, _ = _who(call)
        held = desks.held_by(agent, graph)
        return held is not None and held.pc == server.pc
    words, schema, read_only = _upstream(tool, listed)
    return ToolDef(f"{key}/{tool}", f"On RigDesk PC {server.pc}, MCP server {server.name} ({server.says}): {words}", PassInput, DeskAnswer, run, run,
                   read_only=read_only, plugin=plugin_capability(ID), family=ID, denied_alone=(mcp_capability(key), mcp_capability(key, tool)), upstream_schema=schema,
                   gloss=gloss_of(words), limit_s=MCP_CALL_LIMIT_S + 10).applying(holds)


def _native[I: ToolInput](desks: Desks, tool: str, words: str, shape: type[I], handler: Any, read_only: bool = False, limit_s: float = TOOL_LIMIT_S) -> ToolDef:
    async def run(call: Call, given: ToolInput) -> Answer:
        if not isinstance(given, shape):
            return call.refused("invalid_params", f"{BRANCH}/{tool} takes {shape.__name__}")
        return await handler(desks, call, given)
    return ToolDef(f"{BRANCH}/{tool}", words, shape, DeskAnswer, run, run, read_only=read_only, plugin=plugin_capability(ID), family=ID,
                   denied_alone=(CAPABILITY, mcp_capability(BRANCH, tool)), gloss=gloss_of(words), limit_s=limit_s)


def _built(desks: Desks) -> tuple[ToolDef, ...]:
    return (_native(desks, "join", "Join a RigDesk PC with the share link its human gave you (it works once), or a stock PC assigned to you by name; you then own its session alone. "
                                  "Given a RustDesk ID and password instead, call desk/connect.",
                    JoinInput, _join),
            _native(desks, "leave", "Leave the RigDesk PC you joined; its session ends.", LeaveInput, _leave),
            _native(desks, "connect", "Join a PC by the RustDesk ID and password you were given (its permanent or one-time password): a stock RustDesk "
                                      "PC on RustDesk's public server, one on a self-hosted server as ID@host?key=<key>, or one of our RigDesk PCs by the ID it "
                                      "shows. Nothing needs setting up in Rig or on the RigDesk page. One of our RigDesk PCs joins at once; any other PC is dialed only "
                                      "once the operator approves, with why, and that decision arrives as your next input. You then own its session alone; desk/screenshot, click, type and the rest act there; desk/leave ends it.", ConnectInput, _connect,
                    limit_s=CALL_LIMIT_S + 4 * adhoc.LOGIN_LIMIT_S),
            _native(desks, "pcs", "Every RigDesk PC in Rig: its client, whether it is up, whether it is assigned to you, who holds it now, whether you may run "
                                  "commands there, and its health; with pc, that PC's newest health sample in full.", PcsInput, _pcs, read_only=True),
            _native(desks, "shell", "Run a command on a RigDesk PC: named by pc, with no session and nothing on its screen, as SYSTEM (Windows) or root (Linux), "
                                    "or as its user, where the operator "
                                    "allowed you; unnamed, on the PC you joined. background starts a job; job reads one.", ShellInput, _shell,
                    limit_s=CALL_LIMIT_S + SHELL_LIMIT_S + 10),
            _native(desks, "request", "Ask the operator to let you join a PC now (kind join), or to assign a stock RustDesk PC to you (kind assign).", RequestInput, _request),
            _native(desks, "attachment", "An image the human sent you from the RigDesk viewer, as a PNG file you can read.", AttachmentInput, _attachment, read_only=True),
            *(_passed(desks, name, listed) for name, listed in desks.catalog().items() if name not in NATIVE),
            *(_served(desks, server, listed) for server in desks.pc_servers() for listed in server.tools))


def _tools(host: PluginHost) -> tuple[ToolDef, ...]:
    """A branch tool's run is given the Call alone, so each closes over the live Desks it reaches; rebuilt only when the catalog or a PC's servers change."""
    return host.live(Desks).tools(_built)


def _why_missing(host: PluginHost, name: str, listed: bool) -> str | None:
    branch, slash, tool = name.partition("/")
    desks = host.live(Desks)
    pc, dot, server = branch.partition(".")
    if dot and slash and pc in {entry["name"] for entry in desks.entries()}:
        opened = desks.pc_server(branch)
        if opened is None:
            return (f"no MCP server {server} is open on RigDesk PC {pc}: its servers reach only the agent holding it, while they run there, "
                    "and that agent is told each one's tools by name")
        return f"MCP server {server} on {pc} has no tool {tool!r}; it has " + ", ".join(each["name"] for each in opened.tools)
    if branch != BRANCH or not slash:
        return None
    if not host.live(Desks).catalog():
        return f"no RigDesk PC has been reached yet, so its tools are not known; desk/join works, and {name} shows once a PC answers"
    return f"RigDesk PCs offer no tool {tool!r}" + ("; tool_load `desk/` lists the ones they do" if listed else "")


def _capabilities(host: PluginHost) -> Mapping[str, str]:
    servers = {mcp_capability(server.key): f"MCP server {server.name} ({server.says}) on RigDesk PC {server.pc}, for the agent holding it."
               for server in host.live(Desks).pc_servers()}
    return {CAPABILITY: "Join a RigDesk PC with a share link and use its tools."} | servers


def _reach(host: PluginHost) -> Mapping[str, RigReach]:
    """A PC's servers load on request, `<pc>.<server>/` listing their tools, in the lists of the agent holding the PC alone: any other is shown none and refused."""
    desks = host.live(Desks)
    reach: dict[str, RigReach] = {BRANCH: "listed" if desks.for_fleet else "withheld"}
    for server in desks.pc_servers():
        reach[server.key] = "server"
    return reach


def _listing(host: PluginHost, capability: str) -> tuple[BranchTool, ...]:
    branch = capability.removeprefix("mcp:")
    if capability not in _capabilities(host):
        return ()
    return tuple(BranchTool(capability=mcp_capability(branch, defined.name.split("/", 1)[1]), name=defined.name.split("/", 1)[1], about=defined.description,
                            says_read_only=defined.read_only, held=None) for defined in _tools(host) if defined.name.split("/", 1)[0] == branch)


async def _decide(_host: PluginHost, _capability: str, _tools_named: tuple[str, ...]) -> tuple[str, ...]:
    return ()


class GrantRow(View):
    grantee: str = Field(description='"agent:<id>" or "graph:<name>", as rigdesk keeps it.')
    label: str = Field(description="The agent's title, or every agent in the graph; the id when the agent is gone.")
    graph: str | None = Field(description="The agent's home graph, or the graph granted; null for an agent Rig no longer has.")
    agent_id: str | None


async def _grant_row(host: PluginHost, grantee: str) -> GrantRow:
    kind, _, name = grantee.partition(":")
    if kind == "graph":
        return GrantRow(grantee=grantee, label=f"every agent in {name}", graph=name, agent_id=None)
    home = await host.agent_home(AgentId(name))
    if home is None:
        return GrantRow(grantee=grantee, label=f"{name} (gone)", graph=None, agent_id=name)
    seat = await host.seat_of(AgentId(name), home, "rigdesk")
    identity = None if isinstance(seat, RefusedOutcome) else await host.roster.identity(seat)  # type: ignore[arg-type]
    return GrantRow(grantee=grantee, label=identity.label if identity is not None and identity.label else name, graph=home, agent_id=name)


class PcServerRow(View):
    name: str
    key: str = Field(description="Its tools' branch for the holding agent: <pc>.<server>/<tool>.")
    says: str = Field(description="Its own words for itself: serverInfo's name and version.")
    tools: tuple[str, ...]
    since_ms: int = Field(description="When the holding agent's session opened it.")


class PcMcpRow(View):
    allowed: bool = Field(description="Rig's switch, the PC's mcp setting: off, Rig opens none of its servers.")
    on: bool | None = Field(description="The PC's own switch, as it last said; null while no agent holds the PC, since it looks for servers only then.")
    why: str | None = Field(description="Why no server is offered, or the last one that could not be opened.")
    servers: tuple[PcServerRow, ...] = Field(description="The servers open for the agent holding the PC now.")


class ShellSwitchRow(View):
    open: bool
    code: str | None = Field(description="Why it is closed: shell_off (the install line or the PC's settings), shell_paused (the person at the PC) or lane_down (a Linux PC's management lane does not answer).")
    words: str | None


class ShellGrantRow(GrantRow):
    level: ShellLevel = Field(description="user: as the signed-in user; system: with the highest local rights too (SYSTEM on Windows, root on Linux), and background jobs.")


class PcShellRow(View):
    switch: ShellSwitchRow | None = Field(description="The PC's own switch, as it last said; null for a RigDesk without the agent shell.")
    granted: tuple[ShellGrantRow, ...] = Field(description="Who may run commands there with no session; an agent holding the PC's session may as the user.")


class CheckRow(View):
    name: str
    state: Literal["warn", "fault", "stale"]
    words: str


class VolumeRow(View):
    name: str
    free_pct: float
    free_mb: int
    total_mb: int


class HealthFiguresRow(View):
    volumes: tuple[VolumeRow, ...]
    ram_used_pct: float | None
    cpu_pct: float | None
    build: str
    uptime_s: int
    pending_reboot: bool | None


class PcHealthRow(View):
    state: Literal["ok", "warn", "fault", "stale"] | None = Field(description="Rig's verdict on the newest sample; null while Rig holds none.")
    failing: tuple[CheckRow, ...]
    why: str | None = Field(description="Why samples are not being read, when they are not.")
    sample_ms: int | None
    rigdesk: str | None = Field(description="The RigDesk version that took the newest sample.")
    figures: HealthFiguresRow | None


class PcRow(View):
    name: str
    client: str
    url: str
    stock: bool = Field(description="A stock RustDesk PC, driven through rigdesk-bridge on this host; its share and holder buttons are on this page.")
    os: str | None = Field(description="windows or linux, as its agent API says; null until it answers, and for a stock PC.")
    state: str
    why: str | None
    down_since_ms: int | None
    holder: str | None = Field(description="human, agent or blocked while an agent owns the session; null with none.")
    owner: dict[str, str | None] | None = Field(description="The agent owning the session: id, title and graph.")
    owned_here: bool = Field(description="The owner joined through this Rig.")
    assigned: tuple[GrantRow, ...] = Field(description="Who holds a standing grant to it; a stock PC only.")
    mcp: PcMcpRow = Field(description="The MCP servers running on the PC, as they reach the agent holding it.")
    shell: PcShellRow | None = Field(description="A RigDesk PC's agent shell: its switch and who may use it; null for a stock PC.")
    health: PcHealthRow | None = Field(description="A RigDesk PC's health; null for a stock PC.")
    adhoc: dict[str, str] | None = Field(default=None, description="A PC an agent joined by RustDesk ID and password: id, and via, how it was reached; null otherwise.")


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
    for_fleet: bool = Field(description="desk/* is open to every agent; when false, an agent assigned a PC also needs desk among its mcp_servers.")


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
    host = request.host()
    desks = host.live(Desks)
    rows = desks.pcs()
    named: set[str] = {each for row in rows for each in (*row["assigned"], *desks.shell_granted(row["name"]))}
    labels = {grantee: await _grant_row(host, grantee) for grantee in named}

    def shell(row: dict[str, Any]) -> dict[str, Any] | None:
        kept = row["shell"]
        return None if kept is None else kept | {"granted": tuple(ShellGrantRow(**labels[grantee].model_dump(), level=level) for grantee, level in kept["granted"].items())}
    return PcsView(pcs=tuple(PcRow.model_validate(row | {"assigned": tuple(labels[each] for each in row["assigned"]), "shell": shell(row)}) for row in rows),
                   id_server=await asyncio.to_thread(_id_server_row, desks), for_fleet=desks.for_fleet)


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
        return request.refused(DOWN, f"{body.pc}'s rigdesk-bridge did not answer: {failure(failed)}")


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
    """Gives or takes away one standing grant to a stock PC: to any agent, or every agent in a graph."""
    body = request.params.body
    desks = request.host().live(Desks)
    try:
        changed = await (desks.assign if body.granted else desks.unassign)(body.pc, body.grantee)
    except DeskRefused as refused:
        return request.refused(REFUSED, str(refused))
    return AssignDone(changed=changed)


class ShellGrantBody(BaseModel):
    model_config = ConfigDict(frozen=True, extra="forbid", defer_build=True)
    pc: str
    grantee: Grantee
    level: ShellLevel | None = Field(description="user or system; null takes the grant away.")


class ShellGrantParams(Params):
    body: ShellGrantBody


@endpoint(ShellGrantParams, OPERATOR)
async def post_shell_grant(request: Request[ShellGrantParams]) -> AssignDone | RefusedOutcome:
    """Lets one agent, or every agent in a graph, run commands on a RigDesk PC's agent shell as the user or with the highest local rights (SYSTEM on Windows, root on Linux), or takes that away."""
    body = request.params.body
    try:
        changed = await request.host().live(Desks).allow_shell(body.pc, body.grantee, body.level)
    except DeskRefused as refused:
        return request.refused(REFUSED, str(refused))
    return AssignDone(changed=changed)


class PcParams(Params):
    pc: DeskName


class HealthView(View):
    sample: dict[str, Any] | None = Field(description="The newest sample in full, as the PC took it; null while Rig holds none.")
    series: tuple[dict[str, Any], ...] = Field(description="The week's free space per volume, in percent, oldest first: {at_ms, free_pct: {volume: pct}}.")


@endpoint(PcParams, OPERATOR)
async def get_health(request: Request[PcParams]) -> HealthView:
    """One PC's newest health sample and its week of free space, for its opened row."""
    history = request.host().live(Desks).history(request.params.pc)
    return HealthView(sample=figures_json(history[-1]) if history else None, series=tuple(series(history)))


class ShellRan(View):
    at_ms: int
    who: str = Field(description="The agent, as title (id).")
    graph: str | None
    run_as: Literal["system", "user"] | None = Field(alias="as")
    command: str
    outcome: str = Field(description="exit N, timed out, started, refused: <code>, failed.")
    took_ms: int | None
    job: str | None


class ShellRanView(View):
    ran: tuple[ShellRan, ...] = Field(description="The PC's newest agent-shell commands from its own journal, newest first.")


@endpoint(PcParams, OPERATOR)
async def get_shell_ran(request: Request[PcParams]) -> ShellRanView | RefusedOutcome:
    """What agents ran on a PC's agent shell, read from the PC's journal when its row is open."""
    pc = request.params.pc
    try:
        ran = await request.host().live(Desks).shell_ran(pc)
    except DeskRefused as refused:
        return request.refused(REFUSED, str(refused))
    except (aiohttp.ClientError, TimeoutError, OSError) as failed:
        return request.refused(DOWN, f"{pc} did not answer: {failure(failed)}")
    return ShellRanView(ran=tuple(ShellRan.model_validate(each) for each in ran))


class ScreenshotBody(BaseModel):
    model_config = ConfigDict(frozen=True, extra="forbid", defer_build=True)
    pc: DeskName


class ScreenshotParams(Params):
    body: ScreenshotBody


class Screenshot(View):
    mime: Literal["image/png"]
    data: str = Field(description="The PNG, base64.")
    taken_ms: int


@endpoint(ScreenshotParams, OPERATOR)
async def post_screenshot(request: Request[ScreenshotParams]) -> Screenshot | RefusedOutcome:
    """One picture of a PC's screen, taken when the operator asks and kept nowhere here; the PC journals it."""
    pc = request.params.body.pc
    try:
        png = await request.host().live(Desks).frame(pc)
    except DeskRefused as refused:
        return request.refused(REFUSED, str(refused))
    except (aiohttp.ClientError, TimeoutError, OSError) as failed:
        return request.refused(DOWN, f"{pc} did not answer: {failure(failed)}")
    return Screenshot(mime="image/png", data=base64.b64encode(png).decode("ascii"), taken_ms=now_ms())


TOKENS = Path("~/.rig/rigdesk")

type PcName = Annotated[str, StringConstraints(pattern=r"^[a-z0-9][a-z0-9-]{0,62}$")]
type Token = Annotated[str, StringConstraints(pattern=r"^[A-Za-z0-9_-]{32,128}$")]


class TokenBody(BaseModel):
    model_config = ConfigDict(frozen=True, extra="forbid", defer_build=True)
    pc: PcName
    token: Token
    lane: bool = False


class TokenParams(Params):
    body: TokenBody


class TokenKept(View):
    token_file: str = Field(description="Where the token is kept, readable by this user alone: name it as the PC's token_file, or for a lane token its lane_token_file, "
                                        "in plugins.rigdesk.pcs.")


def _kept(path: Path, token: str) -> None:
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    if path.exists():
        os.chmod(path, 0o600)
    replace_file(path, token.encode(), new_mode=0o600)


@endpoint(TokenParams, OPERATOR)
async def post_token(request: Request[TokenParams]) -> TokenKept:
    """Keeps a PC's agent-API token, or with lane a Linux PC's management-lane token, the one its install line sets, in a 0600 file of its own; a second call for
    the same PC replaces it, as rerunning the line rotates it."""
    body = request.params.body
    path = TOKENS / f"{body.pc}.lane.token" if body.lane else TOKENS / f"{body.pc}.token"
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


CONTRIBUTION = ContributionDescriptor(
    id=ID, release="0.1.0", live=Live(Desks, Desks.settle, Desks.end), settings=RigdeskSettings, states=REPORTED,
    tool_branch=ToolBranch(_tools, _why_missing, _capabilities, _reach, _listing, _decide),
    refusal_codes={REFUSED: HTTPStatus.CONFLICT, DOWN: HTTPStatus.SERVICE_UNAVAILABLE, UNSHARED: HTTPStatus.FORBIDDEN, SHELL_NOT_ALLOWED: HTTPStatus.FORBIDDEN,
                   SYSTEM_NOT_GRANTED: HTTPStatus.FORBIDDEN, **{f"{ID}.{code}": status for code, status in PC_SHELL_CODES.items()}},
    provisions=(provider(REQUEST_CONFIRMED, _request_confirmed), provider(CONNECT_CONFIRMED, _connect_confirmed)),
    routes=(RouteContribution("GET", "pcs", get_pcs), RouteContribution("GET", "session", get_session), RouteContribution("POST", "token", post_token),
            RouteContribution("POST", "viewer", post_viewer), RouteContribution("POST", "assign", post_assign),
            RouteContribution("POST", "pc", post_pc), RouteContribution("POST", "screenshot", post_screenshot),
            RouteContribution("POST", "shell_grant", post_shell_grant), RouteContribution("GET", "health", get_health), RouteContribution("GET", "shell_ran", get_shell_ran)))
