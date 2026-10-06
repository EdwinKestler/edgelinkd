# Common n2link node patterns

Use these properties when the corresponding type appears in the supplied runtime catalog. Preserve an existing configuration-node ID verbatim when referencing it.

## Periodic timestamp

```json
{
  "type": "inject",
  "config": {
    "props": [{"p": "payload"}, {"p": "topic", "vt": "str"}],
    "repeat": "180",
    "crontab": "",
    "once": false,
    "onceDelay": 0.1,
    "topic": "",
    "payload": "",
    "payloadType": "date"
  }
}
```

`repeat` is seconds. Three minutes is `180`.

## Local text split

```json
{
  "type": "ai-split",
  "config": {
    "chunkSize": 512,
    "overlap": 0,
    "separator": "",
    "maxChunks": 256,
    "property": "payload"
  }
}
```

Empty `separator` is character windows. Do not invent a tokenizer.

## Structured JSON

```json
{
  "type": "ai-structured",
  "config": {
    "property": "payload",
    "schemaSource": "node",
    "schema": {"type": "object"},
    "output": "payload"
  }
}
```

Do not use `$ref`, `pattern`, or `allOf`.

## Embeddings

```json
{
  "type": "ai-embed",
  "config": {
    "provider": "EXISTING_AI_PROVIDER_ID",
    "model": "text-embedding-3-small",
    "property": "payload"
  }
}
```

`model` is required. Reuse an existing `ai-provider`. Do not use the chat `defaultModel`.

## MQTT publish and subscribe

```json
{
  "type": "mqtt out",
  "config": {
    "topic": "n2link/timestamp",
    "qos": "",
    "retain": "",
    "respTopic": "",
    "contentType": "",
    "userProps": "",
    "correl": "",
    "expiry": "",
    "broker": "EXISTING_MQTT_BROKER_ID"
  }
}
```

```json
{
  "type": "mqtt in",
  "config": {
    "topic": "n2link/timestamp",
    "qos": "1",
    "datatype": "auto-detect",
    "broker": "EXISTING_MQTT_BROKER_ID",
    "nl": false,
    "rap": true,
    "rh": 0,
    "inputs": 0
  }
}
```

Use the same topic and broker ID for a loopback publish/subscribe flow. If no MQTT broker configuration exists, return a warning and do not invent one or ask for its password in flow JSON.

## Convert a received timestamp to CSV

Use a `change` node to make an object before the CSV node:

```json
{
  "type": "change",
  "config": {
    "rules": [
      {"t": "set", "p": "payload", "pt": "msg", "to": "{\"timestamp\": payload}", "tot": "jsonata"}
    ]
  }
}
```

Then encode and append:

```json
{
  "type": "csv",
  "config": {
    "spec": "rfc",
    "sep": ",",
    "hdrin": false,
    "hdrout": "once",
    "multi": "one",
    "ret": "\\r\\n",
    "temp": "timestamp",
    "skip": "0",
    "strings": true,
    "include_empty_strings": false,
    "include_null_values": false
  }
}
```

```json
{
  "type": "file",
  "config": {
    "filename": "data/timestamps.csv",
    "filenameType": "str",
    "appendNewline": false,
    "createDir": true,
    "overwriteFile": "false",
    "encoding": "none"
  }
}
```

The CSV node already emits a record terminator, so `appendNewline` is false.
