import os
import uuid

import pytest

from tests import *

# Upstream keeps these off unless NR_MQTT_TESTS is true and a broker is on localhost:1883.
# This host publishes MQTT on 127.0.0.1:1883, so the flows use that address.
_LIVE = os.environ.get("NR_MQTT_TESTS") in ("true", "1")
_LIVE_REASON = (
    'skipping MQTT tests. Set env var "NR_MQTT_TESTS=true" to enable. '
    "Requires a v5 capable broker running on localhost:1883."
)
_JS_OBJECT = "pytest cannot read the Node-RED node object (options, wires, client id)"
_BINARY_IN = "binary payloads cannot cross the pytest bridge"
_WILL = (
    "a forced TCP close that makes the broker publish the will is not available; "
    "disconnect sends a clean MQTT DISCONNECT"
)
_BAD_BIRTH = "a wildcard birth topic is rejected when the flow is deployed"

_TOPIC = 0


def next_topic():
    global _TOPIC
    _TOPIC += 1
    return f"edgelinkd/spec/{_TOPIC}/{uuid.uuid4().hex[:8]}"


def _live(test):
    marks = [
        pytest.mark.asyncio,
        pytest.mark.timeout(20),
        pytest.mark.skipif(not _LIVE, reason=_LIVE_REASON),
    ]
    for mark in marks:
        test = mark(test)
    return test


def assert_mqtt(msg, expect):
    assert msg.get("topic") == expect["topic"], msg
    assert msg.get("payload") == expect["payload"], msg
    if "retain" in expect:
        assert msg.get("retain") is expect["retain"], msg
    assert msg.get("qos") == expect.get("qos", 0), msg
    for key in (
        "userProperties",
        "contentType",
        "correlationData",
        "responseTopic",
        "payloadFormatIndicator",
        "messageExpiryInterval",
    ):
        if key in expect:
            assert msg.get(key) == expect[key], msg


def first_on_topic(msgs, topic):
    found = [msg for msg in msgs if msg.get("topic") == topic]
    assert found, msgs
    return found[0]


def _broker_auth(node):
    """Live runs sign in when EDGELINK_MQTT_USER is set. CI leaves the node anonymous."""
    user = os.environ.get("EDGELINK_MQTT_USER", "")
    if user and "username" not in node:
        node["username"] = user
        node["password"] = os.environ.get("EDGELINK_MQTT_PASSWORD", "")
    return node


def flow_for(broker_options, in_options, out_options, *, status_scope=None, extra_brokers=None):
    broker_id = red_id(broker_options.get("id", "mqtt.broker"))
    in_id = red_id((in_options or {}).get("id", "mqtt.in"))
    out_id = red_id((out_options or {}).get("id", "mqtt.out"))
    tab_id = red_id("tab")
    helper_id = red_id("helper.node")
    broker = {
        "id": broker_id,
        "type": "mqtt-broker",
        "name": "mqtt_broker",
        "broker": "127.0.0.1",
        "port": 1883,
        "autoConnect": broker_options.get("autoConnect", True),
        "cleansession": broker_options.get("cleansession", True),
    }
    for key, value in broker_options.items():
        if key != "id":
            broker[key] = value
    _broker_auth(broker)
    in_options = in_options or {}
    out_options = out_options or {}
    mqtt_in = {
        "id": in_id,
        "z": tab_id,
        "type": "mqtt in",
        "name": "mqtt_in",
        "broker": red_id(in_options.get("broker", broker_options.get("id", "mqtt.broker"))),
        "topic": in_options.get("topic", ""),
        "datatype": in_options.get("datatype", "utf8"),
        "qos": in_options.get("qos", 2),
        "inputs": 1 if in_options.get("topicType") == "dynamic" else 0,
        "wires": [[helper_id]],
    }
    for key in ("nl", "rap", "rh"):
        if key in in_options:
            mqtt_in[key] = in_options[key]
    mqtt_out = {
        "id": out_id,
        "z": tab_id,
        "type": "mqtt out",
        "name": "mqtt_out",
        "broker": red_id(out_options.get("broker", broker_options.get("id", "mqtt.broker"))),
        "topic": out_options.get("topic", ""),
        "wires": [],
    }
    catch = {
        "id": red_id("catch.node"),
        "z": tab_id,
        "type": "catch",
        "scope": [in_id],
        "wires": [[helper_id]],
    }
    flows = [
        {"id": tab_id, "type": "tab"},
        broker,
        mqtt_in,
        mqtt_out,
        {"id": helper_id, "z": tab_id, "type": "test-once"},
        catch,
    ]
    for extra in extra_brokers or []:
        extra_id = red_id(extra["id"])
        node = {
            "id": extra_id,
            "type": "mqtt-broker",
            "broker": "127.0.0.1",
            "port": 1883,
            "autoConnect": True,
            "cleansession": True,
        }
        node.update({key: value for key, value in extra.items() if key != "id"})
        _broker_auth(node)
        flows.append(node)
    if status_scope is not None:
        flows.append({
            "id": red_id("status.node"),
            "z": tab_id,
            "type": "status",
            "scope": [red_id(node_id) for node_id in status_scope],
            "wires": [[helper_id]],
        })
    return flows, {"in": in_id, "out": out_id, "broker": broker_id}


async def send_recv(broker_options, in_options, out_options, send_msg, *, delay_ms=800, seconds=2.0, extra=None, status_scope=None, extra_brokers=None):
    flows, ids = flow_for(
        broker_options, in_options, out_options, status_scope=status_scope, extra_brokers=extra_brokers
    )
    injections = [{"nid": ids["out"], "msg": send_msg, "delay_ms": delay_ms}]
    if extra:
        injections.extend(extra)
    msgs = await run_flow_for_seconds_scheduled(flows, injections, seconds)
    return msgs, ids


@pytest.mark.describe("MQTT Nodes")
class TestMqttNodes:
    @pytest.mark.skip(reason=_JS_OBJECT)
    @pytest.mark.it("should be loaded and have default values (MQTT V4)")
    def test_0001(self):
        pass

    @pytest.mark.skip(reason=_JS_OBJECT)
    @pytest.mark.it("should be loaded and have default values (MQTT V5)")
    def test_0002(self):
        pass

    @pytest.mark.it(
        'skipping MQTT tests. Set env var "NR_MQTT_TESTS=true" to enable. Requires a v5 capable broker running on localhost:1883.'
    )
    def test_0003(self):
        pass

    @_live
    @pytest.mark.it("basic send and receive tests")
    async def test_0004(self):
        topic = next_topic()
        send = {"topic": topic, "payload": "hello", "qos": 0}
        msgs, _ids = await send_recv({}, {"datatype": "auto", "topic": topic}, {}, send)
        assert_mqtt(first_on_topic(msgs, topic), send)

    @_live
    @pytest.mark.it("should send JSON and receive string (auto mode)")
    async def test_0005(self):
        topic = next_topic()
        send = {"topic": topic, "payload": '{"prop":"value1", "num":1}', "qos": 1}
        msgs, _ids = await send_recv({}, {"datatype": "auto", "topic": topic}, {}, send)
        assert_mqtt(first_on_topic(msgs, topic), send)

    @_live
    @pytest.mark.it("should send JSON and receive object (auto-detect mode)")
    async def test_0006(self):
        topic = next_topic()
        send = {"topic": topic, "payload": '{"prop":"value1", "num":1}', "qos": 1}
        expect = dict(send, payload={"prop": "value1", "num": 1})
        msgs, _ids = await send_recv({}, {"datatype": "auto-detect", "topic": topic}, {}, send)
        assert_mqtt(first_on_topic(msgs, topic), expect)

    @_live
    @pytest.mark.it("should send invalid JSON and receive string (auto mode)")
    async def test_0007(self):
        topic = next_topic()
        send = {"topic": topic, "payload": '{prop:"value3", "num":3}'}
        msgs, _ids = await send_recv({}, {"datatype": "auto", "topic": topic}, {}, send)
        assert_mqtt(first_on_topic(msgs, topic), send)

    @_live
    @pytest.mark.it("should send invalid JSON and receive string (auto-detect mode)")
    async def test_0008(self):
        topic = next_topic()
        send = {"topic": topic, "payload": '{prop:"value3", "num":3}'}
        msgs, _ids = await send_recv({}, {"datatype": "auto-detect", "topic": topic}, {}, send)
        assert_mqtt(first_on_topic(msgs, topic), send)

    @_live
    @pytest.mark.it("should send JSON and receive string (utf8 mode)")
    async def test_0009(self):
        topic = next_topic()
        send = {"topic": topic, "payload": '{"prop":"value2", "num":2}', "qos": 2}
        msgs, _ids = await send_recv({}, {"datatype": "utf8", "topic": topic}, {}, send)
        assert_mqtt(first_on_topic(msgs, topic), send)

    @_live
    @pytest.mark.it("should send JSON and receive Object (json mode)")
    async def test_0010(self):
        topic = next_topic()
        send = {"topic": topic, "payload": '{"prop":"value3", "num":3}'}
        expect = dict(send, payload={"prop": "value3", "num": 3})
        msgs, _ids = await send_recv({}, {"datatype": "json", "topic": topic}, {}, send)
        assert_mqtt(first_on_topic(msgs, topic), expect)

    @_live
    @pytest.mark.it("should send invalid JSON and raise error (json mode)")
    async def test_0011(self):
        topic = next_topic()
        send = {"topic": topic, "payload": '{prop:"value3", "num":3}'}
        msgs, ids = await send_recv({}, {"datatype": "json", "topic": topic}, {}, send)
        assert msgs, "catch produced no message"
        assert msgs[0]["error"]["source"]["id"] == ids["in"]

    @_live
    @pytest.mark.it("should send String and receive Buffer (buffer mode)")
    async def test_0012(self):
        topic = next_topic()
        send = {"topic": topic, "payload": "a b c"}
        msgs, _ids = await send_recv({}, {"datatype": "buffer", "topic": topic}, {}, send)
        expect = dict(send, payload=[ord(ch) for ch in send["payload"]])
        assert_mqtt(first_on_topic(msgs, topic), expect)

    @pytest.mark.skip(reason=_BINARY_IN)
    @pytest.mark.it("should send utf8 Buffer and receive String (auto mode)")
    def test_0013(self):
        pass

    @pytest.mark.skip(reason=_BINARY_IN)
    @pytest.mark.it("should send non utf8 Buffer and receive Buffer (auto mode)")
    def test_0014(self):
        pass

    @_live
    @pytest.mark.it("should send/receive all v5 flags and settings")
    async def test_0015(self):
        topic = next_topic()
        # The spec publishes a Buffer whose bytes are this UTF-8 string, plus correlation bytes 1, 2, 3.
        send = {
            "topic": topic + "/command",
            "payload": '{"version":"v5"}',
            "qos": 1,
            "retain": True,
            "responseTopic": topic + "/response",
            "userProperties": {"prop1": "val1"},
            "contentType": "text/plain",
            "correlationData": "\x01\x02\x03",
            "messageExpiryInterval": 2000,
        }
        expect = dict(send, correlationData=[1, 2, 3])
        msgs, _ids = await send_recv(
            {"protocolVersion": 5},
            {"datatype": "auto", "topic": send["topic"], "qos": 1, "nl": False, "rap": True, "rh": 1},
            {},
            send,
        )
        assert_mqtt(first_on_topic(msgs, send["topic"]), expect)

    @_live
    @pytest.mark.it('should send regular string with v5 media type "text/plain" and receive a string (auto mode)')
    async def test_0016(self):
        topic = next_topic()
        send = {"topic": topic, "payload": "abc", "contentType": "text/plain"}
        msgs, _ids = await send_recv({"protocolVersion": 5}, {"datatype": "auto", "topic": topic}, {}, send)
        assert_mqtt(first_on_topic(msgs, topic), send)

    @_live
    @pytest.mark.it('should send JSON with v5 media type "text/plain" and receive a string (auto mode)')
    async def test_0017(self):
        topic = next_topic()
        send = {"topic": topic, "payload": '{"prop":"val"}', "contentType": "text/plain"}
        msgs, _ids = await send_recv({"protocolVersion": 5}, {"datatype": "auto", "topic": topic}, {}, send)
        assert_mqtt(first_on_topic(msgs, topic), send)

    @_live
    @pytest.mark.it('should send JSON with v5 media type "text/plain" and receive a string (auto-detect mode)')
    async def test_0018(self):
        topic = next_topic()
        send = {"topic": topic, "payload": '{"prop":"val"}', "contentType": "text/plain"}
        msgs, _ids = await send_recv({"protocolVersion": 5}, {"datatype": "auto-detect", "topic": topic}, {}, send)
        assert_mqtt(first_on_topic(msgs, topic), send)

    @_live
    @pytest.mark.it('should send JSON with v5 media type "application/json" and receive an object (auto-detect mode)')
    async def test_0019(self):
        topic = next_topic()
        send = {"topic": topic, "payload": '{"prop":"val"}', "contentType": "application/json"}
        expect = dict(send, payload={"prop": "val"})
        msgs, _ids = await send_recv({"protocolVersion": 5}, {"datatype": "auto-detect", "topic": topic}, {}, send)
        assert_mqtt(first_on_topic(msgs, topic), expect)

    @_live
    @pytest.mark.it('should send invalid JSON with v5 media type "application/json" and raise an error (auto mode)')
    async def test_0020(self):
        topic = next_topic()
        send = {"topic": topic, "payload": '{prop:"value3", "num":3}', "contentType": "application/json"}
        msgs, ids = await send_recv({"protocolVersion": 5}, {"datatype": "auto", "topic": topic}, {}, send)
        assert msgs, "catch produced no message"
        assert msgs[0]["error"]["source"]["id"] == ids["in"]

    @pytest.mark.skip(reason=_BINARY_IN)
    @pytest.mark.it('should send buffer with v5 media type "application/json" and receive an object (auto-detect mode)')
    def test_0021(self):
        pass

    @pytest.mark.skip(reason=_BINARY_IN)
    @pytest.mark.it('should send buffer with v5 media type "text/plain" and receive a string (auto mode)')
    def test_0022(self):
        pass

    @pytest.mark.skip(reason=_BINARY_IN)
    @pytest.mark.it('should send buffer with v5 media type "application/zip" and receive a buffer (auto mode)')
    def test_0023(self):
        pass

    @_live
    @pytest.mark.it("should subscribe dynamically via action")
    async def test_0024(self):
        topic = next_topic()
        send = {"topic": topic, "payload": "abc"}
        flows, ids = flow_for({"protocolVersion": 5}, {"datatype": "utf8", "topicType": "dynamic"}, {})
        msgs = await run_flow_for_seconds_scheduled(
            flows,
            [
                {"nid": ids["in"], "msg": {"action": "subscribe", "topic": topic}, "delay_ms": 600},
                {"nid": ids["out"], "msg": send, "delay_ms": 1200},
            ],
            2.0,
        )
        assert_mqtt(first_on_topic(msgs, topic), send)

    @_live
    @pytest.mark.it('should connect via "connect" action')
    async def test_0025(self):
        flows, ids = flow_for(
            {"protocolVersion": 5, "autoConnect": False},
            {"datatype": "utf8", "topicType": "dynamic"},
            {},
            status_scope=["mqtt.in"],
        )
        msgs = await run_flow_for_seconds_scheduled(
            flows,
            [{"nid": ids["in"], "msg": {"action": "connect"}, "delay_ms": 400}],
            3.0,
        )
        texts = [msg.get("status", {}).get("text") for msg in msgs]
        assert "node-red:common.status.connected" in texts, texts

    @_live
    @pytest.mark.it('should disconnect via "disconnect" action')
    async def test_0026(self):
        flows, ids = flow_for({"protocolVersion": 5}, None, {}, status_scope=["mqtt.out"])
        disconnect_at = 1200
        msgs = await run_flow_for_seconds_scheduled(
            flows,
            [{"nid": ids["out"], "msg": {"action": "disconnect"}, "delay_ms": disconnect_at}],
            3.0,
        )
        connected = [
            msg for msg in msgs if msg.get("status", {}).get("text") == "node-red:common.status.connected"
        ]
        disconnected = [
            msg
            for msg in msgs
            if "disconnect" in (msg.get("status", {}).get("text") or "") and msg.get("_since_start_ms", 0) >= disconnect_at
        ]
        assert connected, [msg.get("status") for msg in msgs]
        assert disconnected, [msg.get("status") for msg in msgs]

    @_live
    @pytest.mark.it("should publish birth message")
    async def test_0027(self):
        topic = next_topic() + "/birth"
        flows, ids = flow_for(
            {"autoConnect": False, "protocolVersion": 4, "birthTopic": topic, "birthPayload": "broker birth", "birthQos": 2},
            {"topic": topic, "datatype": "utf8"},
            {},
        )
        msgs = await run_flow_for_seconds_scheduled(
            flows,
            [{"nid": ids["in"], "msg": {"action": "connect"}, "delay_ms": 300}],
            2.0,
        )
        assert_mqtt(first_on_topic(msgs, topic), {"topic": topic, "payload": "broker birth", "qos": 2})

    @pytest.mark.skip(reason=_BAD_BIRTH)
    @pytest.mark.it("should safely discard bad birth topic")
    def test_0028(self):
        pass

    @_live
    @pytest.mark.it("should publish close message")
    async def test_0029(self):
        topic = next_topic() + "/close"
        flows, ids = flow_for(
            {"id": "mqtt.broker1"},
            {"broker": "mqtt.broker1", "topic": topic, "datatype": "json"},
            {"broker": "mqtt.broker2"},
            extra_brokers=[{"id": "mqtt.broker2", "closeTopic": topic, "closePayload": '{"msg":"close"}', "closeQos": 1}],
        )
        msgs = await run_flow_for_seconds_scheduled(
            flows,
            [{"nid": ids["out"], "msg": {"action": "disconnect"}, "delay_ms": 1200}],
            3.0,
        )
        assert_mqtt(
            first_on_topic(msgs, topic),
            {"topic": topic, "payload": {"msg": "close"}, "qos": 1},
        )

    @pytest.mark.skip(reason=_WILL)
    @pytest.mark.it("should publish will message")
    def test_0030(self):
        pass

    @pytest.mark.skip(reason=_WILL)
    @pytest.mark.it("should publish will message with V5 properties")
    def test_0031(self):
        pass
