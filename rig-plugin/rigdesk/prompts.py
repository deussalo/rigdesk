"""The person at a PC answers an agent's desk/request on the PC itself, while the operator's ask stands in Rig: the PC's own accept prompt
names the agent, and its Allow or Deny answers the operator's ask as the operator's would. The first answer wins; the other side is closed.

Either PC is sent POST /offer with the ask and a nonce Rig keeps to itself, over Rig's bearer: a RigDesk PC shows the agent in its connection
manager; a stock PC's rigdesk-bridge logs in to it with no password, so it shows RustDesk's own accept prompt, and on Allow that connection
carries the session, so no password is used. The answer comes back on the PC's /events stream, and counts only when it names that PC, that ask
and that nonce. It is taken only while no agent holds a session on the PC, so no agent can click it.

On a stock PC the operator's Accept does not take the prompt away until a RustDesk login has actually succeeded: Rig tries the saved password
once, and if the PC refuses it, or none is kept, the prompt stays up and the agent is told the person there must Allow it. Their Allow then
joins the agent over the connection they allowed; their Dismiss, or a prompt that lapses, is told too.

One tap: a phone named in Rig Assistant (stock.assistant) is asked through that link instead, as one notification that shows with RigDesk's
screen sharing off. Its Accept starts sharing and the phone says ready, with the ask and nonce; only then is the bridge sent the offer, marked
accepted, so its knock names the ask under Rig's signature and the phone lets it in on that Accept with no second Allow. Deny, a failure, the
operator's Deny, a login on the operator's Accept, a withdrawal and the lapse each take the notification away; an unanswered one is shown again, and after the last lapse the
operator's ask is withdrawn and the agent told. With the phone's link down the bridge's knock asks as before."""
from __future__ import annotations

import asyncio
import hmac
import secrets
from collections.abc import Awaitable
from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Any, Literal, TypedDict

import aiohttp
from pydantic import TypeAdapter

from core.sdk.base import event_log, JsonObject, now_ms
from core.sdk.host import PluginHost, PROVIDED, Provision, scoped, TASKS
from core.sdk.model import Refused, SeatId

if TYPE_CHECKING:
    from .desks import Desks, PcLink

_LOG = event_log(__name__)
OPEN_EVERY_S = 2.0
SHOWS = 3
# The operator's ask stops waiting a moment before the Accept's then runs and marks the operator's yes on the prompt, so a prompt is taken away only once
# the ask has not waited for this many looks in a row.
SETTLE_LOOKS = 3
# A phone's one-tap notification lapses as a PC's prompt does, after 3 minutes, and is shown again.
ONE_TAP_LAPSE_S = 180
# The operator's Accept goes here, as __init__.REQUEST_CONFIRMED names it.
REQUEST_CONFIRMED = "rigdesk.request_confirmed"


# What rig-assistant carries to and from the phone; it declares the same shapes, so neither plugin imports the other.
class JoinAsked(TypedDict):
    phone: str
    ask: str
    nonce: str
    agent_id: str
    agent_title: str
    graph: str
    operator: str
    lapse_s: int


class JoinEnded(TypedDict):
    phone: str
    ask: str


class PhoneJoinSaid(TypedDict):
    phone: str
    ask: str
    nonce: str
    said: Literal["ready", "deny", "failed"]
    reason: str


JOIN_REQUEST = Provision("rig-assistant.join_request", TypeAdapter(JoinAsked), TypeAdapter(str))
JOIN_END = Provision("rig-assistant.join_end", TypeAdapter(JoinEnded), TypeAdapter(str))
PHONE_JOIN = Provision("rigdesk.phone_join", TypeAdapter(PhoneJoinSaid), TypeAdapter(str))


@dataclass
class Prompt:
    pc: str
    ask: str
    seat: SeatId
    agent: str
    graph: str
    title: str
    nonce: str = field(default_factory=lambda: secrets.token_hex(16))
    task: asyncio.Task[None] | None = None
    offered: asyncio.Future[str] | None = None
    # Named as the consent states (CONSENT.md): the operator said yes while the prompt is up, and how Rig's saved password fares; until a login
    # succeeds the prompt stays up, and the person's answer joins the agent or is told to it.
    operator_said: bool = False
    password: Literal["trying", "refused", "unreached", "none"] | None = None
    # When the person dismissed it after the operator's yes: denied by the person, which a password login still under way does not override.
    denied_ms: int | None = None
    # The operator's ask as its Accept's then names it, which a one-tap request withdraws after its last lapse.
    then: JsonObject | None = None
    # One tap: what the phone said of its notification, and whether its person accepted, so the bridge's offer names the ask.
    phone_said: asyncio.Future[PhoneJoinSaid] | None = None
    accepted: bool = False


def start(desks: Desks, pc: str, ask: str, seat: SeatId, agent: str, graph: str, title: str, then: JsonObject | None = None) -> None:
    """Shows the agent's request on the PC too, where Rig can take the PC's answer for the operator's (a Rig with PluginHost.confirm_answered)."""
    if not hasattr(desks.host, "confirm_answered") or desks.held_on(pc) is not None:
        return
    desks.unshown.pop(pc, None)
    prompt = desks.prompts[ask] = Prompt(pc, ask, seat, agent, graph, title, then=then)
    prompt.task = TASKS.spawn(f"rigdesk-prompt-{ask}", _prompting(desks, prompt))


def operator_said(desks: Desks, ask: str) -> Prompt | None:
    """The operator accepted the ask: its prompt, kept up while Rig tries the saved password and until a login succeeds; None when none is up."""
    prompt = desks.prompts.get(ask)
    if prompt is None or prompt.task is None or prompt.task.done():
        return None
    prompt.operator_said, prompt.password = True, "trying"
    return prompt


def denied_by_person(prompt: Prompt | None) -> int | None:
    """When the person dismissed this prompt after the operator's yes; None while they have not."""
    return None if prompt is None else prompt.denied_ms


def waiting(desks: Desks, prompt: Prompt | None, pc: str, why: str, password: Literal["refused", "unreached", "none"]) -> str:
    """The operator accepted, but no login succeeded: what the agent is told while the person at the PC is asked, or why it is not asked.
    Nothing at an unreached PC can show the request, so the caller takes its prompt away."""
    from .desks import DeskRefused  # noqa: PLC0415 (desks imports this module)
    if password == "unreached":
        return (f"The operator accepted, but you have not joined {pc}. {why} "
                f"Your request can't show on {pc} until that service runs. Once it runs, desk/request asks again.")
    if prompt is None or desks.prompts.get(prompt.ask) is not prompt or prompt.task is None or prompt.task.done():
        unshown = desks.unshown.get(pc)
        raise DeskRefused(f"{why}, and the person at {pc} is not asked on the device: {unshown or 'no prompt for this request is up there'}")
    prompt.password = password
    return (f"The operator accepted, but you have not joined {pc}: {why}, so the person at {pc} must Allow your request on the device, where it shows now. "
            "Nothing is joined until they do; your next input says when the session is up, or that they dismissed it or it lapsed.")


def end(desks: Desks, ask: str) -> None:
    """Rig answered the ask: the PC's prompt is taken away."""
    prompt = desks.prompts.pop(ask, None)
    if prompt is not None and prompt.task is not None:
        prompt.task.cancel()


async def end_all(desks: Desks) -> None:
    tasks = [prompt.task for prompt in desks.prompts.values() if prompt.task is not None]
    desks.prompts.clear()
    for task in tasks:
        task.cancel()
    await asyncio.gather(*tasks, return_exceptions=True)


def answered(desks: Desks, link: PcLink, event: dict[str, Any]) -> None:
    """A RigDesk PC's offer event: the answer of the person there, for the prompt shown on that PC for that ask with that nonce, else nothing."""
    prompt = desks.prompts.get(str(event.get("ask") or ""))
    nonce, answer = event.get("nonce"), event.get("answer")
    if (prompt is None or prompt.pc != link.name or prompt.offered is None or prompt.offered.done() or not isinstance(nonce, str)
            or not isinstance(answer, str) or not hmac.compare_digest(nonce.encode(), prompt.nonce.encode())):
        return
    prompt.offered.set_result(answer)


async def _prompting(desks: Desks, prompt: Prompt) -> None:
    try:
        said = None
        for _ in range(SHOWS):
            said = await _while_open(desks, prompt, _shown(desks, prompt))
            if said != "lapsed":
                break
        if said in ("allow", "deny"):
            await _settled(desks, prompt, said)
        elif prompt.operator_said:
            await _unjoined(desks, prompt, said)
        elif said == "lapsed" and desks.assistant_of(prompt.pc) is not None:
            await _lapsed(desks, prompt)
        elif said not in (None, "lapsed"):
            desks.unshown[prompt.pc] = said
            _LOG.warning("rigdesk_prompt_unshown", f"RigDesk {prompt.pc}: the request {prompt.ask} could not be shown on the PC: {said}", pc=prompt.pc)
    except asyncio.CancelledError:
        raise
    except Exception as failed:  # noqa: BLE001
        _LOG.warning("rigdesk_prompt_failed", f"RigDesk {prompt.pc}: the request {prompt.ask} shown on the PC failed: {type(failed).__name__}: {failed}", pc=prompt.pc)
    finally:
        if desks.prompts.get(prompt.ask) is prompt:
            desks.prompts.pop(prompt.ask, None)


async def _while_open(desks: Desks, prompt: Prompt, shown: Awaitable[str]) -> str | None:
    """What the PC said, or None once the operator's ask no longer waits or an agent holds the PC: the prompt is then taken away."""
    task = scoped(f"rigdesk-prompt-shown-{prompt.ask}", shown)
    unwaited = 0
    try:
        while not task.done():
            await asyncio.wait({task}, timeout=OPEN_EVERY_S)
            if task.done():
                break
            if desks.held_on(prompt.pc) is not None:
                return None
            unwaited = 0 if prompt.operator_said or await desks.host.confirming(prompt.seat, prompt.ask) is not None else unwaited + 1
            if unwaited >= SETTLE_LOOKS:
                return None
        return task.result()
    finally:
        if not task.done():
            task.cancel()
            await asyncio.gather(task, return_exceptions=True)


def _shown(desks: Desks, prompt: Prompt) -> Awaitable[str]:
    phone = desks.assistant_of(prompt.pc)
    return _offered(desks, prompt) if phone is None else _one_tap(desks, prompt, phone)


async def _one_tap(desks: Desks, prompt: Prompt, phone: str) -> str:
    """The request as the phone's one notification; its Accept, once the phone is ready, sends the bridge's offer marked accepted, whose answer
    is this one's. Said as the PC's offer is: allow, deny, lapsed, or why it could not be shown. The notification goes when this ends."""
    prompt.phone_said = asyncio.get_running_loop().create_future()
    asked = JoinAsked(phone=phone, ask=prompt.ask, nonce=prompt.nonce, agent_id=prompt.agent, agent_title=prompt.title, graph=prompt.graph,
                      operator=await desks.operator_name(), lapse_s=ONE_TAP_LAPSE_S)
    try:
        await PROVIDED.ask(JOIN_REQUEST, asked)
    except Refused as unreached:
        _LOG.info("rigdesk_one_tap_unreached", f"RigDesk {prompt.pc}: Rig Assistant could not show {prompt.ask} on {phone} ({unreached.rendered}); the bridge's knock asks instead",
                  pc=prompt.pc)
        return await _offered(desks, prompt)
    try:
        try:
            said = await asyncio.wait_for(asyncio.shield(prompt.phone_said), ONE_TAP_LAPSE_S)
        except TimeoutError:
            return "lapsed"
        if said["said"] == "deny":
            return "deny"
        if said["said"] == "failed":
            return f"refused: {said['reason'] or 'the phone could not start sharing'}"
        prompt.accepted = True
        return await _offered(desks, prompt)
    finally:
        prompt.phone_said = None
        try:
            await PROVIDED.ask(JOIN_END, JoinEnded(phone=phone, ask=prompt.ask))
        except Refused as unended:
            _LOG.warning("rigdesk_one_tap_unended", f"RigDesk {prompt.pc}: {prompt.ask} could not be taken off {phone}, it lapses there: {unended.rendered}", pc=prompt.pc)


async def phone_said(host: PluginHost, said: PhoneJoinSaid) -> str:
    """rig-assistant passes on what a phone said of a one-tap request: it counts only for that request's prompt, from the phone it was shown on,
    with its nonce, while the prompt waits for it."""
    from .desks import Desks  # noqa: PLC0415 (desks imports this module)
    desks = host.live(Desks)
    prompt = desks.prompts.get(said["ask"])
    waiting = None if prompt is None else prompt.phone_said
    if (prompt is None or waiting is None or waiting.done() or desks.assistant_of(prompt.pc) != said["phone"]
            or not hmac.compare_digest(said["nonce"].encode(), prompt.nonce.encode())):
        return "not waited for"
    waiting.set_result(said)
    return "taken"


async def _lapsed(desks: Desks, prompt: Prompt) -> None:
    """Nobody answered the phone's notification, shown each time it lapsed: the operator's ask is withdrawn, and the agent told."""
    if prompt.then is not None:
        await desks.host.route_ended(prompt.seat, REQUEST_CONFIRMED, prompt.then)
    words = (f"RigDesk: your desk/request for {prompt.pc} lapsed: nobody answered it on the phone within {SHOWS * ONE_TAP_LAPSE_S // 60} minutes, "
             "so the operator's ask is withdrawn and nothing was joined. desk/request asks again.")
    await desks.tell_seat(prompt.pc, prompt.agent, prompt.seat, words, f"lapsed:{prompt.ask}")


async def _offered(desks: Desks, prompt: Prompt) -> str:
    """The agent in the PC's own prompt, answered by an offer event; taken away with POST /offer/end when Rig answers first."""
    url, headers = desks.offer_at(prompt.pc)
    prompt.offered = asyncio.get_running_loop().create_future()
    body = {"agent": {"id": prompt.agent, "title": prompt.title, "graph": prompt.graph}, "ask": prompt.ask, "nonce": prompt.nonce}
    if prompt.accepted:
        body["accepted"] = True
    async with desks.http.post(f"{url}/offer", json=body, headers=headers, timeout=aiohttp.ClientTimeout(total=15)) as answer:
        said: dict[str, Any] = await answer.json(content_type=None) if answer.content_type == "application/json" else {}
        if answer.status != 200:
            return f"{said.get('code') or f'HTTP {answer.status}'}: {said.get('error') or 'refused'}"
    try:
        return await prompt.offered
    except asyncio.CancelledError:
        try:
            async with desks.http.post(f"{url}/offer/end", json={"ask": prompt.ask}, headers=headers, timeout=aiohttp.ClientTimeout(total=15)):
                pass
        except (aiohttp.ClientError, TimeoutError, OSError) as failed:
            _LOG.warning("rigdesk_prompt_unended", f"RigDesk {prompt.pc}: the request {prompt.ask} could not be taken off the PC, it lapses there: {failed}", pc=prompt.pc)
        raise


async def _settled(desks: Desks, prompt: Prompt, said: str) -> None:
    """The person at the PC answered first: the operator's ask is answered as they did. Taken off the open prompts first, so the Then's end()
    does not cancel this answer while it runs."""
    desks.prompts.pop(prompt.ask, None)
    if desks.held_on(prompt.pc) is not None:
        _LOG.warning("rigdesk_prompt_held", f"RigDesk {prompt.pc}: the answer to {prompt.ask} on the PC was ignored, as an agent holds the PC", pc=prompt.pc)
        if prompt.operator_said:
            await _unjoined(desks, prompt, None)
        return
    if prompt.operator_said:
        await _answered_after_accept(desks, prompt, said)
        return
    choice = "accept" if said == "allow" else "deny"
    if said == "allow":
        desks.person_answered.add(prompt.ask)
    try:
        done = await desks.host.confirm_answered(prompt.seat, prompt.ask, choice, f"rigdesk:{prompt.pc}")  # type: ignore[attr-defined]
    except BaseException:
        desks.person_answered.discard(prompt.ask)
        raise
    if done != "answered":
        desks.person_answered.discard(prompt.ask)
        return
    words = (f"RigDesk: the person at {prompt.pc} {'allowed' if said == 'allow' else 'refused'} your desk/request on the PC itself, "
             "so they answered it rather than the operator.")
    await desks.tell_seat(prompt.pc, prompt.agent, prompt.seat, words, f"prompt:{prompt.ask}")


async def _answered_after_accept(desks: Desks, prompt: Prompt, said: str) -> None:
    """The person at a stock PC answered after the operator's Accept found no login: their Allow joins the agent over the connection they allowed."""
    from .desks import DeskRefused  # noqa: PLC0415 (desks imports this module)
    pc = prompt.pc
    if said == "deny":
        prompt.denied_ms = now_ms()
        if prompt.password == "trying":
            return  # the operator's Accept, still logging in, drops its login and tells the agent
        words = (f"RigDesk {pc}: denied by the person at {_clock(prompt.denied_ms)}: they dismissed your request on the device, so you did not join {pc}, "
                 "though the operator accepted it. desk/request asks again.")
    else:
        identity = await desks.host.roster.identity(prompt.seat)
        if identity is None or identity.agent != prompt.agent:
            _LOG.warning("rigdesk_prompt_gone", f"RigDesk {pc}: the person allowed {prompt.ask}, but {prompt.agent} no longer stands at its seat, so nobody joined", pc=pc)
            return
        try:
            await desks.join_approved(pc, prompt.agent, prompt.graph, prompt.seat.node, prompt.title, prompt.ask)
        except (DeskRefused, aiohttp.ClientError, TimeoutError, OSError) as failed:
            mine = desks.held_by(prompt.agent, prompt.graph)
            if mine is not None and mine.pc == pc:
                return  # the operator's Accept logged in meanwhile, and said so
            words = f"RigDesk: the person at {pc} allowed you on the device, but you did not join {pc}: {failed}. desk/request asks again."
        else:
            words = (f"RigDesk: the person at {pc} allowed you on the device, so you joined {pc} over the connection they allowed, with no password. "
                     "Its session is yours until you desk/leave or the human ends the share. desk/<tool> acts there.")
    await desks.tell_seat(pc, prompt.agent, prompt.seat, words, f"prompt:{prompt.ask}")


async def _unjoined(desks: Desks, prompt: Prompt, said: str | None) -> None:
    """The operator accepted and no login succeeded, and the device's prompt ended with no Allow: the agent is told it did not join, and why."""
    pc, holder = prompt.pc, desks.held_on(prompt.pc)
    if holder is not None and holder.agent == prompt.agent:
        return
    why = (f"{holder.title} joined {pc} while the person there was asked, so the prompt was taken away" if holder is not None else
           "nobody answered the prompt on the device by " + _clock(now_ms()) if said in (None, "lapsed") else
           f"the device could not ask the person there: {said}")
    await desks.tell_seat(pc, prompt.agent, prompt.seat, f"RigDesk: you did not join {pc}, though the operator accepted it: {why}. desk/request asks again.",
                          f"prompt:{prompt.ask}")


def _clock(at_ms: int) -> str:
    import time  # noqa: PLC0415
    return time.strftime("%H:%M:%S", time.localtime(at_ms / 1000))
