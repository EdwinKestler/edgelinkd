# TOFSense plugin (example)

Decodes the Nooploop TOFSense / TOFSense P / TOFSense PS laser ranging sensor's UART protocol
(TOFSense User Manual V2.5, §2 and §8) as an EdgeLinkd WASM plugin node, `wasm-nooploop-tofsense`.

## Who does what

A WASM plugin has no I/O: it cannot open `/dev/ttyUSB0`. The serial port reaches the flow through
a built-in node; the plugin does the protocol:

```text
sensor ──UART 921600──► socat/ser2net ──TCP──► [tcp in] ──bytes──► [TOFSense] ──► distance
                                                                       │
            [inject {query: id}] ──► [TOFSense] ──query frame──► [tcp request] ──► [TOFSense] ──► distance
```

- **Active mode** (one module, 10 Hz, or 30 Hz for P/PS): the sensor streams
  `NLink_TOFSense_Frame0`. The plugin reassembles frames split across TCP reads, checks the
  checksum, resynchronises on garbage, and emits one message per frame.
- **Query mode** (cascaded modules with different ids): `msg.query = <id>` makes the plugin emit
  `NLink_TOFSense_Read_Frame0` (`57 10 FF FF id FF FF sum`) on output 2. Send it with
  `tcp request` (return a buffer, fixed count 16) and feed the reply to a second TOFSense node.
  Use two nodes rather than wiring the reply back into the same node.

Each reading: `msg.payload` = distance, `msg.tof` = `{id, distance, unit, status, signal,
systemTimeMs, valid}`. `valid` means checksum ok, `dis_status == 0` and signal ≥ `minSignal`
(manual Q10: only status 0 is usable; out-of-range values jump or read −0.01 m).

| Config | Default | |
|---|---|---|
| Distance unit | `m` | `m` or `mm` |
| Minimum signal strength | 0 | readings below are `valid: false` |
| Only output valid readings | false | drop invalid readings instead |
| Only this module id | −1 | filter one id on a shared bus |

## Real hardware

Wire the sensor to a USB-to-TTL adapter (or the Pi's UART; check the datasheet for supply and
3.3 V logic), set it to UART active output with NAssistant (default 921600 baud), then expose the
port and point `tcp in` (client, stream of Buffer) at it:

```sh
socat TCP-LISTEN:7000,reuseaddr FILE:/dev/ttyUSB0,b921600,raw,echo=0
```

## Without hardware

`tools/fake_tofsense.py` serves frames like the sensor (active on TCP 7000, query on 7001),
splitting every third frame across two writes and marking every 25th frame out of range.

```sh
cargo test                                               # decoder tests with the manual's frames
cargo build --release --target wasm32-unknown-unknown    # the installable package (~95 KiB)
python3 tools/fake_tofsense.py
```

Install `target/wasm32-unknown-unknown/release/n2link_plugin_tofsense.wasm` with
`n2linkd plugin stage` / `activate`, or `POST /wasm/plugins/stage` and
`/wasm/plugins/nooploop/tofsense/activate`, then reload the editor.
