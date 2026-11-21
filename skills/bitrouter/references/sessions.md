# ACP controller and native sessions

BRO serves native workspace sessions and external adapters over local stdio.
Set `BITROUTER_ENDPOINT` to the gateway origin and provide an authorized
`BITROUTER_API_KEY` for gateway-bound adapters. The local BRO home is selected
by `--data-dir` or `BRO_HOME` and defaults to `~/.bro`.

## Native BRO ACP

```sh
bro acp --workspace PATH serve --model MODEL --read-only
```

Choose a model exposed by the gateway. Omit `--read-only` to allow the
controller to receive native effect approval requests. Native ACP sessions
use BRO threads and their durable checkpoint history. Use `bro thread list`,
`bro thread history` and `bro thread pending` to inspect retained local work.
An unresolved effect must be reconciled before the affected turn can continue.

## External adapter

```sh
bro acp list
bro acp --workspace PATH serve codex
bro acp --workspace PATH prompt --agent codex 'Summarize the project'
```

`serve AGENT` supervises one external adapter for the stdio connection. The ACP
manager handles permission requests, and the adapter owns its own session
history. The one-shot prompt emits NDJSON and declines tool permission requests.
For a local adapter configured by its operator, launch
`bro acp serve --agent NAME --direct --config agents.yaml`. See [ACP adapter
configuration](providers.md#acp-agents).
