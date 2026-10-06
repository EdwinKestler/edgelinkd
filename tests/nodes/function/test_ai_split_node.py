import pytest

from tests import *


@pytest.mark.describe("ai-split node")
class TestAiSplitNode:
    @pytest.mark.asyncio
    async def test_splits_overlap_zero_window(self):
        flows = [
            {"id": "100", "type": "tab"},
            {"id": "1", "z": "100", "type": "ai-split", "chunkSize": 4, "overlap": 0, "wires": [["2"]]},
            {"id": "2", "z": "100", "type": "test-once"},
        ]
        msgs = await run_flow_with_msgs_ntimes(flows, [{"payload": "abcdef"}], 2)
        payloads = sorted(m["payload"] for m in msgs)
        assert payloads == ["abcd", "ef"]
        assert msgs[0]["parts"]["id"] == msgs[1]["parts"]["id"]
