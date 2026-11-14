"""Run from a Rig checkout: PYTHONPATH=.:<rigdesk>/rig-plugin .venv/bin/python <rigdesk>/rig-plugin/tests/test_rigdesk.py (or with pytest)."""
import asyncio
import tempfile
from pathlib import Path
from types import SimpleNamespace

from core.base import now_ms
from core.model import GraphName, NodeId, SeatId
from rigdesk.desks import DeskPc, Desks, Held, PcLink, pc_of_link, words_of

PCS = {"deus": DeskPc(url="https://deus.tail1234.ts.net", token_file="/dev/null", client="acme")}
GRANT, SECRET = "a" * 32, "b" * 32


def test_a_link_names_its_pc_by_name_or_tailnet_host() -> None:
    assert pc_of_link(f"rigdesk://deus/{GRANT}#{SECRET}", PCS) == ("deus", GRANT, SECRET)
    assert pc_of_link(f"rigdesk://deus.tail1234.ts.net/{GRANT}#{SECRET}", PCS) == ("deus", GRANT, SECRET)
    assert "no RigDesk PC named other" in str(pc_of_link(f"rigdesk://other/{GRANT}#{SECRET}", PCS))
    assert "not a RigDesk share link" in str(pc_of_link("https://example.com", PCS))


def test_the_agent_hears_of_stop_and_block_but_not_of_the_humans_passing_input_or_its_own_join_or_leave() -> None:
    assert words_of("deus", {"type": "holder", "holder": "human", "by": "viewer input", "at_ms": 0}) is None
    assert words_of("deus", {"type": "holder", "holder": "agent", "by": "quiet", "at_ms": 0}) is None
    assert "resumed you" in str(words_of("deus", {"type": "holder", "holder": "agent", "by": "unblock", "at_ms": 0}))
    assert "blocked you" in str(words_of("deus", {"type": "holder", "holder": "blocked", "by": "block", "at_ms": 0}))
    assert words_of("deus", {"type": "holder", "holder": "agent", "by": "share", "at_ms": 0}) is None
    assert "pressed Stop" in str(words_of("deus", {"type": "holder", "holder": "blocked", "by": "stop", "at_ms": 0}))
    assert words_of("deus", {"type": "revoked", "agent_id": "a", "reason": "the agent left", "at_ms": 0}) is None
    assert "session there ended" in str(words_of("deus", {"type": "revoked", "agent_id": "a", "reason": "the human ended the share", "at_ms": 0}))


def test_a_stock_pc_is_served_on_loopback() -> None:
    stock = {"peer": "front.tail1234.ts.net", "password_file": "/dev/null"}
    assert DeskPc(url="http://127.0.0.1:21120", token_file="/dev/null", stock=stock).stock is not None
    try:
        DeskPc(url="https://front.tail1234.ts.net", token_file="/dev/null", stock=stock)
    except ValueError as refused:
        assert "http://127.0.0.1:<port>" in str(refused)
    else:
        raise AssertionError("a stock PC with a tailnet url was accepted")


def test_stop_interrupts_the_owning_agents_turn_but_a_replayed_stop_only_tells_it() -> None:
    seat, done = SeatId(GraphName("g"), NodeId("n")), []

    async def inject(*args: object) -> object:
        done.append(("interrupt", args[1]))
        return object()

    async def admit(_seat: SeatId, said: object) -> object:
        done.append(("told", "pressed Stop" in getattr(said, "words")))
        return object()

    async def seat_of(*_args: object) -> SeatId:
        return seat

    async def run() -> None:
        with tempfile.TemporaryDirectory() as state:
            host = SimpleNamespace(state_dir=Path(state), kernel=lambda _seat: SimpleNamespace(inject=inject), admit=admit, seat_of=seat_of)
            desks = Desks(host)  # type: ignore[arg-type]
            desks._sessions["deus"] = Held("deus", "agent_a", "g", "n", "A", "s", 0)
            link = PcLink("deus", PCS["deus"])
            await desks._event(link, {"type": "holder", "holder": "blocked", "by": "stop", "id": 1, "at_ms": now_ms()})
            await desks._event(link, {"type": "holder", "holder": "blocked", "by": "stop", "id": 2, "at_ms": now_ms() - 3_600_000})

    asyncio.run(run())
    assert done == [("interrupt", "interrupt"), ("told", True), ("told", True)]


class _Docs:
    """Rig's docs store for one plugin: a body and its revision per key, refusing a put over a revision it no longer holds."""
    def __init__(self) -> None:
        self.held: dict[str, tuple[int, dict]] = {}

    async def get(self, key: str):
        found = self.held.get(key)
        return None if found is None else SimpleNamespace(key=key, revision=found[0], body=found[1])

    async def put(self, key: str, body: dict, *, expect: int | None = None):
        current = self.held.get(key, (0, {}))[0]
        if expect is not None and expect != current:
            raise RuntimeError(f"revision_conflict: {key} is at {current}, not {expect}")
        self.held[key] = (current + 1, body)
        return SimpleNamespace(key=key, revision=current + 1, body=body)


def _granting(state: str, docs: _Docs) -> Desks:
    from rigdesk.desks import StockRemote
    desks = Desks(SimpleNamespace(state_dir=Path(state), docs=docs))  # type: ignore[arg-type]
    stock = StockRemote(peer="deus.tail1234.ts.net", password_file="/dev/null")
    desks._pcs["deus"] = PcLink("deus", DeskPc(url="http://127.0.0.1:21120", token_file="/dev/null", stock=stock))
    desks._pcs["front"] = PcLink("front", DeskPc(url="https://front.tail1234.ts.net", token_file="/dev/null"))
    return desks


def test_an_assigned_agent_or_graph_holds_a_standing_grant_to_its_stock_pc_only_and_keeps_it_across_a_restart() -> None:
    from rigdesk.desks import DeskRefused

    async def run() -> None:
        with tempfile.TemporaryDirectory() as state:
            docs = _Docs()
            desks = _granting(state, docs)
            assert await desks.assign("deus", "agent:a1") and await desks.assign("deus", "graph:clients")
            assert not await desks.assign("deus", "agent:a1"), "a grant held already changes nothing"
            try:
                await desks.assign("front", "agent:a2")
            except DeskRefused:
                pass
            else:
                raise AssertionError("a RigDesk PC took a standing grant")
            assert desks.assigned_pcs("a1", "g") == ("deus",)
            assert desks.assigned_pcs("anyone", "clients") == ("deus",)
            assert desks.assigned_pcs("a3", "g") == ()
            again = _granting(state, docs)
            await again._read_grants()
            assert again.granted("deus") == ("agent:a1", "graph:clients"), "grants outlive the plugin"

    asyncio.run(run())


def test_a_second_writer_is_refused_and_an_unreadable_grants_doc_stops_the_plugin() -> None:
    async def run() -> None:
        with tempfile.TemporaryDirectory() as state:
            docs = _Docs()
            desks = _granting(state, docs)
            await docs.put("grants", {"pcs": {"deus": ["agent:other"]}})
            try:
                await desks.assign("deus", "agent:a1")
            except RuntimeError:
                pass
            else:
                raise AssertionError("a grant overwrote one written behind it")
            assert desks.granted("deus") == ()
            await docs.put("grants", {"pcs": {"deus": "agent:a1"}})
            try:
                await _granting(state, docs)._read_grants()
            except RuntimeError:
                pass
            else:
                raise AssertionError("an unreadable grants doc was read as no grants")

    asyncio.run(run())


def test_an_agent_assigned_two_pcs_names_the_one_it_joins_and_is_never_joined_to_either_by_default() -> None:
    from rigdesk.desks import DeskRefused, StockRemote

    async def run() -> None:
        with tempfile.TemporaryDirectory() as state:
            desks = _granting(state, _Docs())
            stock = StockRemote(peer="back.tail1234.ts.net", password_file="/dev/null")
            desks._pcs["back"] = PcLink("back", DeskPc(url="http://127.0.0.1:21122", token_file="/dev/null", stock=stock))
            await desks.assign("deus", "agent:a1")
            await desks.assign("back", "agent:a1")
            for joining in (desks.join_assigned("front", "a1", "g", "n", "A"),):
                try:
                    await joining
                except DeskRefused as refused:
                    assert "back, deus" in str(refused) or "not assigned to you" in str(refused), refused
                else:
                    raise AssertionError("joined a PC the agent did not name, or one not assigned to it")
            desks._sessions["back"] = Held("back", "a2", "g", "n2", "B", "s", 0, "link")
            try:
                await desks.join_assigned("back", "a1", "g", "n", "A")
            except DeskRefused as refused:
                assert "B holds it now" in str(refused), refused
            else:
                raise AssertionError("joined a PC another agent holds")

    asyncio.run(run())


def test_taking_the_grant_away_ends_the_assigned_session_at_once() -> None:
    ended: list[str] = []

    async def run() -> None:
        with tempfile.TemporaryDirectory() as state:
            desks = _granting(state, _Docs())
            await desks.assign("deus", "agent:a1")
            desks._sessions["deus"] = Held("deus", "a1", "g", "n", "A", "s", 0, "assigned")

            async def unassigned(pc: str) -> None:
                ended.append(pc)
            desks._unassigned = unassigned  # type: ignore[method-assign]
            assert await desks.unassign("deus", "agent:a1")
            assert desks.granted("deus") == ()

    asyncio.run(run())
    assert ended == ["deus"]


def test_a_pc_gone_from_settings_or_no_longer_stock_loses_its_grants() -> None:
    from rigdesk.desks import RigdeskSettings

    async def run() -> None:
        with tempfile.TemporaryDirectory() as state:
            desks = _granting(state, _Docs())
            await desks.assign("deus", "agent:a1")
            desks._reconcile(RigdeskSettings(pcs={"front": desks._pcs["front"].spec}))
            await desks._prune_grants()
            assert desks.granted("deus") == ()

    asyncio.run(run())


def test_an_assigned_session_idle_five_minutes_leaves_the_pc_but_a_link_session_stays() -> None:
    left: list[str] = []
    told: list[str] = []

    async def run() -> None:
        with tempfile.TemporaryDirectory() as state:
            desks = Desks(SimpleNamespace(state_dir=Path(state)))  # type: ignore[arg-type]
            stale = now_ms() - 6 * 60_000
            desks._sessions["deus"] = Held("deus", "a1", "g", "n", "A", "s", stale, "assigned")
            desks._sessions["front"] = Held("front", "a2", "g", "n", "B", "s", stale, "link")
            desks._sessions["back"] = Held("back", "a3", "g", "n", "C", "s", stale, "assigned", used_ms=now_ms())

            async def leave(held: Held) -> bool:
                left.append(held.pc)
                desks._sessions.pop(held.pc)
                return True

            async def tell(held: Held, words: str, key: str, timing: str = "steer") -> bool:
                told.append(f"{timing}: {words}")
                return True
            desks.leave = leave  # type: ignore[method-assign]
            desks._tell = tell  # type: ignore[method-assign]
            await desks._idle_leaves()

    asyncio.run(run())
    assert left == ["deus"]
    assert len(told) == 1 and told[0].startswith("queue: RigDesk deus: you left deus after 5 minutes"), told


if __name__ == "__main__":
    for name, test in list(globals().items()):
        if name.startswith("test_"):
            test()
            print("ok", name)
