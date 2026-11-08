"""Run from a Rig checkout: PYTHONPATH=.:<rigdesk>/rig-plugin .venv/bin/python <rigdesk>/rig-plugin/tests/test_rigdesk.py (or with pytest)."""
from rigdesk.desks import DeskPc, pc_of_link, words_of

PCS = {"deus": DeskPc(url="https://deus.tail1234.ts.net", token_file="/dev/null", client="acme")}
GRANT, SECRET = "a" * 32, "b" * 32


def test_a_link_names_its_pc_by_name_or_tailnet_host() -> None:
    assert pc_of_link(f"rigdesk://deus/{GRANT}#{SECRET}", PCS) == ("deus", GRANT, SECRET)
    assert pc_of_link(f"rigdesk://deus.tail1234.ts.net/{GRANT}#{SECRET}", PCS) == ("deus", GRANT, SECRET)
    assert "no RigDesk PC named other" in str(pc_of_link(f"rigdesk://other/{GRANT}#{SECRET}", PCS))
    assert "not a RigDesk share link" in str(pc_of_link("https://example.com", PCS))


def test_the_agent_hears_of_takeover_and_block_but_not_of_its_own_join_or_leave() -> None:
    assert "took input back" in str(words_of("deus", {"type": "holder", "holder": "human", "by": "viewer input", "at_ms": 0}))
    assert "blocked you" in str(words_of("deus", {"type": "holder", "holder": "blocked", "by": "block", "at_ms": 0}))
    assert words_of("deus", {"type": "holder", "holder": "agent", "by": "share", "at_ms": 0}) is None
    assert "pressed Stop" in str(words_of("deus", {"type": "holder", "holder": "human", "by": "stop", "at_ms": 0}))
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


if __name__ == "__main__":
    for name, test in list(globals().items()):
        if name.startswith("test_"):
            test()
            print("ok", name)
