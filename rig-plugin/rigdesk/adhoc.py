"""Ad-hoc joins: a PC Rig was never set up for, joined by the RustDesk ID and the password the operator handed an agent.

The operator handing them over is the consent. rigdesk-bridge runs with --once: it logs in at once, holds that one connection, never retries a
refused login and exits when the session ends, since a one-time password stops working when its connection closes. The password reaches the
bridge only through a file this user alone reads, deleted with the session, never through its argv, env or log."""
from __future__ import annotations

import asyncio
import base64
import binascii
import re
from dataclasses import dataclass
from typing import Literal

# RustDesk's own public ID server and the key its clients are built with.
PUBLIC_HOST = "rs-ny.rustdesk.com"
PUBLIC_KEY = "OeVuKk5nlHiXp+APNn0Y3pC1Iwpwn44JGqrQCsWqmBw="
DIRECT_PORT = 21118
REACH_LIMIT_S = 3.0
LOGIN_LIMIT_S = 60.0
IDLE_MS = 30 * 60_000
# What a RigDesk PC answers a login made under an ID other than its own, before it looks at the password.
NOT_THIS_PC = "Offline"
_TARGET = re.compile(r"^(?P<id>[0-9 ]{6,40}|[A-Za-z0-9_-]{6,32})(?:@(?P<host>[A-Za-z0-9.\[\]:-]{1,255}?)(?:\?key=(?P<key>[A-Za-z0-9+/_=-]{40,64}))?)?$")


@dataclass(frozen=True)
class Target:
    id: str
    host: str | None
    key: str | None


@dataclass(frozen=True)
class Route:
    """One way to reach the PC: the bridge's arguments for it, in words, and the RigDesk PC of ours it tries, if it is one."""
    argv: tuple[str, ...]
    words: str
    rigdesk: str | None = None


type Outcome = tuple[Literal["logged in", "refused", "failed"], str]


def target_of(text: str) -> Target | str:
    """The PC an operator named: a RustDesk ID, spaces allowed, or ID@host[:port]?key=<the server's public key>; a str says why it is none."""
    found = _TARGET.match(text.strip())
    if found is None:
        return f"{text.strip()[:80]!r} is not a RustDesk ID: give the digits the PC shows, or ID@host?key=<key> for a PC on a self-hosted ID server"
    rustdesk_id = found["id"].replace(" ", "")
    if not 6 <= len(rustdesk_id) <= 32:
        return f"{rustdesk_id!r} is not a RustDesk ID"
    host, key = found["host"], found["key"]
    if host is not None and key is None:
        return f"{host} is a self-hosted ID server: give its public key too, as ID@{host}?key=<key> (the Key its PCs were given)"
    if key is not None:
        try:
            raw = base64.b64decode(key + "=" * (-len(key) % 4), altchars=b"-_" if "-" in key or "_" in key else None, validate=True)
        except (binascii.Error, ValueError):
            raw = b""
        if len(raw) != 32:
            return "the server key is not RustDesk's: it is the 44-character base64 public key (id_ed25519.pub) its PCs were given"
        key = base64.b64encode(raw).decode()
    return Target(rustdesk_id, host, key)


def host_port(peer: str) -> tuple[str, int]:
    """A tailnet peer as settings name it, name or address with an optional :port, as a host and port to probe."""
    if peer.startswith("["):
        host, _, rest = peer[1:].partition("]")
        return host, int(rest[1:]) if rest.startswith(":") else DIRECT_PORT
    if peer.count(":") == 1:
        host, port = peer.split(":")
        return host, int(port)
    return peer, DIRECT_PORT


async def answers(host: str, port: int = DIRECT_PORT) -> bool:
    """Whether something takes a TCP connection there now: a RigDesk PC that is off is skipped rather than waited on for a login."""
    try:
        _, writer = await asyncio.wait_for(asyncio.open_connection(host, port), REACH_LIMIT_S)
    except (OSError, TimeoutError):
        return False
    writer.close()
    return True


async def outcome(process: asyncio.subprocess.Process) -> Outcome:
    """The bridge's first word on its login, read from its stdout: logged in, refused (the PC's own words) or failed (it was never reached)."""
    assert process.stdout is not None, "the bridge's stdout is a pipe"
    async def read() -> Outcome:
        assert process.stdout is not None
        while raw := await process.stdout.readline():
            line = raw.decode("utf-8", "replace").strip()
            for word in ("logged in", "refused", "failed"):
                if line.startswith(word):
                    return word, line[len(word):].lstrip(": ").strip()  # type: ignore[return-value]
        code = await process.wait()
        return "failed", f"rigdesk-bridge exited with code {code} before it logged in"
    try:
        return await asyncio.wait_for(read(), LOGIN_LIMIT_S)
    except TimeoutError:
        return "failed", f"no login within {LOGIN_LIMIT_S:g} s"


async def ended(process: asyncio.subprocess.Process) -> str:
    """Why the bridge's one session ended, from its last word, once it exits."""
    assert process.stdout is not None, "the bridge's stdout is a pipe"
    said = ""
    while raw := await process.stdout.readline():
        line = raw.decode("utf-8", "replace").strip()
        if line.startswith("ended"):
            said = line[len("ended"):].lstrip(": ").strip()
    code = await process.wait()
    return said or f"rigdesk-bridge exited with code {code}"


def refusal(target: Target, route: Route, said: Outcome) -> str:
    kind, words = said
    if kind == "refused":
        return f"{target.id}, reached through {route.words}, refused the login: {words}"
    return f"{target.id} could not be reached through {route.words}: {words}"
