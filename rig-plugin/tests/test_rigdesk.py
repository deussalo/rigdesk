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
    assert pc_of_link(f"rigdesk://desktop_ab12/{GRANT}#{SECRET}", PCS, {"deus": "DESKTOP_AB12"}) == ("deus", GRANT, SECRET)
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

    def entry(join: object, state: str = "up") -> object:
        return {"name": "dell", "stock": False, "join": join, "state": state}

    assert "desk/request (the operator picks Join once or Always allow" in _reached_by(entry({"open": True, "code": None, "words": None}))  # type: ignore[arg-type]
    paused = _reached_by(entry({"open": False, "code": "join_paused", "words": "the person at this PC paused sessions started from Rig"}))  # type: ignore[arg-type]
    assert paused.startswith("a share link") and "paused" in paused
    assert "takes no sessions started from Rig" in _reached_by(entry(None))  # type: ignore[arg-type]
    for state in ("down", "connecting"):
        unheard = _reached_by(entry(None, state))  # type: ignore[arg-type]
        assert "unknown until dell is reachable" in unheard and "takes no sessions" not in unheard, unheard


def test_a_rigdesk_pc_rig_has_not_heard_from_says_its_shell_is_unknown_not_missing() -> None:
    from rigdesk import _shell_words

    class NoGrants:
        def held_by(self, agent: str, graph: str) -> None:
            return None

        def shell_level(self, pc: str, agent: str, graph: str) -> None:
            return None

        def top_rights(self, pc: str) -> str:
            return "root"

    def words(shell: object, state: str) -> str:
        return _shell_words(NoGrants(), {"name": "dell", "stock": False, "shell": shell, "state": state}, "a", "g")  # type: ignore[arg-type]

    for state in ("down", "connecting"):
        unheard = words(None, state)
        assert unheard == "you may not run commands there; whether its RigDesk offers an agent shell is unknown until dell is reachable", unheard
    assert "offers no agent shell" in words(None, "up")
    assert "lane_down" in words({"open": False, "code": "lane_down", "words": "its management lane at /lane does not answer: x"}, "down")


def test_a_job_read_says_the_job_ran_as_system() -> None:
    import json
    from rigdesk import ShellInput, _shell_answered
    state = {"job": "j-c17fcc63", "command": "sleep 1", "started_ms": 1, "output": "", "cut_bytes": 0, "state": "running"}
    for asked in ({"pc": "dell", "job": "j-c17fcc63"}, {"pc": "dell", "job": "j-c17fcc63", "as": "user"}):
        read = _shell_answered("dell", ShellInput.model_validate(asked), "user" if "as" in asked else "system", {"ok": True, "text": json.dumps(state)})
        assert read["as"] == "system" and read["job"] == "j-c17fcc63" and read["state"] == "running", read
    ran = _shell_answered("dell", ShellInput.model_validate({"pc": "dell", "command": "id -un", "as": "user"}), "user", {"ok": True, "text": json.dumps({"exit_code": 0, "output": "deus"})})
    assert ran["as"] == "user" and ran["output"] == "deus", ran


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


def test_a_screenshot_comes_back_as_an_image_at_its_own_pixels_or_says_how_to_map_a_shrunk_one() -> None:
    import base64
    import io

    from PIL import Image

    from rigdesk.shots import inline

    def png(width: int, height: int) -> bytes:
        out = io.BytesIO()
        Image.effect_noise((width, height), 64).convert("RGB").save(out, "PNG")
        return out.getvalue()

    desktop = inline(png(1920, 1080))
    assert Image.open(io.BytesIO(base64.b64decode(desktop.data))).size == (1920, 1080)
    assert desktop.mime == "image/jpeg" and "multiply" not in desktop.note
    wide = inline(png(3840, 2160))
    assert Image.open(io.BytesIO(base64.b64decode(wide.data))).size == (2000, 1125)
    assert "multiply x,y by 1.92" in wide.note


def test_an_operator_names_a_pc_by_rustdesk_id_or_by_id_at_a_self_hosted_server_with_its_key() -> None:
    from rigdesk.adhoc import PUBLIC_KEY, Target, target_of
    assert target_of(" 105 517 719 ") == Target("105517719", None, None)
    assert target_of(f"105517719@rd.example.com:21116?key={PUBLIC_KEY}") == Target("105517719", "rd.example.com:21116", PUBLIC_KEY)
    assert "give its public key too" in str(target_of("105517719@rd.example.com"))
    assert "not RustDesk's" in str(target_of("105517719@rd.example.com?key=" + "A" * 40))
    assert "not a RustDesk ID" in str(target_of("rigdesk://deus/x"))


def test_connect_dials_a_pc_not_ours_only_once_the_operator_approves_and_leaves_no_password_behind() -> None:
    import os
    import stat
    from rigdesk import adhoc, desks as desks_module
    from rigdesk.desks import ConnectAsk, DeskRefused, RigdeskSettings

    async def run() -> None:
        with tempfile.TemporaryDirectory() as state:
            dialed = Path(state) / "dialed"
            fake = Path(state) / "bridge"
            fake.write_text(f'#!/bin/sh\necho "$*" >> {dialed}\ncase "$*" in *--login-id*) echo "refused: Offline";; *) echo "refused: Wrong Password";; esac\nexit 3\n')
            fake.chmod(fake.stat().st_mode | stat.S_IXUSR)
            settings = RigdeskSettings(pcs={"dell": DeskPc(url="https://dell.tail1234.ts.net", token_file="/dev/null")}, bridge_bin=str(fake))
            desks = Desks(SimpleNamespace(state_dir=Path(state), settings=lambda _shape: settings, report=lambda *_: None, forget=lambda *_: None))  # type: ignore[arg-type]
            secrets_dir, answers = desks_module.SECRETS, adhoc.answers
            desks_module.SECRETS = Path(state) / "secrets"

            async def reached(_host: str, _port: int = 0) -> bool:
                return True
            adhoc.answers = reached  # type: ignore[assignment]
            try:
                try:
                    await desks.connect("105 517 719", "hunter2-secret", "a1", "g", "n", "A")
                except ConnectAsk as asked:
                    assert asked.target == "105517719" and "public server" in asked.where and asked.tried == ("dell",), asked
                else:
                    raise AssertionError("a PC not ours was joined unasked")
                assert "--id-server" not in dialed.read_text() and os.listdir(Path(state) / "secrets") == [] and desks.held_by("a1", "g") is None
                desks.connect_asked("desk-1", "105 517 719", "hunter2-secret", "a1", SeatId(GraphName("g"), NodeId("n")), ("p", {"ask": "desk-1"}))
                try:
                    await desks.connect_approved("desk-1", "a1", "g", "n", "A")
                except DeskRefused as refused:
                    said = str(refused)
                else:
                    raise AssertionError("a refused login joined")
                try:
                    await desks.connect_approved("desk-1", "a1", "g", "n", "A")
                except DeskRefused as refused:
                    again = str(refused)
                else:
                    raise AssertionError("an approval joined twice")
            finally:
                desks_module.SECRETS, adhoc.answers = secrets_dir, answers
            assert "public server" in said and "refused the login: Wrong Password" in said and "hunter2" not in said, said
            assert dialed.read_text().count("--login-id") == 1, dialed.read_text()
            assert "nothing was dialed" in again, again
            assert os.listdir(Path(state) / "secrets") == [] and desks.entries() == () and desks.held_by("a1", "g") is None

    asyncio.run(run())


def test_an_invite_link_dials_the_device_with_rigs_key_and_no_password_and_names_the_calling_agent() -> None:
    import json
    import os
    import stat
    from rigdesk import adhoc, desks as desks_module
    from rigdesk.desks import DeskRefused, IdServer, RigdeskSettings

    link = "rigdesk://id.105517719/" + "a" * 32 + "#" + "b" * 32

    async def run() -> None:
        with tempfile.TemporaryDirectory() as state:
            dialed, claim = Path(state) / "dialed", Path(state) / "claim"
            fake = Path(state) / "bridge"
            fake.write_text(f"""#!/bin/sh
echo "$*" >> {dialed}
[ "$1" = --pair-key ] && {{ echo x > "$2"; echo PUBLICKEY; exit 0; }}
while [ $# -gt 0 ]; do [ "$1" = --invite-file ] && cp "$2" {claim}; shift; done
echo "refused: This invite can only be redeemed by Rig"
exit 3
""")
            fake.chmod(fake.stat().st_mode | stat.S_IXUSR)
            server = IdServer(host="rd.example.com", key_file=str(Path(state) / "hbbs.pub"))
            settings = RigdeskSettings(pcs={}, bridge_bin=str(fake), id_server=server)
            desks = Desks(SimpleNamespace(state_dir=Path(state), settings=lambda _shape: settings, report=lambda *_: None, forget=lambda *_: None))  # type: ignore[arg-type]
            secrets_dir = desks_module.SECRETS
            desks_module.SECRETS = Path(state) / "secrets"
            try:
                try:
                    await desks.join(link, "a1", "g", "n", "Builder")
                except DeskRefused as refused:
                    assert "no pairing key" in str(refused), refused
                else:
                    raise AssertionError("an invite was redeemed with no pairing key")
                assert not dialed.exists()
                assert await desks.pair_key() == "PUBLICKEY"
                try:
                    await desks.join(link, "a1", "g", "n", "Builder")
                except DeskRefused as refused:
                    said = str(refused)
                else:
                    raise AssertionError("a refused invite joined")
            finally:
                desks_module.SECRETS = secrets_dir
            argv = dialed.read_text().splitlines()[-1]
            assert "--id 105517719 --id-server rd.example.com" in argv and "--once" in argv and "--pair-key-file" in argv, argv
            assert "--password-file" not in argv and "b" * 32 not in argv, argv
            assert json.loads(claim.read_text()) == {"grant": "a" * 32, "secret": "b" * 32, "agent_id": "a1", "agent_title": "Builder"}
            assert "can only be redeemed by Rig" in said and "b" * 32 not in said, said
            assert os.listdir(Path(state) / "secrets") == ["invite-pair.key"] and desks.entries() == () and desks.held_by("a1", "g") is None

    asyncio.run(run())


def test_a_connect_ask_that_lapses_is_withdrawn_from_the_operator_and_its_agent_told() -> None:
    from rigdesk import desks as desks_module

    async def run() -> None:
        with tempfile.TemporaryDirectory() as state:
            ended: list[tuple[SeatId, str, object]] = []
            told: list[str] = []

            async def route_ended(seat: SeatId, provision: str, given: object) -> int:
                ended.append((seat, provision, given))
                return 1

            async def tell(_held: object, words: str, _key: str, _timing: str = "steer") -> bool:
                told.append(words)
                return True
            desks = Desks(SimpleNamespace(state_dir=Path(state), route_ended=route_ended))  # type: ignore[arg-type]
            desks._tell = tell  # type: ignore[method-assign]
            seat, lapse = SeatId(GraphName("g"), NodeId("n")), desks_module.CONNECT_ASK_S
            desks_module.CONNECT_ASK_S = 0
            try:
                desks.connect_asked("desk-1", "105517719", "hunter2-secret", "a1", seat, ("p", {"ask": "desk-1"}))
                await asyncio.sleep(0.05)
            finally:
                desks_module.CONNECT_ASK_S = lapse
            assert ended == [(seat, "p", {"ask": "desk-1"})], ended
            assert len(told) == 1 and "lapsed" in told[0] and "request is withdrawn" in told[0] and "hunter2" not in told[0], told

    asyncio.run(run())


def test_a_pc_server_that_fails_a_call_shows_as_not_answering_with_its_own_error_until_it_answers_again() -> None:
    from rigdesk.desks import DeskRefused, McpAnswered, PcServer

    async def run() -> None:
        with tempfile.TemporaryDirectory() as state:
            desks = Desks(SimpleNamespace(state_dir=Path(state)))  # type: ignore[arg-type]
            held = Held("deus", "agent_a", "g", "n", "A", "s", 0)
            desks._sessions["deus"] = held
            link = desks._pcs["deus"] = PcLink("deus", PCS["deus"], state="up")
            link.servers["blender"] = PcServer("deus", "blender", "blender-mcp 1.2", ({"name": "get_scene"},), "sid", "2025-06-18", "s", now_ms())
            answers: list[object] = [McpAnswered("deus.blender: Unknown tool: get_scen"), DeskRefused("deus.blender answered HTTP 502: Could not connect to Blender."),
                                     {"content": [{"type": "text", "text": "Cube"}]}]

            async def post(*_args: object) -> tuple[object, None]:
                answer = answers.pop(0)
                if isinstance(answer, Exception):
                    raise answer
                return answer, None

            desks._mcp_post = post  # type: ignore[method-assign]

            def server_row() -> dict[str, object]:
                return next(row for row in desks.pcs() if row["name"] == "deus")["mcp"]["servers"][0]

            try:
                await desks.mcp_call(held, "deus.blender", "get_scen", {})
            except DeskRefused as refused:
                assert "Unknown tool" in str(refused), refused
            assert server_row()["failing"] is None, "a server that answered with an error is not failing"
            try:
                await desks.mcp_call(held, "deus.blender", "get_scene", {})
            except DeskRefused as refused:
                assert "Could not connect to Blender" in str(refused), refused
            else:
                raise AssertionError("a failed call answered")
            row = server_row()
            assert "Could not connect to Blender" in str(row["failing"]) and row["failing_since_ms"] is not None, row
            assert (await desks.mcp_call(held, "deus.blender", "get_scene", {}))["text"] == "Cube"
            assert server_row()["failing"] is None

    asyncio.run(run())



class _Asks:
    """Rig's side of the operator's asks: the asks still open, and each answer a plugin gave for the person at a PC."""
    def __init__(self, *open_asks: str) -> None:
        self.open, self.answered = set(open_asks), []

    async def confirming(self, _seat: SeatId, ask: str) -> str | None:
        return f"attention:{ask}" if ask in self.open else None

    async def confirm_answered(self, seat: SeatId, ask: str, choice: str, by: str) -> str:
        self.answered.append((seat, ask, choice, by))
        return "answered" if self.open and ask in self.open and not self.open.discard(ask) else "settled"


async def _offering_pc(desks: Desks, posted: list[tuple[str, dict]]):
    """A RigDesk PC's agent API taking /offer and /offer/end, on loopback."""
    from aiohttp import ClientSession, web

    async def took(request: web.Request) -> web.Response:
        posted.append((request.path, await request.json()))
        return web.json_response({"shown": True} if request.path == "/offer" else {"ended": True})
    app = web.Application()
    app.router.add_post("/offer", took)
    app.router.add_post("/offer/end", took)

    async def joined(request: web.Request) -> web.Response:
        posted.append((request.path, await request.json()))
        return web.json_response({"session": "s", "standing": {}})
    app.router.add_post("/join", joined)
    runner = web.AppRunner(app)
    await runner.setup()
    site = web.TCPSite(runner, "127.0.0.1", 0)
    await site.start()
    port = site._server.sockets[0].getsockname()[1]  # type: ignore[union-attr]
    desks._http = ClientSession()
    token = desks._dir / "token"
    token.write_text("rig-bearer")
    desks._pcs["front"] = PcLink("front", DeskPc(url=f"http://127.0.0.1:{port}", token_file=str(token)))
    return runner


async def _until(check, limit_s: float = 5.0) -> None:
    for _ in range(int(limit_s / 0.01)):
        if check():
            return
        await asyncio.sleep(0.01)
    raise AssertionError("never happened")


def test_allow_on_a_rigdesk_pc_answers_the_operators_ask_and_an_answer_for_another_ask_nonce_or_pc_settles_nothing() -> None:
    from rigdesk import prompts
    seat, posted, told = SeatId(GraphName("g"), NodeId("n")), [], []

    async def run() -> None:
        with tempfile.TemporaryDirectory() as state:
            asks = _Asks("desk_1")
            desks = Desks(SimpleNamespace(state_dir=Path(state), confirming=asks.confirming, confirm_answered=asks.confirm_answered))  # type: ignore[arg-type]

            async def tell(held: Held, words: str, key: str, timing: str = "steer") -> bool:
                told.append((held.agent, words, key))
                return True
            desks._tell = tell  # type: ignore[method-assign]
            runner = await _offering_pc(desks, posted)
            try:
                link = desks._pcs["front"]
                prompts.start(desks, "front", "desk_1", seat, "a1", "g", "Builder")
                await _until(lambda: posted)
                path, body = posted[0]
                assert path == "/offer" and body["ask"] == "desk_1" and body["agent"] == {"id": "a1", "title": "Builder", "graph": "g"}
                nonce = body["nonce"]
                assert len(nonce) >= 16
                await desks._event(link, {"type": "offer", "ask": "desk_1", "nonce": "f" * 32, "answer": "allow"})
                await desks._event(link, {"type": "offer", "ask": "desk_2", "nonce": nonce, "answer": "allow"})
                await desks._event(PcLink("deus", PCS["deus"]), {"type": "offer", "ask": "desk_1", "nonce": nonce, "answer": "allow"})
                await asyncio.sleep(0.05)
                assert asks.answered == [], "an answer not bound to the shown prompt was taken"
                await desks._event(link, {"type": "offer", "ask": "desk_1", "nonce": nonce, "answer": "allow"})
                await _until(lambda: told)
            finally:
                await prompts.end_all(desks)
                await desks._http.close()
                await runner.cleanup()
            assert asks.answered == [(seat, "desk_1", "accept", "rigdesk:front")]
            assert told == [("a1", told[0][1], "prompt:desk_1")] and "person at front allowed" in told[0][1], told
            assert desks.prompts == {} and [path for path, _ in posted] == ["/offer"], posted

    asyncio.run(run())


def test_the_pcs_journal_is_told_a_join_was_the_persons_and_not_the_operators() -> None:
    from rigdesk import prompts
    seat, posted = SeatId(GraphName("g"), NodeId("n")), []

    async def run() -> None:
        with tempfile.TemporaryDirectory() as state:
            asks = _Asks("desk_1")

            async def then(_seat: SeatId, ask: str, _choice: str, _by: str) -> str:
                await desks.join_approved("front", "a1", "g", "n", "Builder", ask)
                return "answered"
            desks = Desks(SimpleNamespace(state_dir=Path(state), confirming=asks.confirming, confirm_answered=then))  # type: ignore[arg-type]

            async def tell(*_args: object, **_kw: object) -> bool:
                return True
            desks._tell = tell  # type: ignore[method-assign]
            runner = await _offering_pc(desks, posted)
            try:
                link = desks._pcs["front"]
                prompts.start(desks, "front", "desk_1", seat, "a1", "g", "Builder")
                await _until(lambda: posted)
                await desks._event(link, {"type": "offer", "ask": "desk_1", "nonce": posted[0][1]["nonce"], "answer": "allow"})
                await _until(lambda: "front" in desks._sessions)
                del desks._sessions["front"]
                await desks.join_approved("front", "a1", "g", "n", "Builder", "desk_2")
            finally:
                await prompts.end_all(desks)
                await desks._http.close()
                await runner.cleanup()
            joins = [body for path, body in posted if path == "/join"]
            assert [join.get("answered_by") for join in joins] == ["person", None], joins
            assert desks.person_answered == set()

    asyncio.run(run())


def test_rigs_answer_first_takes_the_prompt_off_a_rigdesk_pc_and_its_late_allow_settles_nothing() -> None:
    from rigdesk import prompts
    seat, posted = SeatId(GraphName("g"), NodeId("n")), []

    async def run() -> None:
        with tempfile.TemporaryDirectory() as state:
            asks = _Asks("desk_1", "desk_2")
            desks = Desks(SimpleNamespace(state_dir=Path(state), confirming=asks.confirming, confirm_answered=asks.confirm_answered))  # type: ignore[arg-type]
            runner = await _offering_pc(desks, posted)
            every, prompts.OPEN_EVERY_S = prompts.OPEN_EVERY_S, 0.01
            try:
                link = desks._pcs["front"]
                prompts.start(desks, "front", "desk_1", seat, "a1", "g", "Builder")
                prompts.start(desks, "front", "desk_2", seat, "a2", "g", "Tester")
                await _until(lambda: len(posted) == 2)
                nonces = {body["ask"]: body["nonce"] for _, body in posted}
                prompts.end(desks, "desk_1")
                asks.open.discard("desk_2")
                await _until(lambda: len(posted) == 4)
                for ask, nonce in nonces.items():
                    await desks._event(link, {"type": "offer", "ask": ask, "nonce": nonce, "answer": "allow"})
                await asyncio.sleep(0.05)
            finally:
                prompts.OPEN_EVERY_S = every
                await prompts.end_all(desks)
                await desks._http.close()
                await runner.cleanup()
            assert sorted(body["ask"] for path, body in posted if path == "/offer/end") == ["desk_1", "desk_2"], posted
            assert asks.answered == [] and desks.prompts == {}

    asyncio.run(run())


def test_a_stock_pc_is_asked_through_its_bridge_and_its_allow_answers_the_ask_with_no_password_kept() -> None:
    from rigdesk import prompts
    from rigdesk.desks import StockRemote
    seat, posted = SeatId(GraphName("g"), NodeId("n")), []

    async def run() -> None:
        with tempfile.TemporaryDirectory() as state:
            asks = _Asks("desk_1")
            desks = Desks(SimpleNamespace(state_dir=Path(state), confirming=asks.confirming, confirm_answered=asks.confirm_answered))  # type: ignore[arg-type]

            async def tell(*_args: object, **_kw: object) -> bool:
                return True
            desks._tell = tell  # type: ignore[method-assign]
            runner = await _offering_pc(desks, posted)
            bridge = desks._pcs.pop("front")
            link = desks._pcs["deus"] = PcLink("deus", DeskPc(url=bridge.spec.url, token_file=bridge.spec.token_file, stock=StockRemote(peer="deus.tail1234.ts.net")))
            try:
                prompts.start(desks, "deus", "desk_1", seat, "a1", "g", "Builder")
                await _until(lambda: posted)
                path, body = posted[0]
                assert path == "/offer" and body["agent"]["title"] == "Builder"
                await desks._event(link, {"type": "offer", "ask": "desk_1", "nonce": body["nonce"], "answer": "allow"})
                await _until(lambda: asks.answered)
            finally:
                await prompts.end_all(desks)
                await desks._http.close()
                await runner.cleanup()
            assert asks.answered == [(seat, "desk_1", "accept", "rigdesk:deus")] and desks.prompts == {}

    asyncio.run(run())

if __name__ == "__main__":
    for name, test in list(globals().items()):
        if name.startswith("test_"):
            test()
            print("ok", name)
