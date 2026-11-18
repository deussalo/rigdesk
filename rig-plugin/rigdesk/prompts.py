"""The person at a PC answers an agent's desk/request on the PC itself, while the operator's ask stands in Rig: the PC's own accept prompt
names the agent, and its Allow or Deny answers the operator's ask as the operator's would. The first answer wins; the other side is closed.

A stock PC is knocked on by a rigdesk-bridge --knock that Rig starts for this one ask, which logs in with no password, so the PC shows RustDesk's
accept prompt, and says on its stdout what the person answered. A RigDesk PC is sent POST /offer with the ask and a nonce Rig keeps to itself,
over Rig's bearer, and shows the agent in its connection manager; the answer comes back on that PC's /events stream, and counts only when it
names that PC, that ask and that nonce. Either way the answer is taken only while no agent holds a session on the PC, so no agent can click it."""
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
    from .desks import Desks, PcLink, StockRemote

_LOG = event_log(__name__)
OPEN_EVERY_S = 2.0
SHOWS = 3
KNOCK_SAID = {"allowed": "allow", "dismissed": "deny"}


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
        stock, said = desks.stock_of(prompt.pc), None
        for _ in range(SHOWS):
            said = await _while_open(desks, prompt, _knocked(desks, prompt, stock) if stock is not None else _offered(desks, prompt))
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


async def _knocked(desks: Desks, prompt: Prompt, stock: StockRemote) -> str:
    """A stock PC: RustDesk's own accept prompt, from a bridge that logs in with no password. Lapsed only when the prompt was shown."""
    bridge, *route = desks.bridge_argv(stock)
    process = await asyncio.create_subprocess_exec(bridge, "--knock", "--name", f"{prompt.title} (Rig agent)", *route,
                                                   stdin=asyncio.subprocess.DEVNULL, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.DEVNULL)
    shown, last = False, "it ended without an answer"
    try:
        assert process.stdout is not None
        async for raw in process.stdout:
            line = raw.decode(errors="replace").strip()
            if line == "waiting":
                shown = True
            elif line in KNOCK_SAID:
                return KNOCK_SAID[line]
            elif line.startswith("lapsed"):
                return "lapsed" if shown else line
            elif line:
                last = line
        return last
    finally:
        await _stopped(process)


async def _stopped(process: asyncio.subprocess.Process) -> None:
    if process.returncode is not None:
        return
    process.terminate()
    try:
        await asyncio.wait_for(asyncio.shield(process.wait()), 5)
    except TimeoutError:
        process.kill()


async def _offered(desks: Desks, prompt: Prompt) -> str:
    """A RigDesk PC: the agent in its connection manager, answered by an offer event; taken away with POST /offer/end when Rig answers first."""
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
    done = await desks.host.confirm_answered(prompt.seat, prompt.ask, choice, f"rigdesk:{prompt.pc}")  # type: ignore[attr-defined]
    if done != "answered":
        return
    words = (f"RigDesk: the person at {prompt.pc} {'allowed' if said == 'allow' else 'refused'} your desk/request on the PC itself, "
             "so they answered it rather than the operator.")
    await desks.tell_seat(prompt.pc, prompt.agent, prompt.seat, words, f"prompt:{prompt.ask}")
