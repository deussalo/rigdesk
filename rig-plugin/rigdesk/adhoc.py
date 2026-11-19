"""Ad-hoc joins: a PC Rig was never set up for, joined by the RustDesk ID and the password the operator handed an agent.

The operator handing them over is the consent. rigdesk-bridge runs with --once: it logs in at once, holds that one connection, never retries a
refused login and exits when the session ends, since a one-time password stops working when its connection closes. The password reaches the
bridge only through a file this user alone reads, deleted with the session, never through its argv, env or log."""
from __future__ import annotations

import asyncio
import base64
import binascii
import ipaddress
import re
import socket
from dataclasses import dataclass
from typing import Literal

# RustDesk's own public ID server and the key its clients are built with.
PUBLIC_HOST = "rs-ny.rustdesk.com"
PUBLIC_KEY = "OeVuKk5nlHiXp+APNn0Y3pC1Iwpwn44JGqrQCsWqmBw="
DIRECT_PORT = 21118
TAILNET = ipaddress.IPv4Network("100.64.0.0/10")
REACH_LIMIT_S = 3.0
LOGIN_LIMIT_S = 60.0
IDLE_MS = 30 * 60_000
# What a RigDesk PC answers a login made under an ID other than its own, before it looks at the password.
NOT_THIS_PC = "Offline"
_TAILNET_NAME = re.compile(r"^(?:[a-z0-9-]+\.)+ts\.net$")
_TARGET = re.compile(r"^(?P<id>[0-9 ]{6,40}|[A-Za-z0-9_-]{6,32})(?:@(?P<host>[A-Za-z0-9.\[\]:-]{1,255}?)(?:\?key=(?P<key>[A-Za-z0-9+/_=-]{40,64}))?)?$")


@dataclass(frozen=True)
class Target:
    id: str
    host: str | None
    key: str | None
    peer: str | None = None  # host:port on the tailnet, dialed directly; host and key name an ID server instead


@dataclass(frozen=True)
class Route:
    """One way to reach the PC: the bridge's arguments for it, in words, and the RigDesk PC of ours it tries, if it is one."""
    argv: tuple[str, ...]
    words: str
    rigdesk: str | None = None


type Outcome = tuple[Literal["logged in", "refused", "failed"], str]


class NotOnTailnet(Exception):
    """A tailnet name that does not lead only to tailnet addresses: a direct session has no RustDesk encryption, so it is not dialed."""


def _ipv4(text: str) -> ipaddress.IPv4Address | None:
    try:
        return ipaddress.IPv4Address(text)
    except ValueError:
        return None


def tailnet_peer(host: str) -> str | None:
    """host[:port] as a direct peer's lowercase host:port, if it is a 100.64.0.0/10 address or a *.ts.net name; anything else is no peer."""
    name, colon, given = host.lower().partition(":")
    if colon and not (given.isdecimal() and 1 <= int(given) <= 65535):
        return None
    address = _ipv4(name)
    if not (address in TAILNET if address is not None else _TAILNET_NAME.match(name)):
        return None
    return f"{name}:{int(given) if colon else DIRECT_PORT}"


def target_of(text: str) -> Target | str:
    """The PC an operator named: a RustDesk ID, spaces allowed; ID@host[:port]?key=<the server's public key>, on that ID server; or ID@<tailnet address>[:port],
    dialed directly. A str says why it is none."""
    found = _TARGET.match(text.strip())
    if found is None:
        return f"{text.strip()[:80]!r} is not a RustDesk ID: give the digits the PC shows, ID@host?key=<key> for a PC on a self-hosted ID server, or ID@<tailnet address> for one on the tailnet"
    rustdesk_id = found["id"].replace(" ", "")
    if not 6 <= len(rustdesk_id) <= 32:
        return f"{rustdesk_id!r} is not a RustDesk ID"
    host, key = found["host"], found["key"]
    if host is not None and key is None:
        peer = tailnet_peer(host)
        if peer is not None:
            return Target(rustdesk_id, None, None, peer)
        return (f"{host} is a self-hosted ID server: give its public key too, as ID@{host}?key=<key> (the Key its PCs were given). "
                "Only a tailnet address, in 100.64.0.0/10 or a *.ts.net name, is dialed directly")
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


async def lookup(name: str) -> list[str]:
    """The IPv4 addresses a name has now; none if it does not resolve in time."""
    try:
        found = await asyncio.wait_for(asyncio.get_running_loop().getaddrinfo(name, None, family=socket.AF_INET, type=socket.SOCK_STREAM), REACH_LIMIT_S)
    except (OSError, TimeoutError):
        return []
    return sorted({str(each[4][0]) for each in found})


async def tailnet_address(peer: str) -> str:
    """The address:port to dial for a tailnet peer. A name is resolved here and its address handed on, so what was checked is what the bridge dials."""
    name, port = peer.rsplit(":", 1)
    if _ipv4(name) is not None:
        return peer
    addresses = await lookup(name)
    if not addresses:
        raise NotOnTailnet(f"{name} has no address here, so it was not dialed: is this machine on that tailnet?")
    outside = [each for each in addresses if (address := _ipv4(each)) is None or address not in TAILNET]
    if outside:
        raise NotOnTailnet(f"{name} leads to {', '.join(outside)}, outside the tailnet, so it was not dialed")
    return f"{addresses[0]}:{port}"


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
    if kind == "refused" and words == NOT_THIS_PC and target.peer is not None:
        return f"the PC at {target.peer} is not {target.id}: it refused a login under that ID"
    if kind == "refused":
        return f"{target.id}, reached through {route.words}, refused the login: {words}"
    return f"{target.id} could not be reached through {route.words}: {words}"
