"""The person at a PC answers an agent's desk/request on the PC itself, while the operator's ask stands in Rig: the PC's own accept prompt
names the agent, and its Allow or Deny answers the operator's ask as the operator's would. The first answer wins; the other side is closed.

Either PC is sent POST /offer with the ask and a nonce Rig keeps to itself, over Rig's bearer: a RigDesk PC shows the agent in its connection
manager; a stock PC's rigdesk-bridge logs in to it with no password, so it shows RustDesk's own accept prompt, and on Allow that connection
carries the session, so no password is used. The answer comes back on the PC's /events stream, and counts only when it names that PC, that ask
and that nonce. It is taken only while no agent holds a session on the PC, so no agent can click it.

On a stock PC the operator's Accept does not take the prompt away until a RustDesk login has actually succeeded: Rig tries the saved password
once, and if the PC refuses it, or none is kept, the prompt stays up and the agent is told the person there must Allow it. Their Allow then
joins the agent over the connection they allowed; their Dismiss, or a prompt that lapses, is told too."""
from __future__ import annotations

import asyncio
import hmac
import secrets
from collections.abc import Awaitable
from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Any

import aiohttp

from core.sdk.base import event_log
from core.sdk.host import scoped, TASKS
from core.sdk.model import SeatId

if TYPE_CHECKING:
    from .desks import Desks, PcLink

_LOG = event_log(__name__)
OPEN_EVERY_S = 2.0
SHOWS = 3
# The operator's ask stops waiting a moment before the Accept's then runs and marks the prompt accepted, so a prompt is taken away only once
# the ask has not waited for this many looks in a row.
SETTLE_LOOKS = 3


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
    # The operator accepted and no login has succeeded: the prompt stays up, and the person's answer joins the agent or is told to it.
    accepted: bool = False


def start(desks: Desks, pc: str, ask: str, seat: SeatId, agent: str, graph: str, title: str) -> None:
    """Shows the agent's request on the PC too, where Rig can take the PC's answer for the operator's (a Rig with PluginHost.confirm_answered)."""
    if not hasattr(desks.host, "confirm_answered") or desks.held_on(pc) is not None:
        return
    prompt = desks.prompts[ask] = Prompt(pc, ask, seat, agent, graph, title)
    prompt.task = TASKS.spawn(f"rigdesk-prompt-{ask}", _prompting(desks, prompt))


def accepted(desks: Desks, ask: str) -> Prompt | None:
    """The operator accepted the ask: its prompt, kept up until a login succeeds; None when none is up."""
    prompt = desks.prompts.get(ask)
    if prompt is None or prompt.task is None or prompt.task.done():
        return None
    prompt.accepted = True
    return prompt


def waiting(desks: Desks, prompt: Prompt | None, pc: str, why: str) -> str:
    """The operator accepted, but no login succeeded: what the agent is told while the person at the PC is asked, or why it is not asked."""
    from .desks import DeskRefused  # noqa: PLC0415 (desks imports this module)
    if prompt is None or desks.prompts.get(prompt.ask) is not prompt or prompt.task is None or prompt.task.done():
        unshown = desks.unshown.get(pc)
        raise DeskRefused(f"{why}, and the person at {pc} is not asked on the device: {unshown or 'no prompt for this request is up there'}")
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
            said = await _while_open(desks, prompt, _offered(desks, prompt))
            if said != "lapsed":
                break
        if said in ("allow", "deny"):
            await _settled(desks, prompt, said)
        elif prompt.accepted:
            await _unjoined(desks, prompt, said)
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
            unwaited = 0 if prompt.accepted or await desks.host.confirming(prompt.seat, prompt.ask) is not None else unwaited + 1
            if unwaited >= SETTLE_LOOKS:
                return None
        return task.result()
    finally:
        if not task.done():
            task.cancel()
            await asyncio.gather(task, return_exceptions=True)


async def _offered(desks: Desks, prompt: Prompt) -> str:
    """The agent in the PC's own prompt, answered by an offer event; taken away with POST /offer/end when Rig answers first."""
    url, headers = desks.offer_at(prompt.pc)
    prompt.offered = asyncio.get_running_loop().create_future()
    body = {"agent": {"id": prompt.agent, "title": prompt.title, "graph": prompt.graph}, "ask": prompt.ask, "nonce": prompt.nonce}
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
        if prompt.accepted:
            await _unjoined(desks, prompt, None)
        return
    if prompt.accepted:
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
        words = f"RigDesk: the person at {pc} dismissed your request on the device, so you did not join {pc}, though the operator accepted it. desk/request asks again."
    else:
        identity = await desks.host.roster.identity(prompt.seat)
        if identity is None or identity.agent != prompt.agent:
            _LOG.warning("rigdesk_prompt_gone", f"RigDesk {pc}: the person allowed {prompt.ask}, but {prompt.agent} no longer stands at its seat, so nobody joined", pc=pc)
            return
        try:
            await desks.join_approved(pc, prompt.agent, prompt.graph, prompt.seat.node, prompt.title, prompt.ask)
        except (DeskRefused, aiohttp.ClientError, TimeoutError, OSError) as failed:
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
           "nobody answered the prompt on the device" if said in (None, "lapsed") else
           f"the device could not ask the person there: {said}")
    await desks.tell_seat(pc, prompt.agent, prompt.seat, f"RigDesk: you did not join {pc}, though the operator accepted it: {why}. desk/request asks again.",
                          f"prompt:{prompt.ask}")
