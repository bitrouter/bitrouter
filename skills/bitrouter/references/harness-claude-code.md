# Harness: Claude

Run the bundled Claude ACP adapter through BRO. Start BitRouter separately
and set `BITROUTER_ENDPOINT` and `BITROUTER_API_KEY` for the selected gateway.
Use `bro acp list` to inspect available adapters. The bundled adapter launches
through `npx`.

To use a subscription route, configure the corresponding provider on the
gateway host:

```sh
bitrouter providers login claude-code
```

## ACP session

```sh
bro acp --workspace . prompt --agent claude 'Summarize this project'
bro acp --workspace . serve claude
```

`prompt` emits NDJSON and declines external permission requests. `serve`
forwards ACP over stdio so an ACP manager can handle permissions. Use
`prompt --model MODEL` to set an explicit model for a single prompt.
See [sessions and authority](sessions.md).

## Native BRO task

For BRO's native workspace tools, select a model from `bitrouter models`.
A subscription route uses the provider-qualified selector printed by
the gateway, for example `claude-code:claude-sonnet-5` when available. Pass that selector with
`bro task run --model MODEL`. BRO owns the local approvals and thread history.
