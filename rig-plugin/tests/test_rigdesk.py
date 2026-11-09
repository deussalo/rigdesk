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


def test_an_assigned_agent_or_graph_holds_a_standing_grant_to_its_stock_pc_only() -> None:
    from rigdesk.desks import StockRemote
    with tempfile.TemporaryDirectory() as state:
        desks = Desks(SimpleNamespace(state_dir=Path(state)))  # type: ignore[arg-type]
        stock = StockRemote(peer="deus.tail1234.ts.net", password_file="/dev/null")
        desks._pcs["deus"] = PcLink("deus", DeskPc(url="http://127.0.0.1:21120", token_file="/dev/null", stock=stock, assigned=["agent:a1", "graph:clients"]))
        desks._pcs["front"] = PcLink("front", DeskPc(url="https://front.tail1234.ts.net", token_file="/dev/null", assigned=["agent:a2"]))
        assert desks.assigned_pc("a1", "g") == "deus"
        assert desks.assigned_pc("anyone", "clients") == "deus"
        assert desks.assigned_pc("a3", "g") is None
        assert desks.assigned_pc("a2", "g") is None, "a RigDesk PC shares through its viewer only"


def test_taking_the_grant_away_ends_the_assigned_session_at_once() -> None:
    from rigdesk.desks import RigdeskSettings, StockRemote
    ended: list[str] = []

    async def run() -> None:
        with tempfile.TemporaryDirectory() as state:
            desks = Desks(SimpleNamespace(state_dir=Path(state)))  # type: ignore[arg-type]
            stock = StockRemote(peer="deus.tail1234.ts.net", password_file="/dev/null")
            spec = DeskPc(url="http://127.0.0.1:21120", token_file="/dev/null", stock=stock, assigned=["agent:a1"])
            desks._pcs["deus"] = PcLink("deus", spec)
            desks._sessions["deus"] = Held("deus", "a1", "g", "n", "A", "s", 0, "assigned")

            async def unassigned(pc: str) -> None:
                ended.append(pc)
            desks._unassigned = unassigned  # type: ignore[method-assign]
            desks._reconcile(RigdeskSettings(pcs={"deus": spec.model_copy(update={"assigned": []})}))
            await asyncio.sleep(0.01)
            assert desks._pcs["deus"].spec.assigned == [], "the bridge keeps running; only the grant changed"

    asyncio.run(run())
    assert ended == ["deus"]


if __name__ == "__main__":
    for name, test in list(globals().items()):
        if name.startswith("test_"):
            test()
            print("ok", name)
