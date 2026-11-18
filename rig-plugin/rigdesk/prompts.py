"""The person at a PC answers an agent's desk/request on the PC itself, while the operator's ask stands in Rig: the PC's own accept prompt
names the agent, and its Allow or Deny answers the operator's ask as the operator's would. The first answer wins; the other side is closed.

Either PC is sent POST /offer with the ask and a nonce Rig keeps to itself, over Rig's bearer: a RigDesk PC shows the agent in its connection
manager; a stock PC's rigdesk-bridge logs in to it with no password, so it shows RustDesk's own accept prompt, and on Allow that connection
carries the session, so no password is used. The answer comes back on the PC's /events stream, and counts only when it names that PC, that ask
and that nonce. It is taken only while no agent holds a session on the PC, so no agent can click it."""
from __future__ import annotations

import asyncio
import hmac
import secrets
from collections.abc import Awaitable
from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Any

import aiohttp

from core.base import event_log
from core.model import SeatId
from core.tasks import TASKS, scoped

if TYPE_CHECKING:
    from .desks import Desks, PcLink

_LOG = event_log(__name__)
OPEN_EVERY_S = 2.0
SHOWS = 3


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


def start(desks: Desks, pc: str, ask: str, seat: SeatId, agent: str, graph: str, title: str) -> None:
    """Shows the agent's request on the PC too, where Rig can take the PC's answer for the operator's (a Rig with PluginHost.confirm_answered)."""
    if not hasattr(desks.host, "confirm_answered") or desks.held_on(pc) is not None:
        return
    prompt = desks.prompts[ask] = Prompt(pc, ask, seat, agent, graph, title)
    prompt.task = TASKS.spawn(f"rigdesk-prompt-{ask}", _prompting(desks, prompt))


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
        elif said not in (None, "lapsed"):
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
    try:
        while not task.done():
            await asyncio.wait({task}, timeout=OPEN_EVERY_S)
            if not task.done() and (desks.held_on(prompt.pc) is not None or await desks.host.confirming(prompt.seat, prompt.ask) is None):
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
