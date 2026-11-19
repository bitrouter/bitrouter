# Persistent Pi CLI/TUI connection

```sh
bro connect pi --dry-run
bro connect pi --base-url https://gateway.example --yes
bro connect pi --instance /path/to/pi-agent-dir --status
bro connect pi --check
bro connect pi --update --base-url https://gateway.example --yes
bro connect pi --remove --yes
```

The agent directory is `--instance`, then `PI_CODING_AGENT_DIR`, then
`~/.pi/agent`. An explicit instance is the agent directory itself, not the
project `.pi` directory. SDK clients using another `agentDir` need that same
directory supplied explicitly. Each directory has its own connection receipt.
The scope is Pi's CLI/TUI.

Setup adds an independent `bitrouter` provider to `models.json` and a saved
`api_key` credential to `auth.json`. Other providers and credentials remain
available. The token is persisted privately; starting Pi from another shell
does not require `BITROUTER_API_KEY`. Runtime `--api-key` and provider extensions
can still override authentication. Existing `bitrouter` provider/auth entries
are conflicts, so setup cannot overwrite a manually configured provider.

The default request model is exactly `bitrouter/automatos`. Repeat `--model` to
register several exact IDs in the same provider. The first keeps the custom
`--display-name`; additional entries use their exact IDs as labels. Updates
replace the complete list when model flags are supplied and preserve it
otherwise. Removal restores the saved startup route if any owned ID is selected.
Workflow bindings remain native Pi settings. `--model ID` changes
the wire ID, which must exist in the gateway catalog. Pi picker rows display
the wire ID; `--display-name` supplies the model's detail/search name and cannot
rename that ID. Select the entry under `[bitrouter]` in `/model`. Its fully
qualified Pi identity is `bitrouter/<wire-ID>`; a repeated provider segment in
that local identity does not change the wire ID.

The default protocol is Chat Completions (`openai-completions`).
`--protocol responses` selects `openai-responses`; `--protocol messages`
selects `anthropic-messages`. OpenAI base URLs end in `/v1`; Messages uses the
gateway root. Reverse-proxy path prefixes are retained.

Opening `/model` reloads the model configuration. Setup keeps the current
startup defaults. It snapshots `settings.json` without changing existing
bytes; a fresh agent directory receives `{}`. Removal restores the previous
default provider/model only if the connected BitRouter model is still the
saved default. Later native choices and unrelated settings/provider/auth edits
survive. Edits to the owned BitRouter provider or credential must be resolved
before removal can proceed; the receipt is retained on conflict.

`--status` is offline. `--check` checks the saved credential against the gateway
catalog without inference. `--update` replaces the managed connection with
recoverable rollback. A successful update returns a saved BitRouter default to
its previous native default, so reselect BitRouter afterward if desired.

No model context, output, price, image or reasoning metadata is invented.
Pi supplies defaults for omitted fields, so values such as 128K context or
zero estimated cost in its UI are Pi defaults, not verified gateway limits or
pricing. Catalog checks establish availability only; connection reports keep
`inference_verified: false`.

Native acceptance used `@earendil-works/pi-coding-agent` 1.2.0 for the TUI picker
and 1.4.0 for streaming/read-tool execution through the actual BitRouter gateway. Older
`@mariozechner/pi-coding-agent` releases have different credential-value
semantics; their compatibility is unverified. In particular, literal `$` and
leading `!` escaping follows current Pi semantics. Verify the installed version
before using a legacy installation.

Sources: [Pi model setup](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/models.md),
[agent directory](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/configuration.md),
[model fields](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/models-configuration.md),
[legacy value resolution](https://github.com/earendil-works/pi/blob/v0.58.0/packages/coding-agent/src/core/resolve-config-value.ts).
