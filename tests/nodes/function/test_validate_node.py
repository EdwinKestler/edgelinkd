import pytest

from tests import *


def _validate(**fields):
    node = {"type": "validate", "name": "check", "property": "payload", "check": "type", "expect": "number"}
    node.update(fields)
    return node


@pytest.mark.describe("validate node")
class TestValidateNode:
    @pytest.mark.asyncio
    @pytest.mark.it("forwards a payload inside the range")
    async def test_forwards_a_value_inside_the_range(self):
        msgs = await run_single_node_with_msgs_ntimes(
            _validate(check="range", min=0, max=10),
            [{"payload": 4}],
            1,
        )
        assert msgs[0]["payload"] == 4

    @pytest.mark.asyncio
    @pytest.mark.it("a value of the wrong type is a node error")
    async def test_wrong_type_is_a_node_error(self):
        flows = [
            {"id": "100", "type": "tab"},
            {
                "id": "1",
                "z": "100",
                "type": "validate",
                "property": "payload",
                "check": "type",
                "expect": "number",
                "wires": [[]],
            },
            {"id": "3", "z": "100", "type": "catch", "scope": ["1"], "uncaught": False, "wires": [["2"]]},
            {"id": "2", "z": "100", "type": "test-once"},
        ]
        msgs = await run_flow_with_msgs_ntimes(flows, [{"payload": "hot"}], 1, "1")
        assert "string" in msgs[0]["error"]["message"]
        assert "number" in msgs[0]["error"]["message"]

    @pytest.mark.asyncio
    @pytest.mark.it("a stale timestamp is a node error")
    async def test_stale_timestamp_is_a_node_error(self):
        flows = [
            {"id": "100", "type": "tab"},
            {
                "id": "1",
                "z": "100",
                "type": "validate",
                "property": "payload",
                "check": "age",
                "maxAgeMs": 1000,
                "wires": [[]],
            },
            {"id": "3", "z": "100", "type": "catch", "scope": ["1"], "uncaught": False, "wires": [["2"]]},
            {"id": "2", "z": "100", "type": "test-once"},
        ]
        msgs = await run_flow_with_msgs_ntimes(flows, [{"payload": 1000}], 1, "1")
        assert "older" in msgs[0]["error"]["message"]

    @pytest.mark.asyncio
    @pytest.mark.it("an unknown check is rejected at deploy")
    async def test_unknown_check_is_rejected(self):
        with pytest.raises(Exception) as exc:
            await run_single_node_with_msgs_ntimes(_validate(check="schema"), [{"payload": 1}], 1)
        message = str(exc.value).lower()
        assert "not supported" in message
        assert "schema" in message
