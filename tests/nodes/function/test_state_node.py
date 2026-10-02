import pytest

from tests import *


def _machine(states, initial="idle", period=0):
    return {
        "type": "state",
        "name": "machine",
        "initial": initial,
        "period": period,
        "states": states,
    }


def _sequence():
    return [
        {
            "name": "idle",
            "on": [
                {"when": {"type": "msg", "property": "payload", "op": "eq", "value": "start"}, "to": "run"},
                {"when": {"type": "flow", "property": "go", "op": "eq", "value": True}, "to": "run"},
            ],
        },
        {
            "name": "run",
            "on": [
                {"when": {"type": "msg", "property": "payload", "op": "eq", "value": "trip"}, "to": "fault"},
            ],
        },
        {
            "name": "fault",
            "on": [
                {"when": {"type": "msg", "property": "payload", "op": "eq", "value": "reset"}, "to": "idle"},
            ],
        },
    ]


@pytest.mark.describe("state node")
class TestStateNode:
    @pytest.mark.asyncio
    @pytest.mark.it("walks idle, run, and fault from message compares")
    async def test_three_state_sequence(self):
        node = _machine(_sequence())
        msgs = await run_single_node_with_msgs_ntimes(
            node,
            [{"payload": "start"}, {"payload": "trip"}, {"payload": "reset"}],
            3,
        )
        assert [msg["state"] for msg in msgs] == ["run", "fault", "idle"]
        assert [msg["payload"] for msg in msgs] == ["start", "trip", "reset"]

    @pytest.mark.asyncio
    @pytest.mark.it("a tick does not advance when no edge matches")
    async def test_tick_does_not_advance(self):
        node = _machine(_sequence())
        msgs = await run_single_node_with_msgs_ntimes(
            node,
            [{"tick": True}, {"payload": "trip"}, {"payload": "start"}],
            1,
        )
        assert msgs[0]["state"] == "run"
        assert msgs[0]["payload"] == "start"

    @pytest.mark.asyncio
    @pytest.mark.it("a tick enters the next state when the context edge matches")
    async def test_tick_follows_a_context_edge(self):
        states = [
            {
                "name": "idle",
                "on": [
                    {"when": {"type": "msg", "property": "payload", "op": "eq", "value": "arm"}, "to": "armed"},
                ],
            },
            {
                "name": "armed",
                "entry": [{"scope": "flow", "key": "go", "value": True}],
                "on": [
                    {"when": {"type": "flow", "property": "go", "op": "eq", "value": True}, "to": "run"},
                ],
            },
            {"name": "run", "on": []},
        ]
        node = _machine(states)
        msgs = await run_single_node_with_msgs_ntimes(
            node,
            [{"payload": "arm"}, {"tick": True}],
            2,
        )
        assert [msg["state"] for msg in msgs] == ["armed", "run"]

    @pytest.mark.asyncio
    @pytest.mark.it("an unknown condition type is rejected at deploy")
    async def test_unknown_condition_is_rejected(self):
        states = _sequence()
        states[0]["on"][0]["when"]["type"] = "jsonata"
        node = _machine(states)
        with pytest.raises(Exception) as exc:
            await run_single_node_with_msgs_ntimes(node, [{"payload": "start"}], 1)
        message = str(exc.value).lower()
        assert "not supported" in message
        assert "jsonata" in message
