import pytest
from tests import *

@pytest.mark.describe('YAML node')
class TestYamlMode:
    @pytest.mark.asyncio
    @pytest.mark.it('should be loaded')
    async def test_should_be_loaded(self):
        pass

    @pytest.mark.asyncio
    @pytest.mark.it('should convert a valid yaml string to a javascript object')
    async def test_should_convert_a_valid_yaml_string_to_a_javascript_object(self):
        flows = [
            {"id": "100", "type": "tab"},
            {"id": "101", "z": "100", "type": "yaml", "func": "return msg;", "wires": [["102"]]},
            {"id": "102", "z": "100", "type": "test-once"}
        ]
        yaml_string = "employees:\n  - firstName: John\n    lastName: Smith\n"
        injections = [
            {"nid": "101", "msg": { "payload": yaml_string, "topic": "bar"}}
        ]
        msgs = await run_flow_with_msgs_ntimes(flows, injections, 1)
        assert len(msgs) == 1
        msg = msgs[0]
        assert "topic" in msg
        assert msg["topic"] == "bar"
        assert "payload" in msg
        assert "employees" in msg["payload"]
        e1 = msg["payload"]["employees"][0]
        assert e1["firstName"] == "John"
        assert e1["lastName"] == "Smith"

    @pytest.mark.asyncio
    @pytest.mark.it('should convert a valid yaml string to a javascript object - using another property')
    async def test_should_convert_a_valid_yaml_string_to_a_javascript_object_using_another_property(self):
        flows = [
            {"id": "100", "type": "tab"},
            {"id": "101", "z": "100", "type": "yaml", "property": "foo", "func": "return msg;", "wires": [["102"]]},
            {"id": "102", "z": "100", "type": "test-once"}
        ]
        yaml_string = "employees:\n  - firstName: John\n    lastName: Smith\n"
        injections = [
            {"nid": "101", "msg": { "foo": yaml_string, "topic": "bar"}}
        ]
        msgs = await run_flow_with_msgs_ntimes(flows, injections, 1)
        assert len(msgs) == 1
        msg = msgs[0]
        assert "topic" in msg
        assert msg["topic"] == "bar"
        assert "foo" in msg
        assert "employees" in msg["foo"]
        e1 = msg["foo"]["employees"][0]
        assert e1["firstName"] == "John"
        assert e1["lastName"] == "Smith"

    @pytest.mark.asyncio
    @pytest.mark.it('should convert a javascript object to a yaml string')
    async def test_should_convert_a_javascript_object_to_a_yaml_string(self):
        flows = [
            {"id": "100", "type": "tab"},
            {"id": "101", "z": "100", "type": "yaml", "func": "return msg;", "wires": [["102"]]},
            {"id": "102", "z": "100", "type": "test-once"}
        ]
        obj = {"employees":[{"firstName":"John", "lastName":"Smith"}]}
        injections = [
            {"nid": "101", "msg": { "payload": obj } }
        ]
        msgs = await run_flow_with_msgs_ntimes(flows, injections, 1)
        assert len(msgs) == 1
        msg = msgs[0]
        print(msgs)
        print(msg)
        "employees:\n- firstName: John\n  lastName: Smith\n"
        assert msg["payload"] == "employees:\n  - firstName: John\n    lastName: Smith\n"

    @pytest.mark.asyncio
    @pytest.mark.it('should convert a javascript object to a yaml string - using another property')
    async def test_dump_another_property(self):
        flows = [
            {"id": "100", "type": "tab"},
            {"id": "101", "z": "100", "type": "yaml", "property": "foo", "wires": [["102"]]},
            {"id": "102", "z": "100", "type": "test-once"},
        ]
        obj = {"employees": [{"firstName": "John", "lastName": "Smith"}]}
        msgs = await run_flow_with_msgs_ntimes(flows, [{"nid": "101", "msg": {"foo": obj}}], 1)
        assert msgs[0]["foo"] == "employees:\n  - firstName: John\n    lastName: Smith\n"

    @pytest.mark.asyncio
    @pytest.mark.it('should convert an array to a yaml string')
    async def test_dump_array(self):
        flows = [
            {"id": "100", "type": "tab"},
            {"id": "101", "z": "100", "type": "yaml", "wires": [["102"]]},
            {"id": "102", "z": "100", "type": "test-once"},
        ]
        msgs = await run_flow_with_msgs_ntimes(flows, [{"nid": "101", "msg": {"payload": [1, 2, 3]}}], 1)
        assert msgs[0]["payload"] == "- 1\n- 2\n- 3\n"

    @pytest.mark.asyncio
    @pytest.mark.it('should pass straight through if no payload set')
    async def test_pass_through(self):
        flows = [
            {"id": "100", "type": "tab"},
            {"id": "101", "z": "100", "type": "yaml", "wires": [["102"]]},
            {"id": "102", "z": "100", "type": "test-once"},
        ]
        msgs = await run_flow_with_msgs_ntimes(flows, [{"nid": "101", "msg": {"topic": "bar"}}], 1)
        assert msgs[0]["topic"] == "bar"
        assert "payload" not in msgs[0]

    @pytest.mark.asyncio
    @pytest.mark.it('should log an error if asked to parse an invalid yaml string')
    async def test_invalid_yaml(self):
        # The spec reads helper.log() for js-yaml's "end of the stream" text. The harness has
        # no log channel, so the catch node is what shows the parse was rejected.
        flows = [
            {"id": "100", "type": "tab"},
            {"id": "101", "z": "100", "type": "yaml", "wires": [[]]},
            {"id": "103", "z": "100", "type": "catch", "scope": ["101"], "uncaught": False, "wires": [["102"]]},
            {"id": "102", "z": "100", "type": "test-once"},
        ]
        bad = "employees:\n-firstName: John\n- lastName: Smith\n"
        msgs = await run_flow_with_msgs_ntimes(
            flows, [{"nid": "101", "msg": {"payload": bad, "topic": "bar"}}], 1)
        assert msgs[0]["topic"] == "bar"
        assert msgs[0]["payload"] == bad
        assert "message" in msgs[0]["error"]
        assert msgs[0]["error"]["message"]

    @pytest.mark.asyncio
    @pytest.mark.it('should log an error if asked to parse something thats not yaml or js')
    async def test_not_yaml_or_js(self):
        # helper.log() is not readable here. Booleans and numbers keep their value and the
        # catch node reports yaml.errors.dropped. A Buffer cannot cross the pytest bridge;
        # the bytes case is the `scalars_and_buffers_are_not_dumped` unit test.
        flows = [
            {"id": "100", "type": "tab"},
            {"id": "101", "z": "100", "type": "yaml", "wires": [["102"]]},
            {"id": "103", "z": "100", "type": "catch", "scope": ["101"], "uncaught": False, "wires": [["102"]]},
            {"id": "102", "z": "100", "type": "test-once"},
        ]
        msgs = await run_flow_with_msgs_ntimes(
            flows,
            [{"nid": "101", "msg": {"payload": True}}, {"nid": "101", "msg": {"payload": 1}}],
            4,
        )
        outputs = [m for m in msgs if "error" not in m]
        errors = [m for m in msgs if "error" in m]
        assert sorted((type(m["payload"]).__name__, m["payload"]) for m in outputs) == [("bool", True), ("int", 1)]
        assert len(errors) == 2
        assert all(m["error"]["message"] == "Ignored unsupported payload type" for m in errors)

