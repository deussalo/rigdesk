"""Run from a Rig checkout: PYTHONPATH=.:<rigdesk>/rig-plugin .venv/bin/python <rigdesk>/rig-plugin/tests/test_rigdesk.py (or with pytest)."""
import asyncio
import json
import tempfile
import time
from pathlib import Path
from types import SimpleNamespace

from core.base import now_ms
from core.model import GraphName, NodeId, SeatId
from rigdesk.desks import PERSON_ENDED, DeskPc, DeskRefused, Desks, Held, PcLink, RedirectRefused, pc_of_link, token_session, words_of
from rigdesk.health import Sample

PCS = {"deus": DeskPc(url="https://deus.tail1234.ts.net", token_file="/dev/null", client="acme")}
GRANT, SECRET = "a" * 32, "b" * 32


def test_a_link_names_its_pc_by_name_or_tailnet_host() -> None:
    assert pc_of_link(f"rigdesk://deus/{GRANT}#{SECRET}", PCS) == ("deus", GRANT, SECRET)
    assert pc_of_link(f"rigdesk://deus.tail1234.ts.net/{GRANT}#{SECRET}", PCS) == ("deus", GRANT, SECRET)
    assert "no RigDesk PC named other" in str(pc_of_link(f"rigdesk://other/{GRANT}#{SECRET}", PCS))
    assert pc_of_link(f"rigdesk://desktop_ab12/{GRANT}#{SECRET}", PCS, {"deus": "DESKTOP_AB12"}) == ("deus", GRANT, SECRET)
    assert "not a RigDesk share link" in str(pc_of_link("https://example.com", PCS))


def test_the_agent_hears_only_of_a_holder_change_that_lets_it_carry_on_and_not_of_its_own_leave() -> None:
    for holder, by in (("human", "viewer input"), ("agent", "quiet"), ("blocked", "block"), ("agent", "share"), ("agent", "operator"), ("blocked", "stop")):
        assert words_of("deus", {"type": "holder", "holder": holder, "by": by, "at_ms": 0}) is None, by
    resumed = str(words_of("deus", {"type": "holder", "holder": "agent", "by": "unblock", "at_ms": 0}))
    handed = str(words_of("deus", {"type": "holder", "holder": "agent", "by": "hand to agent", "at_ms": 0}))
    assert "resumed you" in resumed and "handed input back to you" in handed and all("Carry on there by yourself" in said for said in (resumed, handed))
    assert words_of("deus", {"type": "revoked", "agent_id": "a", "reason": "the agent left", "at_ms": 0}) is None
    assert "session there ended" in str(words_of("deus", {"type": "revoked", "agent_id": "a", "reason": "the human ended the share", "at_ms": 0}))
    closed = str(words_of("deus", {"type": "revoked", "agent_id": "a", "reason": PERSON_ENDED, "at_ms": 0}))
    assert "the person at deus ended it" in closed and "Nothing reconnects by itself" in closed, closed


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


def test_stop_interrupts_the_owning_agents_turn_and_tells_it_nothing_and_a_replayed_stop_does_neither() -> None:
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
    assert done == [("interrupt", "interrupt")]


def test_an_agent_refused_while_the_person_uses_the_pc_is_told_once_when_they_are_inactive_and_one_never_refused_is_not() -> None:
    from aiohttp import ClientSession, web
    refusal = "the person is using the PC (since 09:00:00 UTC); reads still work; you will be notified when they have been inactive for more than 5 seconds"

    standing = {"holder": "human", "since_ms": 1}

    async def mcp(_request: web.Request) -> web.Response:
        return web.json_response({"jsonrpc": "2.0", "id": 1, "result": {"isError": True, "content": [{"type": "text", "text": refusal}]}})

    async def health(_request: web.Request) -> web.Response:
        return web.json_response({"ok": True, "standing": standing})

    async def run() -> None:
        app = web.Application()
        app.router.add_post("/mcp", mcp)
        app.router.add_get("/health", health)
        runner = web.AppRunner(app)
        await runner.setup()
        await web.TCPSite(runner, "127.0.0.1", 0).start()
        with tempfile.TemporaryDirectory() as state:
            token = Path(state) / "token"
            token.write_text("t" * 32)
            desks = Desks(SimpleNamespace(state_dir=Path(state)))  # type: ignore[arg-type]
            told: list[str] = []

            async def tell(_held: Held, words: str, *_args: object) -> bool:
                told.append(words)
                return True

            desks._tell = tell  # type: ignore[method-assign]
            held = desks._sessions["deus"] = Held("deus", "agent_a", "g", "n", "A", "s", 0)
            link = desks._pcs["deus"] = PcLink("deus", DeskPc(url=f"http://127.0.0.1:{runner.addresses[0][1]}", token_file=str(token)), state="up")
            desks._http = ClientSession()
            ids = iter(range(1, 100))

            async def holder(holder: str, by: str) -> None:
                await desks._event(link, {"type": "holder", "holder": holder, "by": by, "id": next(ids), "at_ms": now_ms()})
            try:
                await holder("human", "local input")
                await holder("agent", "quiet")
                assert told == [], "an agent never refused is not told"
                await holder("human", "viewer input")
                for _ in range(2):
                    assert (await desks.call(held, "type", {"text": "x"}))["is_error"]
                assert json.loads(desks._state_file.read_text())["sessions"]["deus"]["refused_human"] is True, "the promise outlives a restart"
                await holder("agent", "quiet")
                await holder("human", "local input")
                await holder("agent", "quiet")
                assert len(told) == 1 and "no longer using the PC" in told[0] and "Carry on there by yourself" in told[0], told
                await holder("human", "local input")
                await holder("agent", "quiet")
                standing["holder"] = "agent"
                assert (await desks.call(held, "type", {"text": "x"}))["is_error"]
                assert len(told) == 2, "a hand-back that passed before the refused answer is still told"
                await holder("human", "local input")
                await holder("agent", "quiet")
                assert len(told) == 2, told
            finally:
                await desks._http.close()
                await runner.cleanup()

    asyncio.run(run())


async def _next_call(desks: Desks, agent: str = "a1", graph: str = "g") -> str:
    """The words of the agent's next desk/click, which it holds no session to make."""
    from rigdesk import PassInput, _passed
    call = SimpleNamespace(contract=SimpleNamespace(place=SimpleNamespace(agent=agent)), graph=graph, node="n", refused=lambda _code, words, *_: words)
    return await _passed(desks, "click", {}).run(call, PassInput())  # type: ignore[arg-type]


def test_a_session_that_ends_by_itself_sends_the_agent_nothing_and_its_next_desk_call_says_why() -> None:
    admitted: list[object] = []

    async def admit(_seat: SeatId, said: object) -> object:
        admitted.append(said)
        return object()

    async def seat_of(*_args: object) -> SeatId:
        return SeatId(GraphName("g"), NodeId("n"))

    async def leave(held: Held) -> bool:
        desks._sessions.pop(held.pc)
        return True

    async def ended_by(cause: str) -> None:
        held = desks._sessions["deus"] = Held("deus", "a1", "g", "n", "A", "s", now_ms() - 6 * 60_000, "assigned")
        if cause == "person":
            await desks._event(PcLink("deus", PCS["deus"]), {"type": "revoked", "agent_id": "a1", "reason": "the person at the PC ended the session", "at_ms": now_ms()})
        elif cause == "closed":
            await desks._event(PcLink("deus", PCS["deus"]), {"type": "revoked", "agent_id": "a1", "reason": PERSON_ENDED, "at_ms": now_ms()})
        elif cause == "restart":
            desks._boots["deus"] = "boot-1"
            await desks._rebooted("deus", "boot-2")
        elif cause == "idle":
            await desks._idle_leaves()
        elif cause == "unassigned":
            await desks._ungranted(held)

    async def run() -> None:
        nonlocal desks
        with tempfile.TemporaryDirectory() as state:
            desks = Desks(SimpleNamespace(state_dir=Path(state), admit=admit, seat_of=seat_of, report=lambda *_: None, forget=lambda *_: None))  # type: ignore[arg-type]
            desks.leave = leave  # type: ignore[method-assign]
            assert (await _next_call(desks)).startswith("you hold no RigDesk session")
            for cause, why in (("person", "the person at the PC ended the session"), ("closed", "the person at deus ended it"), ("restart", "RigDesk restarted there"),
                               ("idle", "you left deus after 5 minutes without a desk call"), ("unassigned", "took away your standing access to deus")):
                await ended_by(cause)
                said = await _next_call(desks)
                assert "deus" not in desks._sessions and why in said and "You hold no RigDesk session" in said, (cause, said)
                assert await _next_call(desks) == said, "it says why until the agent joins again"
            assert (await _next_call(desks, "a2")).startswith("you hold no RigDesk session"), "another agent is not told of a1's session"

    desks: Desks
    asyncio.run(run())
    assert admitted == []


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


def test_always_allow_on_a_rigdesk_pc_grants_and_joins_through_join_and_unassign_ends_that_session_and_the_next_call_says_so() -> None:
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
            assert "took away your standing access to front" in await _next_call(desks)
            desks._pcs["busy"] = PcLink("busy", DeskPc(url="https://busy.tail1234.ts.net", token_file="/dev/null"))
            try:
                await _always_allowed(desks, {"pc": "busy", "agent": "a1", "graph": "g", "kind": "join", "ask": "desk_2"}, "n", "A")
            except DeskRefused:
                pass
            else:
                raise AssertionError("a refused join was taken as joined")
            assert desks.granted("busy") == (), "a grant whose join failed is taken back"

    asyncio.run(run())
    assert told == [], told


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
            assert "RigDesk deus: you left deus after 5 minutes" in await _next_call(desks)

    asyncio.run(run())
    assert left == ["deus"]
    assert told == [], told


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


def test_only_a_tailnet_address_is_a_direct_peer_and_a_key_still_means_an_id_server() -> None:
    from rigdesk.adhoc import PUBLIC_KEY, Target, target_of
    assert target_of("105 517 719@100.64.0.1") == Target("105517719", None, None, "100.64.0.1:21118")
    assert target_of("105517719@100.127.255.254:21119") == Target("105517719", None, None, "100.127.255.254:21119")
    assert target_of("105517719@Front.Tail1234.TS.NET") == Target("105517719", None, None, "front.tail1234.ts.net:21118")
    for server in ("100.64.0.1", "8.8.8.8", "front.tail1234.ts.net:21116", "rd.example.com"):
        assert target_of(f"105517719@{server}?key={PUBLIC_KEY}") == Target("105517719", server, PUBLIC_KEY), server
    for outside in ("8.8.8.8", "8.8.8.8:21118", "100.63.255.255", "100.128.0.1", "192.168.1.5", "127.0.0.1", "rd.example.com", "ts.net", "eviltS.net", "front.ts.net.example.com",
                    "100.64.0.1.example.com", "[fd7a:115c:a1e0::1]", "[fd7a:115c:a1e0::1]:21118", "100.64.0.1:0", "100.64.0.1:65536", "100.64.0.1:port", "front..ts.net", "front.ts.net."):
        said = target_of(f"105517719@{outside}")
        assert isinstance(said, str) and "give its public key too" in said, (outside, said)


def test_connect_dials_a_tailnet_peer_directly_only_once_the_operator_approves_and_never_off_the_tailnet() -> None:
    import stat
    from rigdesk import adhoc, desks as desks_module
    from rigdesk.desks import ConnectAsk, DeskRefused, RigdeskSettings

    async def run() -> None:
        with tempfile.TemporaryDirectory() as state:
            dialed = Path(state) / "dialed"
            fake = Path(state) / "bridge"
            fake.write_text(f'#!/bin/sh\necho "$*" >> {dialed}\necho "refused: Wrong Password"\nexit 3\n')
            fake.chmod(fake.stat().st_mode | stat.S_IXUSR)
            settings = RigdeskSettings(pcs={}, bridge_bin=str(fake))
            desks = Desks(SimpleNamespace(state_dir=Path(state), settings=lambda _shape: settings, report=lambda *_: None, forget=lambda *_: None))  # type: ignore[arg-type]
            names = {"good.tail1234.ts.net": ["100.70.1.2"], "mixed.tail1234.ts.net": ["100.70.1.3", "8.8.8.8"], "public.tail1234.ts.net": ["8.8.8.8"], "gone.tail1234.ts.net": []}

            async def lookup(name: str) -> list[str]:
                return names[name]
            secrets_dir, kept_lookup = desks_module.SECRETS, adhoc.lookup
            desks_module.SECRETS, adhoc.lookup = Path(state) / "secrets", lookup  # type: ignore[assignment]

            async def approved(given: str) -> str:
                try:
                    await desks.connect(given, "hunter2-secret", "a1", "g", "n", "A")
                except ConnectAsk as asked:
                    assert "over the tailnet" in asked.where and not dialed.exists(), asked.where
                else:
                    raise AssertionError("a PC not ours was joined unasked")
                desks.connect_asked("desk-1", given, "hunter2-secret", "a1", SeatId(GraphName("g"), NodeId("n")), ("p", {"ask": "desk-1"}))
                try:
                    await desks.connect_approved("desk-1", "a1", "g", "n", "A")
                except DeskRefused as refused:
                    assert "Wrong Password" in str(refused) and "hunter2" not in str(refused), refused
                argv = dialed.read_text().strip().split()
                dialed.unlink()
                assert "--id-server" not in argv and argv[argv.index("--login-id") + 1] == "105517719", argv
                return argv[argv.index("--peer") + 1]
            try:
                assert await approved("105517719@100.70.1.9") == "100.70.1.9:21118"
                assert await approved("105517719@100.70.1.9:21119") == "100.70.1.9:21119"
                assert await approved("105517719@good.tail1234.ts.net") == "100.70.1.2:21118"
                for host, words in (("8.8.8.8", "give its public key too"), ("mixed.tail1234.ts.net", "8.8.8.8"), ("public.tail1234.ts.net", "8.8.8.8"), ("gone.tail1234.ts.net", "no address")):
                    try:
                        await desks.connect(f"105517719@{host}", "hunter2-secret", "a1", "g", "n", "A", approved=True)
                    except DeskRefused as refused:
                        assert words in str(refused), (host, refused)
                    else:
                        raise AssertionError(f"{host} was dialed")
                    assert not dialed.exists(), host
            finally:
                desks_module.SECRETS, adhoc.lookup = secrets_dir, kept_lookup

    asyncio.run(run())


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


def test_an_invite_link_dials_only_a_pc_set_up_on_the_tailnet_with_rigs_key_and_no_password_and_names_the_calling_agent() -> None:
    import json
    import os
    import stat
    from rigdesk import desks as desks_module
    from rigdesk.desks import DeskRefused, IdServer, RigdeskSettings, StockRemote

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
            phone = await asyncio.start_server(lambda _reader, writer: writer.close(), "127.0.0.1", 0)
            peer = f"127.0.0.1:{phone.sockets[0].getsockname()[1]}"
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
                    assert "stock.login_id" in str(refused) and "b" * 32 not in str(refused), refused
                else:
                    raise AssertionError("an invite joined a device no PC in settings is")
                assert len(dialed.read_text().splitlines()) == 1, "an invite was dialed through an ID server"
                entry = DeskPc(url="http://127.0.0.1:21120", token_file="/dev/null", stock=StockRemote(peer=peer, login_id="105517719"))
                settings = RigdeskSettings(pcs={"phone": entry}, bridge_bin=str(fake), id_server=server)
                try:
                    await desks.join(link, "a1", "g", "n", "Builder")
                except DeskRefused as refused:
                    said = str(refused)
                else:
                    raise AssertionError("a refused invite joined")
            finally:
                phone.close()
                await phone.wait_closed()
                desks_module.SECRETS = secrets_dir
            lines = dialed.read_text().splitlines()
            argv = lines[-1]
            assert len(lines) == 2 and f"--peer {peer} --login-id 105517719" in argv and "--once" in argv and "--pair-key-file" in argv, lines
            assert "--id-server" not in argv, argv
            assert "--password-file" not in argv and "b" * 32 not in argv, argv
            assert json.loads(claim.read_text()) == {"grant": "a" * 32, "secret": "b" * 32, "agent_id": "a1", "agent_title": "Builder"}
            assert "can only be redeemed by Rig" in said and "b" * 32 not in said, said
            assert os.listdir(Path(state) / "secrets") == ["invite-pair.key"] and desks.entries() == () and desks.held_by("a1", "g") is None

    asyncio.run(run())


def _invite_tools(state: Path, said: str, settings: dict, first: str = "") -> tuple[Desks, SimpleNamespace, Path, list]:
    """Desks and a tool call as the plugin's tools get them, with a bridge that records its arguments and answers `said`."""
    import stat
    from rigdesk.desks import PAIR_KEY, SECRETS, RigdeskSettings

    dialed, fake = state / "dialed", state / "bridge"
    fake.write_text(f'#!/bin/sh\necho "$*" >> {dialed}\n{first}\necho "{said}"\nexit 3\n')
    fake.chmod(fake.stat().st_mode | stat.S_IXUSR)
    SECRETS.mkdir(mode=0o700)
    (SECRETS / PAIR_KEY).write_text("x")
    asked: list = []

    async def confirm(*given: object) -> str:
        asked.append(given)
        return "open"

    async def caller_title() -> str:
        return "Builder"
    shaped = RigdeskSettings(bridge_bin=str(fake), **settings)
    desks = Desks(SimpleNamespace(state_dir=state, settings=lambda _shape: shaped, report=lambda *_: None, forget=lambda *_: None, confirm=confirm))  # type: ignore[arg-type]
    place = SimpleNamespace(agent="a1", seat=SeatId(GraphName("g"), NodeId("n")))
    call = SimpleNamespace(contract=SimpleNamespace(place=place), graph="g", node="n", kernel=SimpleNamespace(caller_title=caller_title, activation_id="act-1"),
                           refused=lambda code, words, *_: {"refused": code, "said": words})
    return desks, call, dialed, asked


def _with_invite_tools(body) -> None:
    from rigdesk import desks as desks_module

    async def run() -> None:
        with tempfile.TemporaryDirectory() as state:
            secrets_dir = desks_module.SECRETS
            desks_module.SECRETS = Path(state) / "secrets"
            try:
                await body(Path(state))
            finally:
                desks_module.SECRETS = secrets_dir

    asyncio.run(run())


INVITE_LINK = "rigdesk://id.105517719/" + "a" * 32 + "#" + "b" * 32


async def _two_pcs_up(body) -> None:
    """Calls body with settings for two stock PCs listening on loopback, the device 105517719 and another, and the connections each has taken."""
    from rigdesk.desks import ConnectServer, IdServer, StockRemote

    taken = {"phone": 0, "other": 0}

    def counting(name: str):
        def accepted(_reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
            taken[name] += 1
            writer.close()
        return accepted
    phone, other = [await asyncio.start_server(counting(name), "127.0.0.1", 0) for name in taken]
    peers = [f"127.0.0.1:{server.sockets[0].getsockname()[1]}" for server in (phone, other)]
    pcs = {"phone": DeskPc(url="http://127.0.0.1:21120", token_file="/dev/null", stock=StockRemote(peer=peers[0], login_id="105517719")),
           "other": DeskPc(url="http://127.0.0.1:21121", token_file="/dev/null", stock=StockRemote(peer=peers[1], login_id="999888777"))}
    settings = {"pcs": pcs, "id_server": IdServer(host="rd.example.com", key_file="/dev/null"), "connect_server": ConnectServer(host="rd.example.com", key="A" * 44)}
    try:
        await body(settings, peers[0], taken)
    finally:
        for server in (phone, other):
            server.close()
            await server.wait_closed()


def test_an_invite_for_an_id_no_entry_declares_connects_to_nothing_and_asks_nobody() -> None:
    from rigdesk import JoinInput, _join

    async def body(state: Path) -> None:
        async def with_pcs(settings: dict, _peer: str, taken: dict[str, int]) -> None:
            desks, call, dialed, asked = _invite_tools(state, "refused: Offline", settings)
            answer = await _join(desks, call, JoinInput(link="rigdesk://id.123123123/" + "a" * 32 + "#" + "b" * 32))  # type: ignore[arg-type]
            assert answer["refused"] and "stock.login_id" in answer["said"] and "b" * 32 not in answer["said"], answer
            assert not dialed.exists() and taken == {"phone": 0, "other": 0}, taken
            assert asked == [] and desks.entries() == () and desks.held_by("a1", "g") is None
        await _two_pcs_up(with_pcs)

    _with_invite_tools(body)


def test_an_invite_goes_only_to_the_entry_declaring_its_id() -> None:
    from rigdesk import JoinInput, _join

    async def body(state: Path) -> None:
        async def with_pcs(settings: dict, peer: str, taken: dict[str, int]) -> None:
            desks, call, dialed, asked = _invite_tools(state, "refused: Offline", settings)
            answer = await _join(desks, call, JoinInput(link=INVITE_LINK))  # type: ignore[arg-type]
            lines = dialed.read_text().splitlines()
            assert len(lines) == 1 and f"--peer {peer} --login-id 105517719" in lines[0] and "--invite-file" in lines[0], lines
            assert "--id-server" not in lines[0], lines
            assert taken["other"] == 0, taken
            assert "phone did not show it holds this invite" in answer["said"] and "stock.login_id on phone" in answer["said"], answer
            assert asked == [] and desks.entries() == ()
        await _two_pcs_up(with_pcs)

    _with_invite_tools(body)


def test_a_device_held_through_its_invite_cannot_also_be_joined_through_its_settings_entry() -> None:
    from aiohttp import ClientSession, web
    from core.tasks import TASKS
    from rigdesk import JoinInput, _join
    from rigdesk.desks import DeskRefused, StockRemote

    async def claimed(_request: web.Request) -> web.Response:
        return web.json_response({"session": "s2"})

    async def body(state: Path) -> None:
        app = web.Application()
        app.router.add_post("/claim", claimed)
        runner = web.AppRunner(app)
        await runner.setup()
        site = web.TCPSite(runner, "127.0.0.1", 0)
        await site.start()
        port = runner.addresses[0][1]
        go, token = state / "go", state / "token"
        token.write_text("t0ken")
        entry = DeskPc(url=f"http://127.0.0.1:{port}", token_file=str(token), stock=StockRemote(peer="127.0.0.1:1", login_id="105517719"))
        desks, call, dialed, _ = _invite_tools(state, "refused: Offline", {"pcs": {"phone": entry}}, f"until [ -e {go} ]; do sleep 0.05; done")
        desks._pcs["phone"] = PcLink("phone", entry)
        desks._http = ClientSession()
        share = f"rigdesk://phone/{GRANT}#{SECRET}"
        try:
            invited = TASKS.spawn("test-invited-join", _join(desks, call, JoinInput(link=INVITE_LINK)))  # type: ignore[arg-type]
            await _until(dialed.exists)
            desks._sessions["id-105517719"] = Held("id-105517719", "a1", "g", "n", "Builder", "s", now_ms(), "adhoc")
            for attempt in (desks.join(share, "a2", "g", "n2", "Other"), desks.join_approved("phone", "a2", "g", "n2", "Other")):
                try:
                    await attempt
                except DeskRefused as refused:
                    assert "Builder holds phone" in str(refused), refused
                else:
                    raise AssertionError("two agents hold one device")
            go.write_text("")
            await invited
            assert (await desks.join(share, "a2", "g", "n2", "Other")).pc == "phone", "the entry stayed closed after the invite's session ended"
        finally:
            go.write_text("")
            await desks._http.close()
            await runner.cleanup()

    _with_invite_tools(body)


def test_connecting_to_a_pc_not_ours_still_waits_for_the_operators_accept() -> None:
    from rigdesk import ConnectInput, _connect, adhoc

    async def reached(_host: str, _port: int = 0) -> bool:
        return True

    async def body(state: Path) -> None:
        pcs = {"dell": DeskPc(url="https://dell.tail1234.ts.net", token_file="/dev/null")}
        desks, call, dialed, asked = _invite_tools(state, "refused: Offline", {"pcs": pcs})
        answers, adhoc.answers = adhoc.answers, reached  # type: ignore[assignment]
        try:
            answer = await _connect(desks, call, ConnectInput(id="105517719", password="hunter2-secret", why="to fix the printer"))  # type: ignore[arg-type]
        finally:
            adhoc.answers = answers
        desks.connect_dropped(answer["ask"])
        assert answer["joined"] is False and answer["state"] == "open" and len(asked) == 1, answer
        assert asked[0][4].operator_only, "desk/connect's ask is the operator's alone"
        assert "--id-server" not in dialed.read_text(), dialed.read_text()

    _with_invite_tools(body)


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


def test_passwords_saved_at_once_start_one_bridge_and_the_newest_one_stays() -> None:
    from rigdesk.desks import StockRemote

    async def run() -> None:
        with tempfile.TemporaryDirectory() as state:
            desks = Desks(SimpleNamespace(state_dir=Path(state), report=lambda *_: None))  # type: ignore[arg-type]
            password, started = Path(state) / "phone.password", Path(state) / "started"
            password.write_text("old-password")
            bridge = Path(state) / "bridge"
            bridge.write_text(f"#!/bin/sh\nwhile [ \"$1\" != --password-file ]; do shift; done\ncat \"$2\" >> {started}; echo >> {started}\nexec sleep 60\n")
            bridge.chmod(0o700)
            desks._bridge_bin = str(bridge)
            stock = StockRemote(peer="moto-g57.tail1234.ts.net", password_file=str(password))
            link = desks._pcs["moto-g57"] = PcLink("moto-g57", DeskPc(url="http://127.0.0.1:21120", token_file="/dev/null", stock=stock))
            link.remote = asyncio.get_running_loop().create_task(desks._remote(link, stock))
            write = desks._replaced_secret
            writes: list[str] = []

            def first_write_is_slow(path: Path, value: str) -> None:
                writes.append(value)
                if len(writes) == 1:
                    time.sleep(0.3)
                write(path, value)
            desks._replaced_secret = first_write_is_slow  # type: ignore[method-assign]

            async def save_after_the_first_began() -> None:
                await asyncio.sleep(0.05)
                await desks.set_password("moto-g57", "last-password")
            try:
                await _until(lambda: started.exists() and started.read_text() == "old-password\n")
                await asyncio.gather(desks.set_password("moto-g57", "first-password"), save_after_the_first_began())
                await _until(lambda: started.read_text() != "old-password\n")
                await asyncio.sleep(0.3)
                assert started.read_text() == "old-password\nlast-password\n"
                assert password.read_text() == "last-password"
            finally:
                if link.remote is not None:
                    link.remote.cancel()
                    await asyncio.gather(link.remote, return_exceptions=True)

    asyncio.run(run())


def test_a_cancelled_password_save_still_ends_with_the_newest_password_and_a_running_bridge() -> None:
    from rigdesk.desks import StockRemote

    async def run() -> None:
        with tempfile.TemporaryDirectory() as state:
            desks = Desks(SimpleNamespace(state_dir=Path(state), report=lambda *_: None))  # type: ignore[arg-type]
            password, started = Path(state) / "phone.password", Path(state) / "started"
            password.write_text("old-password")
            bridge = Path(state) / "bridge"
            bridge.write_text(f"#!/bin/sh\nwhile [ \"$1\" != --password-file ]; do shift; done\ncat \"$2\" >> {started}; echo >> {started}\nexec sleep 60\n")
            bridge.chmod(0o700)
            desks._bridge_bin = str(bridge)
            stock = StockRemote(peer="moto-g57.tail1234.ts.net", password_file=str(password))
            link = desks._pcs["moto-g57"] = PcLink("moto-g57", DeskPc(url="http://127.0.0.1:21120", token_file="/dev/null", stock=stock))
            write = desks._replaced_secret
            writes: list[str] = []

            def first_write_is_slow(path: Path, value: str) -> None:
                writes.append(value)
                if len(writes) == 1:
                    time.sleep(0.3)
                write(path, value)
            desks._replaced_secret = first_write_is_slow  # type: ignore[method-assign]

            async def slow_to_stop() -> None:
                try:
                    await asyncio.sleep(60)
                except asyncio.CancelledError:
                    await asyncio.sleep(0.2)
                    raise

            async def cancelled_soon(new_password: str) -> None:
                call = asyncio.get_running_loop().create_task(desks.set_password("moto-g57", new_password))
                await asyncio.sleep(0.05)
                call.cancel()
                await asyncio.gather(call, return_exceptions=True)
            try:
                await cancelled_soon("cancelled-in-its-write")
                await desks.set_password("moto-g57", "newest-password")
                await _until(lambda: started.exists())
                await asyncio.sleep(0.3)
                assert password.read_text() == "newest-password"
                assert started.read_text() == "newest-password\n"

                assert link.remote is not None
                link.remote.cancel()
                await asyncio.gather(link.remote, return_exceptions=True)
                link.remote = asyncio.get_running_loop().create_task(slow_to_stop())
                await cancelled_soon("cancelled-while-the-bridge-stops")
                await _until(lambda: started.read_text().endswith("cancelled-while-the-bridge-stops\n"))
            finally:
                if link.remote is not None:
                    link.remote.cancel()
                    await asyncio.gather(link.remote, return_exceptions=True)

    asyncio.run(run())


def test_the_operator_replaces_a_stock_pcs_saved_password_and_its_bridge_logs_in_with_it_at_once() -> None:
    from rigdesk.desks import DeskRefused, StockRemote

    async def run() -> None:
        with tempfile.TemporaryDirectory() as state:
            desks = Desks(SimpleNamespace(state_dir=Path(state), report=lambda *_: None))  # type: ignore[arg-type]
            told: list[str] = []

            async def tell(_held: Held, words: str, _key: str, _timing: str = "steer") -> bool:
                told.append(words)
                return True
            desks._tell = tell  # type: ignore[method-assign]
            password, started = Path(state) / "phone.password", Path(state) / "started"
            password.write_text("old-password")
            bridge = Path(state) / "bridge"
            bridge.write_text(f"#!/bin/sh\nwhile [ \"$1\" != --password-file ]; do shift; done\ncat \"$2\" >> {started}; echo >> {started}\nexec sleep 60\n")
            bridge.chmod(0o700)
            desks._bridge_bin = str(bridge)
            stock = StockRemote(peer="moto-g57.tail1234.ts.net", password_file=str(password))
            link = desks._pcs["moto-g57"] = PcLink("moto-g57", DeskPc(url="http://127.0.0.1:21120", token_file="/dev/null", stock=stock))
            desks._pcs["deus"] = PcLink("deus", PCS["deus"])
            link.remote = asyncio.get_running_loop().create_task(desks._remote(link, stock))
            try:
                await _until(lambda: started.exists() and started.read_text() == "old-password\n")
                desks._sessions["moto-g57"] = Held("moto-g57", "agent_a", "g", "n", "A", "s", 0)
                desks._unsent["moto-g57"] = {5: {"kind": "message", "text": "for agent_a"}}
                both = await asyncio.gather(desks.set_password("moto-g57", "new-password"), desks.set_password("moto-g57", "new-password"))
                assert both == [str(password), str(password)]
                await _until(lambda: started.read_text() == "old-password\nnew-password\n")
                await asyncio.sleep(0.2)
                assert started.read_text() == "old-password\nnew-password\n", "two calls at once started two bridges"
                assert "moto-g57" not in desks._sessions and told == [], told
                assert "replaced its saved password" in await _next_call(desks, "agent_a")
                assert "moto-g57" not in desks._unsent, "a message for the ended session would go to the PC's next agent"
                assert password.read_text() == "new-password" and password.stat().st_mode & 0o777 == 0o600
                rows = {row["name"]: row for row in desks.pcs()}
                assert rows["moto-g57"]["password_file"] == str(password) and rows["deus"]["password_file"] is None
                assert "new-password" not in json.dumps(rows, default=str)
                try:
                    await desks.set_password("deus", "new-password")
                except DeskRefused as refused:
                    assert "keeps no password" in str(refused)
                else:
                    raise AssertionError("a RigDesk PC was given a password")
            finally:
                if link.remote is not None:
                    link.remote.cancel()
                    await asyncio.gather(link.remote, return_exceptions=True)

    asyncio.run(run())


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
            refused: list[str] = []

            async def then(_seat: SeatId, ask: str, choice: str, _by: str) -> str:
                if choice == "accept":
                    await desks.join_approved("front", "a1", "g", "n", "Builder", ask)
                else:
                    refused.append(ask)
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
                del desks._sessions["front"]
                prompts.start(desks, "front", "desk_3", seat, "a1", "g", "Builder")
                await _until(lambda: any(path == "/offer" and body["ask"] == "desk_3" for path, body in posted))
                nonce = next(body["nonce"] for path, body in posted if path == "/offer" and body["ask"] == "desk_3")
                await desks._event(link, {"type": "offer", "ask": "desk_3", "nonce": nonce, "answer": "deny"})
                await _until(lambda: refused == ["desk_3"])
                assert "desk_3" not in desks.person_answered, "a Deny at the PC was counted as the person's answer"
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


class _StockBridge:
    """A stock PC's rigdesk-bridge on loopback: its prompt routes, its login on the operator's Accept (which logs in once the person allowed, or
    with a good saved password), and mint and claim."""
    def __init__(self, password: str | None) -> None:
        self.password, self.allowed, self.posted, self.login_s = password, False, [], 0.0

    async def took(self, request) -> object:
        from aiohttp import web
        body = await request.json() if request.can_read_body else {}
        self.posted.append((request.path, body))
        if request.path == "/login":
            await asyncio.sleep(self.login_s)
            if self.allowed or self.password == "good":
                return web.json_response({"logged_in": True})
            if self.password is None:
                return web.json_response({"logged_in": False, "code": "no_password", "error": "Rig keeps no password for this PC"}, status=409)
            return web.json_response({"logged_in": False, "code": "login_failed", "error": "login refused by the PC: Wrong Password"}, status=409)
        if request.path == "/viewer/mint":
            return web.json_response({"grant": GRANT, "secret": SECRET})
        if request.path == "/claim":
            return web.json_response({"session": "s", "standing": {"holder": "agent"}})
        return web.json_response({"shown": True} if request.path == "/offer" else {"ended": True})

    def paths(self) -> list[str]:
        return [path for path, _ in self.posted]


async def _stock_desks(state: str, bridge: _StockBridge, asks: "_Asks", told: list) -> tuple[Desks, object]:
    from aiohttp import ClientSession, web
    from rigdesk.desks import RigdeskSettings, StockRemote
    app = web.Application()
    for path in ("/offer", "/offer/end", "/login", "/viewer/mint", "/claim"):
        app.router.add_post(path, bridge.took)
    runner = web.AppRunner(app)
    await runner.setup()
    site = web.TCPSite(runner, "127.0.0.1", 0)
    await site.start()
    token = Path(state) / "token"
    token.write_text("rig-bearer")
    stock = (StockRemote(peer="phone.tail1234.ts.net") if bridge.password is None else
             StockRemote(peer="phone.tail1234.ts.net", password_file=str(Path(state) / "phone.password")))
    spec = DeskPc(url=f"http://127.0.0.1:{runner.addresses[0][1]}", token_file=str(token), stock=stock)
    settings = RigdeskSettings(pcs={"phone": spec})
    roster = SimpleNamespace(identity=lambda seat: _now(SimpleNamespace(agent="a1", label="Builder")), closed=lambda seats: _now(set()))
    host = SimpleNamespace(state_dir=Path(state), docs=_Docs(), settings=lambda _shape: settings, report=lambda *_: None, forget=lambda *_: None,
                           confirming=asks.confirming, confirm_answered=asks.confirm_answered, roster=roster)
    desks = Desks(host)  # type: ignore[arg-type]
    host.live = lambda _kind: desks
    desks._pcs["phone"] = PcLink("phone", spec)
    desks._resync = lambda *_: None  # type: ignore[method-assign]
    desks._http = ClientSession()

    async def tell(held: Held, words: str, key: str, timing: str = "steer") -> bool:
        told.append((held.agent, words))
        return True
    desks._tell = tell  # type: ignore[method-assign]
    return desks, runner


async def _now(value: object) -> object:
    return value


def _accepted(ask: str) -> dict:
    return {"given": {"pc": "phone", "agent": "a1", "graph": "g", "kind": "join", "ask": ask}, "graph": "g", "node": "n", "choice": "accept"}


def test_rigs_accept_on_a_stock_pc_keeps_the_devices_prompt_up_until_a_login_succeeds_and_says_joined_only_then() -> None:
    from core.sdk.host import scoped
    from rigdesk import _request_confirmed, prompts
    seat = SeatId(GraphName("g"), NodeId("n"))

    async def accepted_then(password: str | None, device: str, while_logging_in: bool = False) -> tuple[str, _StockBridge, Desks, list]:
        """The operator accepts while the device's prompt is up; the person there then answers device ("allow", "deny" or "" for nothing), after
        the Accept's login or while it is under way."""
        told: list = []
        with tempfile.TemporaryDirectory() as state:
            asks, bridge = _Asks("desk_1"), _StockBridge(password)
            desks, runner = await _stock_desks(state, bridge, asks, told)
            every, prompts.OPEN_EVERY_S = prompts.OPEN_EVERY_S, 0.01
            try:
                prompts.start(desks, "phone", "desk_1", seat, "a1", "g", "Builder")
                await _until(lambda: "/offer" in bridge.paths())
                asks.open.discard("desk_1")
                if while_logging_in:
                    bridge.login_s = 0.3
                    accepting = scoped("accepting", _request_confirmed(desks.host, _accepted("desk_1")))  # type: ignore[arg-type]
                    await _until(lambda: "/login" in bridge.paths())
                    nonce = next(body["nonce"] for path, body in bridge.posted if path == "/offer")
                    await desks._event(desks._pcs["phone"], {"type": "offer", "ask": "desk_1", "nonce": nonce, "answer": device})
                    said = await accepting
                    device = ""
                else:
                    said = await _request_confirmed(desks.host, _accepted("desk_1"))  # type: ignore[arg-type]
                await asyncio.sleep(0.1)
                if device:
                    assert "/offer/end" not in bridge.paths() and "desk_1" in desks.prompts, "the device's prompt went away before any login succeeded"
                    assert desks.held_on("phone") is None, "the agent holds the PC before any login succeeded"
                    bridge.allowed = device == "allow"
                    nonce = next(body["nonce"] for path, body in bridge.posted if path == "/offer")
                    await desks._event(desks._pcs["phone"], {"type": "offer", "ask": "desk_1", "nonce": nonce, "answer": device})
                    await _until(lambda: told)
            finally:
                prompts.OPEN_EVERY_S = every
                await prompts.end_all(desks)
                await desks._http.close()
                await runner.cleanup()
            return said, bridge, desks, told

    async def run() -> None:
        said, bridge, desks, told = await accepted_then("good", "")
        assert said.startswith("You joined phone"), said
        assert bridge.paths()[1:] == ["/login", "/viewer/mint", "/claim", "/offer/end"], bridge.paths()
        assert desks.held_on("phone") is not None and told == [], told

        said, bridge, desks, told = await accepted_then("stale", "allow")
        assert "you have not joined phone: Rig's saved password for phone did not log in: login refused by the PC: Wrong Password" in said, said
        assert "must Allow your request on the device" in said and "joined phone" not in said.replace("not joined phone", ""), said
        assert bridge.paths().count("/login") == 2 and "/claim" in bridge.paths(), bridge.paths()
        assert len(told) == 1 and "allowed you on the device, so you joined phone over the connection they allowed, with no password" in told[0][1], told
        assert desks.held_on("phone") is not None

        said, bridge, desks, told = await accepted_then(None, "deny")
        assert "you have not joined phone: Rig keeps no password for phone" in said, said
        assert len(told) == 1 and "dismissed your request on the device, so you did not join phone" in told[0][1], told
        assert "/claim" not in bridge.paths() and desks.held_on("phone") is None

        said, bridge, desks, told = await accepted_then("good", "deny", while_logging_in=True)
        assert "you did not join phone: denied by the person at phone" in said and "before Rig's login succeeded" in said, said
        assert "/claim" not in bridge.paths() and desks.held_on("phone") is None and told == [], (bridge.paths(), told)

    asyncio.run(run())


def test_an_accept_with_no_login_and_no_prompt_on_the_device_says_why_and_joins_nothing() -> None:
    from rigdesk import _request_confirmed
    from core.sdk.model import Refused

    async def run() -> None:
        told: list = []
        with tempfile.TemporaryDirectory() as state:
            bridge = _StockBridge(None)
            desks, runner = await _stock_desks(state, bridge, _Asks(), told)
            try:
                desks._pcs["phone"].prompt = False
                assert desks.device_unasked("phone") == "its rigdesk-bridge predates showing requests on the device (/offer)"
                assert desks.pcs()[0]["device_unasked"] == desks.device_unasked("phone")
                desks.unshown["phone"] = str(desks.device_unasked("phone"))
                try:
                    await _request_confirmed(desks.host, _accepted("desk_1"))  # type: ignore[arg-type]
                except Refused as refused:
                    words = str(refused)
                else:
                    raise AssertionError("an Accept with no login and nobody asked on the device said it joined")
                desks._pcs["phone"].prompt = True
                assert desks.device_unasked("phone") is None
            finally:
                await desks._http.close()
                await runner.cleanup()
        assert "Rig keeps no password for phone, and the person at phone is not asked on the device: its rigdesk-bridge predates" in words, words
        assert "/claim" not in bridge.paths() and desks.held_on("phone") is None and told == []

    asyncio.run(run())


def test_a_stock_pcs_bridge_gets_rigs_pairing_key_in_its_environment_and_its_arguments_stay_as_they_were() -> None:
    from rigdesk.desks import PAIR_KEY, PAIR_KEY_ENV, SECRETS, StockRemote

    async def run() -> None:
        with tempfile.TemporaryDirectory() as state:
            desks = Desks(SimpleNamespace(state_dir=Path(state), report=lambda *_: None))  # type: ignore[arg-type]
            argv, env, bridge = Path(state) / "argv", Path(state) / "env", Path(state) / "bridge"
            bridge.write_text(f"#!/bin/sh\nprintf '%s\\n' \"${PAIR_KEY_ENV}\" > {env}\nprintf '%s\\n' \"$@\" > {argv}\nexec sleep 60\n")
            bridge.chmod(0o700)
            desks._bridge_bin = str(bridge)
            stock = StockRemote(peer="moto-g57.tail1234.ts.net")
            link = desks._pcs["moto-g57"] = PcLink("moto-g57", DeskPc(url="http://127.0.0.1:21120", token_file="/dev/null", stock=stock))
            link.remote = asyncio.get_running_loop().create_task(desks._remote(link, stock))
            try:
                await _until(lambda: argv.exists() and argv.read_text().strip())
                assert argv.read_text().split() == ["--peer", "moto-g57.tail1234.ts.net", "--token-file", "/dev/null", "--port", "21120",
                                                    "--journal", str(desks._dir / "bridge-moto-g57.journal")]
                assert env.read_text().strip() == str((SECRETS / PAIR_KEY).expanduser())
            finally:
                link.remote.cancel()
                await asyncio.gather(link.remote, return_exceptions=True)

    asyncio.run(run())


def test_a_message_whose_session_ended_while_it_was_told_is_not_kept_for_the_pcs_next_agent() -> None:
    async def run() -> None:
        with tempfile.TemporaryDirectory() as state:
            desks = Desks(SimpleNamespace(state_dir=Path(state)))  # type: ignore[arg-type]
            link = desks._pcs["deus"] = PcLink("deus", PCS["deus"], state="up")
            desks._sessions["deus"] = Held("deus", "agent_a", "g", "n", "A", "s", 0)

            async def message(*_args: object) -> bool:
                desks._sessions.pop("deus")
                return False
            desks._message = message  # type: ignore[method-assign]
            await desks._event(link, {"type": "message", "id": 7, "text": "for agent_a"})
            assert "deus" not in desks._unsent, "a message for the ended session would go to the PC's next agent"

    asyncio.run(run())


def test_an_invite_to_a_pc_where_an_agent_may_run_commands_waits_for_the_operator_and_one_elsewhere_joins_at_once() -> None:
    from aiohttp import ClientSession, web
    from rigdesk.desks import InviteAsk, RigdeskSettings
    claimed: list[str] = []

    async def health(_request: web.Request) -> web.Response:
        return web.json_response({"ok": True, "invite_open": True})

    async def claim(request: web.Request) -> web.Response:
        claimed.append(request.host)
        return web.json_response({"session": "s"})

    async def run() -> None:
        app = web.Application()
        app.router.add_get("/health", health)
        app.router.add_post("/claim", claim)
        runner = web.AppRunner(app)
        await runner.setup()
        site = web.TCPSite(runner, "127.0.0.1", 0)
        await site.start()
        port = runner.addresses[0][1]
        with tempfile.TemporaryDirectory() as state:
            token = Path(state) / "token"
            token.write_text("t" * 32)
            pcs = {name: DeskPc(url=f"http://127.0.0.1:{port}", token_file=str(token)) for name in ("front", "back")}
            settings = RigdeskSettings(pcs=pcs)
            desks = Desks(SimpleNamespace(state_dir=Path(state), docs=_Docs(), settings=lambda _shape: settings, report=lambda *_: None, forget=lambda *_: None))  # type: ignore[arg-type]
            desks._pcs.update({name: PcLink(name, spec) for name, spec in pcs.items()})
            desks._resync = lambda *_: None  # type: ignore[method-assign]
            desks._http = ClientSession()
            try:
                assert await desks.allow_shell("front", "agent:a9", "user")
                try:
                    await desks.join(f"rigdesk://front/{GRANT}#{SECRET}", "a1", "g", "n", "A")
                except InviteAsk as asked:
                    assert asked.pc == "front"
                else:
                    raise AssertionError("an invite to a PC with a shell grant was claimed unasked")
                assert claimed == [] and desks.held_by("a1", "g") is None
                assert (await desks.join(f"rigdesk://front/{GRANT}#{SECRET}", "a1", "g", "n", "A", approved=True)).pc == "front"
                assert (await desks.join(f"rigdesk://back/{GRANT}#{SECRET}", "a2", "g", "n", "B")).pc == "back"
                assert len(claimed) == 2
            finally:
                await desks._http.close()
                await runner.cleanup()

    asyncio.run(run())


def test_no_request_carrying_a_token_follows_a_redirect() -> None:
    import aiohttp
    from aiohttp import web

    reached: list[str] = []

    async def target(request: web.Request) -> web.Response:
        reached.append(request.path)
        return web.json_response({})

    async def run() -> None:
        far = web.Application()
        far.router.add_route("*", "/{tail:.*}", target)
        far_runner = web.AppRunner(far)
        await far_runner.setup()
        await web.TCPSite(far_runner, "127.0.0.1", 0).start()
        elsewhere = f"http://127.0.0.1:{far_runner.addresses[0][1]}/stolen"

        async def redirecting(request: web.Request) -> web.Response:
            raise web.HTTPTemporaryRedirect(elsewhere) if request.method == "POST" else web.HTTPFound(elsewhere)

        near = web.Application()
        near.router.add_route("*", "/{tail:.*}", redirecting)
        near_runner = web.AppRunner(near)
        await near_runner.setup()
        await web.TCPSite(near_runner, "127.0.0.1", 0).start()
        with tempfile.TemporaryDirectory() as state:
            token = Path(state) / "token"
            token.write_text("t" * 32)
            desks = Desks(SimpleNamespace(state_dir=Path(state)))  # type: ignore[arg-type]
            url = f"http://127.0.0.1:{near_runner.addresses[0][1]}"
            link = PcLink("deus", DeskPc(url=url, token_file=str(token)))
            desks._http = token_session()
            try:
                for method, path in (("GET", "/health"), ("POST", "/mcp")):
                    for caught in (DeskRefused, aiohttp.ClientError):
                        try:
                            await desks.http.request(method, url + path, headers=desks._headers(link, "session-secret"))
                        except caught as refused:
                            assert isinstance(refused, RedirectRefused) and "answered with a redirect" in str(refused), refused
                        else:
                            raise AssertionError(f"{method} {path} followed a redirect")
                assert reached == [], reached
            finally:
                await desks._http.close()
                await near_runner.cleanup()
                await far_runner.cleanup()

    asyncio.run(run())


def test_a_real_server_gives_the_operator_rigs_pairing_key_and_refuses_a_missing_or_wrong_bearer() -> None:
    import base64
    import subprocess
    import sys
    import time
    import urllib.error
    import urllib.request

    import core
    checkout = Path(core.__file__).resolve().parent.parent
    sys.path.insert(0, str(checkout / "scripts"))
    from fixture_home import free_port, isolated_env

    key, operator = base64.b64encode(bytes(range(32))).decode(), "operator-bearer-for-this-test"
    with tempfile.TemporaryDirectory() as held:
        root, port = Path(held) / "rig", free_port()
        env = isolated_env(root, port, "rigdesk-pair-key-test-absent.service") | {"RIG_ACCESS_TOKEN": operator}
        bridge = Path(held) / "bridge"
        bridge.write_text(f'#!/bin/sh\n[ "$1" = --pair-key ] && [ -n "$2" ] && echo {key}\n')
        bridge.chmod(0o755)
        (root / "home" / "settings.json").write_text(json.dumps({"plugins": {"rigdesk": {"bridge_bin": str(bridge)}}}))
        rig = [sys.executable, str(checkout / "rig")]
        with (root / "server.log").open("wb") as log:
            server = subprocess.Popen([*rig, "serve", "--port", str(port)], cwd=checkout, env=env, stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT)

        def asked(path: str, bearer: str | None) -> tuple[int, str]:
            headers = {} if bearer is None else {"Authorization": f"Bearer {bearer}"}
            try:
                with urllib.request.urlopen(urllib.request.Request(f"http://127.0.0.1:{port}{path}", headers=headers), timeout=20) as answer:
                    return answer.status, answer.read().decode()
            except urllib.error.HTTPError as refused:
                return refused.code, refused.read().decode()

        try:
            # `rig serve` on a spare port queues for a dev slot for up to 600 s, and both slots being busy is normal.
            deadline = time.monotonic() + 660
            while True:
                try:
                    asked("/api/contributions/status", operator)
                    break
                except OSError:
                    assert server.poll() is None and time.monotonic() < deadline, (root / "server.log").read_text()[-2000:]
                    time.sleep(0.2)
            added = subprocess.run([*rig, "plugin", "add", str(Path(__file__).resolve().parent.parent / "rigdesk")], cwd=checkout, capture_output=True, text=True, env=env, timeout=60)
            assert added.returncode == 0, added.stdout + added.stderr
            route = "/api/contributions/rigdesk/routes/pair_key"
            status, body = asked(route, operator)
            assert status == 200, (status, body)
            assert json.loads(body)["key"] == key and len(base64.b64decode(json.loads(body)["key"], validate=True)) == 32, body
            for bearer in (None, "not-the-operators-bearer"):
                status, body = asked(route, bearer)
                assert status in (401, 403) and key not in body, (bearer, status, body)
        finally:
            server.terminate()
            server.wait(20)


def test_every_access_grant_rigdesk_asks_for_is_the_operators_alone() -> None:
    """Join, Always allow, an invite where an agent may run commands, and desk/connect each give an agent a PC: no agent answers them for him."""
    from rigdesk import RequestInput, _ask_invite, _request

    asked: list = []

    async def confirm(*given: object) -> str:
        asked.append(given)
        return "resolved"

    async def caller_title() -> str:
        return "Builder"

    async def switch(_pc: str) -> dict:
        return {"open": True}

    entries = [{"name": "phone", "stock": {"peer": "p"}, "held_by": None}, {"name": "deus", "stock": None, "held_by": None}]
    desks = SimpleNamespace(host=SimpleNamespace(confirm=confirm), entries=lambda: entries, join_switch=switch, assigned_pcs=lambda *_: (),
                            held_by=lambda *_: None, device_unasked=lambda _pc: "it is unshown here", unshown={})
    call = SimpleNamespace(contract=SimpleNamespace(place=SimpleNamespace(agent="a1", seat=SeatId(GraphName("g"), NodeId("n")))), graph="g", node="n",
                           kernel=SimpleNamespace(caller_title=caller_title, activation_id="act-1"), refused=lambda code, words, *_: {"refused": code, "said": words})

    async def run() -> None:
        for pc, kind in (("phone", "join"), ("phone", "assign"), ("deus", "join"), ("deus", "assign")):
            answer = await _request(desks, call, RequestInput(pc=pc, why="to fix the printer", kind=kind))  # type: ignore[arg-type]
            assert "refused" not in answer, (pc, kind, answer)
        await _ask_invite(desks, call, "deus", "rigdesk://deus/x#y")  # type: ignore[arg-type]
        thens = [given[4] for given in asked]
        assert len(thens) == 5 and all(then.operator_only for then in thens), thens

    asyncio.run(run())


if __name__ == "__main__":
    for name, test in list(globals().items()):
        if name.startswith("test_"):
            test()
            print("ok", name)
