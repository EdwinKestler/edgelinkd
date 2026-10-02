import pytest
from tests import *

@pytest.mark.describe('CSV node (Legacy Mode)')
class TestCsvNodeLegacyMode:
    @pytest.mark.asyncio
    @pytest.mark.it('should be loaded with defaults')
    async def test_json_string_to_object(self):
        pass

    @pytest.mark.describe('csv to json')
    class TestCsvToJson:

        @pytest.mark.asyncio
        @pytest.mark.it('should convert a simple csv string to a javascript object')
        async def test_buffer_json_string_to_object(self):
            # JavaScript equivalent:
            # var flow = [ { id:"n1", type:"csv", temp:"a,b,c,d", wires:[["n2"]] },
            #     {id:"n2", type:"helper"} ];
            # var testString = "1,2,3,4"+String.fromCharCode(10);
            # n1.emit("input", {payload:testString});
            # Expected: msg.payload = { a: 1, b: 2, c: 3, d: 4 }
            # Expected: msg.columns = "a,b,c,d"

            flows = [
                {"id": "100", "type": "tab"},
                {"id": "101", "z": "100", "type": "csv", "temp": "a,b,c,d", "wires": [["102"]]},
                {"id": "102", "z": "100", "type": "test-once"}
            ]

            # Test string with newline character (equivalent to String.fromCharCode(10))
            test_string = "1,2,3,4\n"

            injections = [
                {"nid": "101", "msg": {"payload": test_string}}
            ]

            msgs = await run_flow_with_msgs_ntimes(flows, injections, 1)

            # Verify the output
            assert len(msgs) == 1
            msg = msgs[0]

            # Check payload - should be converted to object
            assert msg["payload"] == {"a": 1, "b": 2, "c": 3, "d": 4}

            # Check columns property
            assert msg["columns"] == "a,b,c,d"

        @pytest.mark.asyncio
        @pytest.mark.it('should convert a simple string to a javascript object with | separator (no template)')
        async def test_convert_with_pipe_separator_no_template(self):
            # JavaScript equivalent:
            # var flow = [ { id:"n1", type:"csv", sep:"|", wires:[["n2"]] },
            #     {id:"n2", type:"helper"} ];
            # var testString = "1|2|3|4"+String.fromCharCode(10);
            # Expected: msg.payload = { col1: 1, col2: 2, col3: 3, col4: 4 }
            # Expected: msg.columns = "col1,col2,col3,col4"

            flows = [
                {"id": "100", "type": "tab"},
                {"id": "1", "z": "100", "type": "csv", "sep": "|", "wires": [["2"]]},
                {"id": "2", "z": "100", "type": "test-once"}
            ]

            # Test string with pipe separator
            test_string = "1|2|3|4\n"

            injections = [
                {"nid": "1", "msg": {"payload": test_string}}
            ]

            msgs = await run_flow_with_msgs_ntimes(flows, injections, 1)

            # Verify the output
            assert len(msgs) == 1
            msg = msgs[0]

            # Check payload - should be converted to object with default column names
            assert msg["payload"] == {"col1": 1, "col2": 2, "col3": 3, "col4": 4}

            # Check columns property
            assert msg["columns"] == "col1,col2,col3,col4"

        @pytest.mark.asyncio
        @pytest.mark.it('should convert a simple string to a javascript object with tab separator (with template)')
        async def test_convert_with_tab_separator_with_template(self):
            # JavaScript equivalent:
            # var flow = [ { id:"n1", type:"csv", sep:"\t", temp:"A,B,,D", wires:[["n2"]] },
            #     {id:"n2", type:"helper"} ];
            # var testString = "1\t2\t3\t4"+String.fromCharCode(10);
            # Expected: msg.payload = { A: 1, B: 2, D: 4 }
            # Expected: msg.columns = "A,B,D"

            flows = [
                {"id": "100", "type": "tab"},
                {"id": "1", "z": "100", "type": "csv", "sep": "\t", "temp": "A,B,,D", "wires": [["2"]]},
                {"id": "2", "z": "100", "type": "test-once"}
            ]

            # Test string with tab separator
            test_string = "1\t2\t3\t4\n"

            injections = [
                {"nid": "1", "msg": {"payload": test_string}}
            ]

            msgs = await run_flow_with_msgs_ntimes(flows, injections, 1)

            # Verify the output
            assert len(msgs) == 1
            msg = msgs[0]

            # Check payload - should skip the third column (empty in template)
            assert msg["payload"] == {"A": 1, "B": 2, "D": 4}

            # Check columns property
            assert msg["columns"] == "A,B,D"

        @pytest.mark.asyncio
        @pytest.mark.it('should convert a simple string to a javascript object with space separator (with spaced template)')
        async def test_convert_with_space_separator_with_spaced_template(self):
            # JavaScript equivalent:
            # var flow = [ { id:"n1", type:"csv", sep:" ", temp:"A, B, , D", wires:[["n2"]] },
            #     {id:"n2", type:"helper"} ];
            # var testString = "1 2 3 4"+String.fromCharCode(10);
            # Expected: msg.payload = { A: 1, B: 2, D: 4 }
            # Expected: msg.columns = "A,B,D"

            flows = [
                {"id": "100", "type": "tab"},
                {"id": "1", "z": "100", "type": "csv", "sep": " ", "temp": "A, B, , D", "wires": [["2"]]},
                {"id": "2", "z": "100", "type": "test-once"}
            ]

            # Test string with space separator
            test_string = "1 2 3 4\n"

            injections = [
                {"nid": "1", "msg": {"payload": test_string}}
            ]

            msgs = await run_flow_with_msgs_ntimes(flows, injections, 1)

            # Verify the output
            assert len(msgs) == 1
            msg = msgs[0]

            # Check payload - should skip the third column (empty in template)
            assert msg["payload"] == {"A": 1, "B": 2, "D": 4}

            # Check columns property
            assert msg["columns"] == "A,B,D"

        @pytest.mark.asyncio
        @pytest.mark.it('should remove quotes and whitespace from template')
        async def test_remove_quotes_and_whitespace_from_template(self):
            # JavaScript equivalent:
            # var flow = [ { id:"n1", type:"csv", temp:'"a",  "b" , " c "," d  " ', wires:[["n2"]] },
            #     {id:"n2", type:"helper"} ];
            # var testString = "1,2,3,4"+String.fromCharCode(10);
            # Expected: msg.payload = { a: 1, b: 2, c: 3, d: 4 }

            flows = [
                {"id": "100", "type": "tab"},
                {"id": "1", "z": "100", "type": "csv", "temp": '"a",  "b" , " c "," d  " ', "wires": [["2"]]},
                {"id": "2", "z": "100", "type": "test-once"}
            ]

            # Test string with comma separator
            test_string = "1,2,3,4\n"

            injections = [
                {"nid": "1", "msg": {"payload": test_string}}
            ]

            msgs = await run_flow_with_msgs_ntimes(flows, injections, 1)

            # Verify the output
            assert len(msgs) == 1
            msg = msgs[0]

            # Check payload - quotes and whitespace should be removed from template
            assert msg["payload"] == {"a": 1, "b": 2, "c": 3, "d": 4}

            # Check columns property (should be clean column names)
            assert msg["columns"] == "a,b,c,d"

    @pytest.mark.skip(reason='out of scope: the assertion is the mocha node.warn log key csv.errors.csv_js, and EdgeLinkd has no node.warn channel. A legacy bad type sets a red status and completes the message without sending it')
    @pytest.mark.asyncio
    @pytest.mark.it('should warn if provided a number or boolean')
    async def test_legacy_warn_bad_type(self):
        pass

    @pytest.mark.asyncio
    @pytest.mark.it('should call done when input causes an error')
    async def test_legacy_done_on_bad_type(self):
        # spec "" is what the editor saves for legacy. Complete fires. Catch must not.
        flows = [
            {"id": "100", "type": "tab"},
            {"id": "101", "z": "100", "type": "csv", "spec": "", "temp": "a,b,c,d", "wires": [[]]},
            {"id": "103", "z": "100", "type": "complete", "scope": ["101"], "uncaught": False, "wires": [["102"]]},
            {"id": "104", "z": "100", "type": "catch", "scope": ["101"], "uncaught": False, "wires": [["102"]]},
            {"id": "102", "z": "100", "type": "test-once"},
        ]
        msgs = await run_flow_for_seconds(flows, [{"nid": "101", "msg": {"payload": 1}}], 0.3)
        assert len(msgs) == 1, msgs
        assert msgs[0]["payload"] == 1
        assert "error" not in msgs[0]


# RFC mode is the legacy splitter plus quoting and a CRLF default. Embedded newlines
# inside quotes match, and a bad type is reported instead of dropped. Every other RFC
# `it()` stays skipped because the node does not implement Node-RED's full RFC parser
# (strict templates, preserved numeric strings, and the rest).
@pytest.mark.describe('CSV node (RFC Mode)')
class TestCsvNodeRfcMode:
    @pytest.mark.asyncio
    @pytest.mark.it('should be loaded with defaults')
    async def test_rfc_0001(self):
        pass

    @pytest.mark.skip(reason='spec: "rfc" is the legacy splitter plus quoting and a CRLF default, not Node-RED\'s full RFC mode')
    @pytest.mark.asyncio
    @pytest.mark.it('should just pass through if no payload provided')
    async def test_rfc_0002(self):
        pass

    @pytest.mark.asyncio
    @pytest.mark.it('should warn if provided a number or boolean')
    async def test_rfc_0003(self):
        # helper.log() is not readable here. RFC mode reports csv.errors.csv_js on the
        # catch node and does not send the payload.
        flows = [
            {"id": "100", "type": "tab"},
            {"id": "101", "z": "100", "type": "csv", "spec": "rfc", "temp": "a,b,c,d", "wires": [[]]},
            {"id": "103", "z": "100", "type": "catch", "scope": ["101"], "uncaught": False, "wires": [["102"]]},
            {"id": "102", "z": "100", "type": "test-once"},
        ]
        msgs = await run_flow_with_msgs_ntimes(
            flows,
            [{"nid": "101", "msg": {"payload": 1}}, {"nid": "101", "msg": {"payload": True}}],
            2,
        )
        assert [m["payload"] for m in msgs] == [1, True]
        assert all(
            m["error"]["message"] == "This node only handles CSV strings or js objects." for m in msgs
        )

    @pytest.mark.asyncio
    @pytest.mark.it('should call done when message processing is completed')
    async def test_rfc_0004(self):
        flows = [
            {"id": "100", "type": "tab"},
            {"id": "101", "z": "100", "type": "csv", "spec": "rfc", "temp": "a,b,c,d", "wires": [[]]},
            {"id": "103", "z": "100", "type": "complete", "scope": ["101"], "uncaught": False, "wires": [["102"]]},
            {"id": "102", "z": "100", "type": "test-once"},
        ]
        msgs = await run_flow_with_msgs_ntimes(flows, [{"nid": "101", "msg": {"payload": "1,2,3,4"}}], 1)
        assert msgs[0]["payload"] == "1,2,3,4"
        assert "error" not in msgs[0]

    @pytest.mark.asyncio
    @pytest.mark.it('should not call done or pass the bad msg through when input causes an error - should throw error and set status')
    async def test_rfc_0005(self):
        # The harness cannot read node status. RFC mode sets a red status and reports
        # the catalog string on the catch node only.
        flows = [
            {"id": "100", "type": "tab"},
            {"id": "101", "z": "100", "type": "csv", "spec": "rfc", "temp": "a,b,c,d", "wires": [[]]},
            {"id": "103", "z": "100", "type": "complete", "scope": ["101"], "uncaught": False, "wires": [["102"]]},
            {"id": "104", "z": "100", "type": "catch", "scope": ["101"], "uncaught": False, "wires": [["102"]]},
            {"id": "102", "z": "100", "type": "test-once"},
        ]
        msgs = await run_flow_for_seconds(flows, [{"nid": "101", "msg": {"payload": 1}}], 0.3)
        assert len(msgs) == 1, msgs
        assert msgs[0]["payload"] == 1
        assert msgs[0]["error"]["message"] == "This node only handles CSV strings or js objects."

    @pytest.mark.describe('csv to json')
    class TestRfcCsvToJson:
        @pytest.mark.skip(reason='spec: "rfc" is the legacy splitter plus quoting and a CRLF default, not Node-RED\'s full RFC mode')
        @pytest.mark.asyncio
        @pytest.mark.it('should convert a simple csv string to a javascript object')
        async def test_rfc_0006(self):
            pass

        @pytest.mark.skip(reason='spec: "rfc" is the legacy splitter plus quoting and a CRLF default, not Node-RED\'s full RFC mode')
        @pytest.mark.asyncio
        @pytest.mark.it('should convert a simple string to a javascript object with | separator (no template)')
        async def test_rfc_0007(self):
            pass

        @pytest.mark.skip(reason='spec: "rfc" is the legacy splitter plus quoting and a CRLF default, not Node-RED\'s full RFC mode')
        @pytest.mark.asyncio
        @pytest.mark.it('should convert a simple string to a javascript object with tab separator (with template)')
        async def test_rfc_0008(self):
            pass

        @pytest.mark.skip(reason='spec: "rfc" is the legacy splitter plus quoting and a CRLF default, not Node-RED\'s full RFC mode')
        @pytest.mark.asyncio
        @pytest.mark.it('should convert a simple string to a javascript object with space separator (with spaced template)')
        async def test_rfc_0009(self):
            pass

        @pytest.mark.skip(reason='spec: "rfc" is the legacy splitter plus quoting and a CRLF default, not Node-RED\'s full RFC mode')
        @pytest.mark.asyncio
        @pytest.mark.it('should not remove quotes and whitespace from template - should set status and send warning')
        async def test_rfc_0010(self):
            pass

        @pytest.mark.skip(reason='spec: "rfc" is the legacy splitter plus quoting and a CRLF default, not Node-RED\'s full RFC mode')
        @pytest.mark.asyncio
        @pytest.mark.it('should create column names if no template provided')
        async def test_rfc_0011(self):
            pass

        @pytest.mark.skip(reason='spec: "rfc" is the legacy splitter plus quoting and a CRLF default, not Node-RED\'s full RFC mode')
        @pytest.mark.asyncio
        @pytest.mark.it('should allow dropping of fields from the template')
        async def test_rfc_0012(self):
            pass

        @pytest.mark.skip(reason='spec: "rfc" is the legacy splitter plus quoting and a CRLF default, not Node-RED\'s full RFC mode')
        @pytest.mark.asyncio
        @pytest.mark.it('should allow commas and spaces in the template')
        async def test_rfc_0013(self):
            pass

        @pytest.mark.skip(reason='spec: "rfc" is the legacy splitter plus quoting and a CRLF default, not Node-RED\'s full RFC mode')
        @pytest.mark.asyncio
        @pytest.mark.it('should allow passing in a template as first line of CSV')
        async def test_rfc_0014(self):
            pass

        @pytest.mark.skip(reason='spec: "rfc" is the legacy splitter plus quoting and a CRLF default, not Node-RED\'s full RFC mode')
        @pytest.mark.asyncio
        @pytest.mark.it('should allow passing in a template as first line of CSV (not comma)')
        async def test_rfc_0015(self):
            pass

        @pytest.mark.skip(reason='spec: "rfc" is the legacy splitter plus quoting and a CRLF default, not Node-RED\'s full RFC mode')
        @pytest.mark.asyncio
        @pytest.mark.it('should allow passing in a template as first line of CSV (special char /)')
        async def test_rfc_0016(self):
            pass

        @pytest.mark.skip(reason='spec: "rfc" is the legacy splitter plus quoting and a CRLF default, not Node-RED\'s full RFC mode')
        @pytest.mark.asyncio
        @pytest.mark.it('should allow passing in a template as first line of CSV (special char \\)')
        async def test_rfc_0017(self):
            pass

        @pytest.mark.skip(reason='spec: "rfc" is the legacy splitter plus quoting and a CRLF default, not Node-RED\'s full RFC mode')
        @pytest.mark.asyncio
        @pytest.mark.it('should leave numbers starting with 0, e and + as strings (except 0.)')
        async def test_rfc_0018(self):
            pass

        @pytest.mark.skip(reason='spec: "rfc" is the legacy splitter plus quoting and a CRLF default, not Node-RED\'s full RFC mode')
        @pytest.mark.asyncio
        @pytest.mark.it('should not parse numbers when told not to do so')
        async def test_rfc_0019(self):
            pass

        @pytest.mark.skip(reason='spec: "rfc" is the legacy splitter plus quoting and a CRLF default, not Node-RED\'s full RFC mode')
        @pytest.mark.asyncio
        @pytest.mark.it('should parse numbers when told to do so')
        async def test_rfc_0020(self):
            pass

        @pytest.mark.skip(reason='spec: "rfc" is the legacy splitter plus quoting and a CRLF default, not Node-RED\'s full RFC mode')
        @pytest.mark.asyncio
        @pytest.mark.it('should leave handle strings with scientific notation as numbers')
        async def test_rfc_0021(self):
            pass

        @pytest.mark.skip(reason='spec: "rfc" is the legacy splitter plus quoting and a CRLF default, not Node-RED\'s full RFC mode')
        @pytest.mark.asyncio
        @pytest.mark.it('should allow quotes in the input (but drop blank strings)')
        async def test_rfc_0022(self):
            pass

        @pytest.mark.skip(reason='spec: "rfc" is the legacy splitter plus quoting and a CRLF default, not Node-RED\'s full RFC mode')
        @pytest.mark.asyncio
        @pytest.mark.it('should allow blank strings in the input if selected')
        async def test_rfc_0023(self):
            pass

        @pytest.mark.skip(reason='spec: "rfc" is the legacy splitter plus quoting and a CRLF default, not Node-RED\'s full RFC mode')
        @pytest.mark.asyncio
        @pytest.mark.it('should allow missing columns (nulls) in the input if selected')
        async def test_rfc_0024(self):
            pass

        @pytest.mark.skip(reason='spec: "rfc" is the legacy splitter plus quoting and a CRLF default, not Node-RED\'s full RFC mode')
        @pytest.mark.asyncio
        @pytest.mark.it('should handle cr and lf in the input')
        async def test_rfc_0025(self):
            pass

        @pytest.mark.skip(reason='spec: "rfc" is the legacy splitter plus quoting and a CRLF default, not Node-RED\'s full RFC mode')
        @pytest.mark.asyncio
        @pytest.mark.it('should recover from an odd number of quotes in the input')
        async def test_rfc_0026(self):
            pass

        @pytest.mark.asyncio
        @pytest.mark.it('should handle newlines in the input data')
        async def test_rfc_0027(self):
            flows = [
                {"id": "100", "type": "tab"},
                {"id": "101", "z": "100", "type": "csv", "spec": "rfc", "temp": "a,b,c,d,e,f,g", "wires": [["102"]]},
                {"id": "102", "z": "100", "type": "test-once"},
            ]
            payload = 'ay,be,"c has 2\nnew\nlines",dee,eee,eff,gee'
            msgs = await run_flow_with_msgs_ntimes(flows, [{"nid": "101", "msg": {"payload": payload}}], 1)
            assert msgs[0]["payload"] == {
                "a": "ay", "b": "be", "c": "c has 2\nnew\nlines",
                "d": "dee", "e": "eee", "f": "eff", "g": "gee",
            }
            assert msgs[0]["parts"]["index"] == 0
            assert msgs[0]["parts"]["count"] == 1

        @pytest.mark.skip(reason='spec: "rfc" is the legacy splitter plus quoting and a CRLF default, not Node-RED\'s full RFC mode')
        @pytest.mark.asyncio
        @pytest.mark.it('should be able to use the first line as a template')
        async def test_rfc_0028(self):
            pass

        @pytest.mark.skip(reason='spec: "rfc" is the legacy splitter plus quoting and a CRLF default, not Node-RED\'s full RFC mode')
        @pytest.mark.asyncio
        @pytest.mark.it('should be able to output multiple lines as one array')
        async def test_rfc_0029(self):
            pass

        @pytest.mark.skip(reason='spec: "rfc" is the legacy splitter plus quoting and a CRLF default, not Node-RED\'s full RFC mode')
        @pytest.mark.asyncio
        @pytest.mark.it('should be able to create an array from multiple parts')
        async def test_rfc_0030(self):
            pass

        @pytest.mark.skip(reason='spec: "rfc" is the legacy splitter plus quoting and a CRLF default, not Node-RED\'s full RFC mode')
        @pytest.mark.asyncio
        @pytest.mark.it('should be able to output multiple objects as an array from an input of parts')
        async def test_rfc_0031(self):
            pass

        @pytest.mark.skip(reason='spec: "rfc" is the legacy splitter plus quoting and a CRLF default, not Node-RED\'s full RFC mode')
        @pytest.mark.asyncio
        @pytest.mark.it('should handle numbers in strings but not IP addresses')
        async def test_rfc_0032(self):
            pass

        @pytest.mark.skip(reason='spec: "rfc" is the legacy splitter plus quoting and a CRLF default, not Node-RED\'s full RFC mode')
        @pytest.mark.asyncio
        @pytest.mark.it('should preserve parts property')
        async def test_rfc_0033(self):
            pass

        @pytest.mark.skip(reason='spec: "rfc" is the legacy splitter plus quoting and a CRLF default, not Node-RED\'s full RFC mode')
        @pytest.mark.asyncio
        @pytest.mark.it('should be able to use the first of multiple parts as a template if parts are present')
        async def test_rfc_0034(self):
            pass

        @pytest.mark.skip(reason='spec: "rfc" is the legacy splitter plus quoting and a CRLF default, not Node-RED\'s full RFC mode')
        @pytest.mark.asyncio
        @pytest.mark.it('should skip several lines from start if requested')
        async def test_rfc_0035(self):
            pass

        @pytest.mark.skip(reason='spec: "rfc" is the legacy splitter plus quoting and a CRLF default, not Node-RED\'s full RFC mode')
        @pytest.mark.asyncio
        @pytest.mark.it('should skip several lines from start then use next line as a template')
        async def test_rfc_0036(self):
            pass

        @pytest.mark.skip(reason='spec: "rfc" is the legacy splitter plus quoting and a CRLF default, not Node-RED\'s full RFC mode')
        @pytest.mark.asyncio
        @pytest.mark.it('should skip several lines from start and correct parts')
        async def test_rfc_0037(self):
            pass

        @pytest.mark.skip(reason='spec: "rfc" is the legacy splitter plus quoting and a CRLF default, not Node-RED\'s full RFC mode')
        @pytest.mark.asyncio
        @pytest.mark.it('should be able to skip and then use the first of multiple parts as a template if parts are present')
        async def test_rfc_0038(self):
            pass

    @pytest.mark.describe('json object to csv')
    class TestRfcJsonToCsv:
        @pytest.mark.skip(reason='spec: "rfc" is the legacy splitter plus quoting and a CRLF default, not Node-RED\'s full RFC mode')
        @pytest.mark.asyncio
        @pytest.mark.it('should convert a simple object back to a csv')
        async def test_rfc_0039(self):
            pass

        @pytest.mark.skip(reason='spec: "rfc" is the legacy splitter plus quoting and a CRLF default, not Node-RED\'s full RFC mode')
        @pytest.mark.asyncio
        @pytest.mark.it('should convert a simple object back to a csv with no template')
        async def test_rfc_0040(self):
            pass

        @pytest.mark.skip(reason='spec: "rfc" is the legacy splitter plus quoting and a CRLF default, not Node-RED\'s full RFC mode')
        @pytest.mark.asyncio
        @pytest.mark.it('should convert a simple object back to a tsv using a tab as a separator')
        async def test_rfc_0041(self):
            pass

        @pytest.mark.skip(reason='spec: "rfc" is the legacy splitter plus quoting and a CRLF default, not Node-RED\'s full RFC mode')
        @pytest.mark.asyncio
        @pytest.mark.it('should convert a simple object back to a tsv with headers using a tab as a separator')
        async def test_rfc_0042(self):
            pass

        @pytest.mark.skip(reason='spec: "rfc" is the legacy splitter plus quoting and a CRLF default, not Node-RED\'s full RFC mode')
        @pytest.mark.asyncio
        @pytest.mark.it('should handle a template with spaces in the property names')
        async def test_rfc_0043(self):
            pass

        @pytest.mark.skip(reason='spec: "rfc" is the legacy splitter plus quoting and a CRLF default, not Node-RED\'s full RFC mode')
        @pytest.mark.asyncio
        @pytest.mark.it('should handle a template with quotes in the property names')
        async def test_rfc_0044(self):
            pass

        @pytest.mark.skip(reason='spec: "rfc" is the legacy splitter plus quoting and a CRLF default, not Node-RED\'s full RFC mode')
        @pytest.mark.asyncio
        @pytest.mark.it('should convert an array of objects to a multi-line csv')
        async def test_rfc_0045(self):
            pass

        @pytest.mark.skip(reason='spec: "rfc" is the legacy splitter plus quoting and a CRLF default, not Node-RED\'s full RFC mode')
        @pytest.mark.asyncio
        @pytest.mark.it('should convert an array of objects to a multi-line csv and add a header')
        async def test_rfc_0046(self):
            pass

        @pytest.mark.skip(reason='spec: "rfc" is the legacy splitter plus quoting and a CRLF default, not Node-RED\'s full RFC mode')
        @pytest.mark.asyncio
        @pytest.mark.it('should convert an array of objects to a multi-line csv without a template')
        async def test_rfc_0047(self):
            pass

        @pytest.mark.skip(reason='spec: "rfc" is the legacy splitter plus quoting and a CRLF default, not Node-RED\'s full RFC mode')
        @pytest.mark.asyncio
        @pytest.mark.it('should convert an array of objects to a multi-line csv without a template and with a header')
        async def test_rfc_0048(self):
            pass

        @pytest.mark.skip(reason='spec: "rfc" is the legacy splitter plus quoting and a CRLF default, not Node-RED\'s full RFC mode')
        @pytest.mark.asyncio
        @pytest.mark.it('should convert a simple array back to a csv')
        async def test_rfc_0049(self):
            pass

        @pytest.mark.skip(reason='spec: "rfc" is the legacy splitter plus quoting and a CRLF default, not Node-RED\'s full RFC mode')
        @pytest.mark.asyncio
        @pytest.mark.it('should convert an array of arrays back to a multi-line csv')
        async def test_rfc_0050(self):
            pass

        @pytest.mark.skip(reason='spec: "rfc" is the legacy splitter plus quoting and a CRLF default, not Node-RED\'s full RFC mode')
        @pytest.mark.asyncio
        @pytest.mark.it('should be able to include column names as first row')
        async def test_rfc_0051(self):
            pass

        @pytest.mark.skip(reason='spec: "rfc" is the legacy splitter plus quoting and a CRLF default, not Node-RED\'s full RFC mode')
        @pytest.mark.asyncio
        @pytest.mark.it('should be able to include column names as first row, and missing properties')
        async def test_rfc_0052(self):
            pass

        @pytest.mark.skip(reason='spec: "rfc" is the legacy splitter plus quoting and a CRLF default, not Node-RED\'s full RFC mode')
        @pytest.mark.asyncio
        @pytest.mark.it('should be able to pass in column names')
        async def test_rfc_0053(self):
            pass

        @pytest.mark.skip(reason='spec: "rfc" is the legacy splitter plus quoting and a CRLF default, not Node-RED\'s full RFC mode')
        @pytest.mark.asyncio
        @pytest.mark.it('should be able to pass in column names - with payload as an array')
        async def test_rfc_0054(self):
            pass

        @pytest.mark.skip(reason='spec: "rfc" is the legacy splitter plus quoting and a CRLF default, not Node-RED\'s full RFC mode')
        @pytest.mark.asyncio
        @pytest.mark.it('should handle quotes and sub-properties')
        async def test_rfc_0055(self):
            pass
