"""Run from a Rig checkout: PYTHONPATH=.:<rigdesk>/rig-plugin .venv/bin/python <rigdesk>/rig-plugin/tests/test_rigdesk.py (or with pytest)."""
import asyncio
import json
import tempfile
from pathlib import Path
from types import SimpleNamespace

from core.base import now_ms
from core.model import GraphName, NodeId, SeatId
from rigdesk.desks import DeskPc, Desks, Held, PcLink, pc_of_link, words_of
from rigdesk.health import Sample

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
    assert words_of("deus", {"type": "holder", "holder": "agent", "by": "operator", "at_ms": 0}) is None
    assert "pressed Stop" in str(words_of("deus", {"type": "holder", "holder": "blocked", "by": "stop", "at_ms": 0}))
    assert words_of("deus", {"type": "revoked", "agent_id": "a", "reason": "the agent left", "at_ms": 0}) is None
    assert "session there ended" in str(words_of("deus", {"type": "revoked", "agent_id": "a", "reason": "the human ended the share", "at_ms": 0}))


def test_a_rigdesk_pc_says_whether_the_operator_can_start_a_session_there() -> None:
    from rigdesk import _reached_by

    def entry(join: object) -> object:
        return {"stock": False, "join": join}

    assert "desk/request (the operator picks Join once or Always allow" in _reached_by(entry({"open": True, "code": None, "words": None}))  # type: ignore[arg-type]
    paused = _reached_by(entry({"open": False, "code": "join_paused", "words": "the person at this PC paused sessions started from Rig"}))  # type: ignore[arg-type]
    assert paused.startswith("a share link") and "paused" in paused
    assert "takes no sessions started from Rig" in _reached_by(entry(None))  # type: ignore[arg-type]


def test_a_stock_pc_names_the_mcp_servers_on_its_own_address_by_http_url() -> None:
    from rigdesk.desks import StockRemote
    phone = StockRemote(id="123456789", password_file="/dev/null", mcp_servers=[{"url": "http://100.81.214.29:8790/mcp", "token_file": "/dev/null"}])
    assert phone.mcp_servers[0].url == "http://100.81.214.29:8790/mcp"
    for url in ("https://moto-g57:8790", "http://moto g57", "http://moto-g57/a b"):
        try:
            StockRemote(id="123456789", password_file="/dev/null", mcp_servers=[{"url": url}])
        except ValueError:
            continue
        raise AssertionError(f"{url} was accepted")


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
    desks = Desks(SimpleNamespace(state_dir=Path(state), docs=docs, report=lambda *_: None, forget=lambda *_: None))  # type: ignore[arg-type]
    stock = StockRemote(peer="deus.tail1234.ts.net", password_file="/dev/null")
    desks._pcs["deus"] = PcLink("deus", DeskPc(url="http://127.0.0.1:21120", token_file="/dev/null", stock=stock))
    desks._pcs["front"] = PcLink("front", DeskPc(url="https://front.tail1234.ts.net", token_file="/dev/null"))
    return desks


def test_an_assigned_agent_or_graph_holds_a_standing_grant_to_its_pc_and_keeps_it_across_a_restart() -> None:
    async def run() -> None:
        with tempfile.TemporaryDirectory() as state:
            docs = _Docs()
            desks = _granting(state, docs)
            assert await desks.assign("deus", "agent:a1") and await desks.assign("deus", "graph:clients")
            assert not await desks.assign("deus", "agent:a1"), "a grant held already changes nothing"
            assert await desks.assign("front", "agent:a2"), "a RigDesk PC takes a standing grant: Always allow"
            assert desks.assigned_pcs("a2", "g") == ("front",)
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


def test_always_allow_on_a_rigdesk_pc_grants_and_joins_through_join_and_unassign_ends_that_session_and_tells_the_agent() -> None:
    from rigdesk import _always_allowed
    from rigdesk.desks import DeskRefused
    joined: list[tuple[str, str, str]] = []
    told: list[str] = []

    async def run() -> None:
        with tempfile.TemporaryDirectory() as state:
            desks = _granting(state, _Docs())

            async def join_operator(pc: str, agent: str, graph: str, node: str, title: str, ask: str, via: str = "link") -> Held:
                if pc == "busy":
                    raise DeskRefused("busy refused the session: held")
                joined.append((pc, ask, via))
                held = desks._sessions[pc] = Held(pc, agent, graph, node, title, "s", now_ms(), via)  # type: ignore[arg-type]
                return held

            async def leave(held: Held) -> bool:
                desks._sessions.pop(held.pc)
                return True

            async def tell(held: Held, words: str, key: str, timing: str = "steer") -> bool:
                told.append(words)
                return True
            desks._join_operator = join_operator  # type: ignore[method-assign]
            desks.leave = leave  # type: ignore[method-assign]
            desks._tell = tell  # type: ignore[method-assign]
            said = await _always_allowed(desks, {"pc": "front", "agent": "a1", "graph": "g", "kind": "join", "ask": "desk_1"}, "n", "A")
            assert "You joined front" in said and desks.granted("front") == ("agent:a1",)
            await desks.leave(desks._sessions["front"])
            await desks.join_assigned("front", "a1", "g", "n", "A")
            assert joined == [("front", "desk_1", "assigned"), ("front", "standing grant", "assigned")]
            assert await desks.unassign("front", "agent:a1")
            assert "front" not in desks._sessions and desks.assigned_pcs("a1", "g") == ()
            desks._pcs["busy"] = PcLink("busy", DeskPc(url="https://busy.tail1234.ts.net", token_file="/dev/null"))
            try:
                await _always_allowed(desks, {"pc": "busy", "agent": "a1", "graph": "g", "kind": "join", "ask": "desk_2"}, "n", "A")
            except DeskRefused:
                pass
            else:
                raise AssertionError("a refused join was taken as joined")
            assert desks.granted("busy") == (), "a grant whose join failed is taken back"

    asyncio.run(run())
    assert len(told) == 1 and "took away your standing access to front" in told[0], told


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


FIGURES = Path(__file__).resolve().parents[2] / "libs/agent/tests/fixtures/figures-windows.json"


def _sample(id: int, at_ms: int, **changed: object) -> Sample:
    figures = json.loads(FIGURES.read_text()) | changed
    return Sample.model_validate({"id": id, "at_ms": at_ms, "boot": "b", "rigdesk": "1.4.2", "took_ms": 900, "taken": "ok", "figures": figures})


def test_rig_judges_a_pcs_figures_and_a_pc_up_with_no_new_sample_reads_stale() -> None:
    from rigdesk.health import Sample, Thresholds, health
    now = now_ms()
    sample = _sample(1, now - 60_000)
    judged = health(sample, Thresholds(), now, up=True)
    assert judged.state == "fault" and [check.name for check in judged.failing] == ["disk D:", "services"], judged
    assert "Print Spooler" in judged.said and "Edge" not in judged.said, "a delayed service stopped cleanly is no failure"
    low = health(sample, Thresholds(disk_free_fault_pct=50), now, up=True)
    assert low.state == "fault" and any(check.name == "disk C:" and check.state == "fault" for check in low.failing), low
    assert health(_sample(1, now - 30 * 60_000), Thresholds(), now, up=True).state == "stale"
    assert health(_sample(1, now - 30 * 60_000), Thresholds(), now, up=False).state == "fault", "a PC that is down is down, not stale"
    failed = Sample.model_validate({"id": 2, "at_ms": now, "taken": "failed", "why": "powershell exited 1"})
    assert health(failed, Thresholds(), now, up=True).state == "warn"
    blind = _sample(3, now, defender={"realtime_on": None, "signature_age_days": None}, stopped_services=[], volumes=[{"name": "A:", "total_mb": 0, "free_mb": 0}])
    assert health(blind, Thresholds(), now, up=True).state == "ok", "a figure the PC could not read is unknown, never a fault"


def test_samples_are_kept_a_week_by_the_pcs_clock_and_outlive_the_plugin() -> None:
    from rigdesk.health import KEPT_MS, REWRITE_SLACK_MS, History
    now = now_ms()
    with tempfile.TemporaryDirectory() as state:
        history = History(Path(state))
        assert len(history.add("deus", [_sample(1, now - 20 * 60_000), _sample(2, now - 10 * 60_000)], now)) == 2
        assert history.add("deus", [_sample(2, now - 10 * 60_000)], now) == [], "a sample read twice is kept once"
        assert [each.id for each in history.add("deus", [_sample(1, now)], now)] == [1], "a PC whose ids began again is still read, by its clock"
        assert [each.id for each in History(Path(state)).of("deus")] == [1, 2, 1]
        old = History(Path(state) / "old")
        old.add("deus", [_sample(1, now - KEPT_MS - REWRITE_SLACK_MS - 1)], now - KEPT_MS)
        old.add("deus", [_sample(2, now)], now)
        assert [each.id for each in History(Path(state) / "old").of("deus")] == [2], "a week is kept, no more"


def test_the_shell_allow_list_grants_user_or_system_per_agent_or_graph_on_rigdesk_pcs_only() -> None:
    from rigdesk.desks import DeskRefused

    async def run() -> None:
        with tempfile.TemporaryDirectory() as state:
            docs = _Docs()
            desks = _granting(state, docs)
            await desks.assign("deus", "agent:a1")
            assert await desks.allow_shell("front", "agent:a1", "user") and await desks.allow_shell("front", "graph:ops", "system")
            assert not await desks.allow_shell("front", "agent:a1", "user")
            assert desks.shell_level("front", "a1", "g") == "user"
            assert desks.shell_level("front", "a1", "ops") == "system", "the most of its own grant and its graph's"
            assert desks.shell_level("front", "a2", "g") is None and desks.shell_level("deus", "a1", "g") is None
            try:
                await desks.allow_shell("deus", "agent:a1", "user")
            except DeskRefused:
                pass
            else:
                raise AssertionError("a stock PC took a shell grant")
            again = _granting(state, docs)
            await again._read_grants()
            assert again.granted("deus") == ("agent:a1",) and again.shell_granted("front") == {"agent:a1": "user", "graph:ops": "system"}
            assert await again.allow_shell("front", "graph:ops", None) and again.shell_level("front", "a1", "ops") == "user"

    asyncio.run(run())


def test_pc_health_is_reported_each_judging() -> None:
    from rigdesk.health import Thresholds
    reported: list[tuple[str, str, str]] = []
    now = now_ms()

    def report(state: str, subject: str, value: str, why: str, fields: dict) -> None:
        reported.append((state, subject, value))

    async def run() -> None:
        with tempfile.TemporaryDirectory() as state:
            desks = Desks(SimpleNamespace(state_dir=Path(state), report=report))  # type: ignore[arg-type]
            desks._pcs["deus"] = PcLink("deus", PCS["deus"], state="up")
            desks._history.add("deus", [_sample(1, now, stopped_services=[], volumes=[{"name": "C:", "total_mb": 100, "free_mb": 40}])], now)
            await desks._judge()
            desks._thresholds = Thresholds(disk_free_fault_pct=50)
            await desks._judge()

    asyncio.run(run())
    assert reported == [("rigdesk.pc_health", "deus", "ok"), ("rigdesk.pc_health", "deus", "fault")], reported


if __name__ == "__main__":
    for name, test in list(globals().items()):
        if name.startswith("test_"):
            test()
            print("ok", name)
