# OpenCode persistent BitRouter provider

```sh
bro connect opencode --dry-run
bro connect opencode --yes
bro connect opencode --instance /path/to/opencode.jsonc --yes
bro connect opencode --base-url https://gateway.example \
  --model team/coder --display-name bitrouter/automatos --protocol responses --yes
bro connect opencode --status
bro connect opencode --check
bro connect opencode --update --protocol messages --yes
bro connect opencode --remove --yes
```

The connector adds `provider.bitrouter` with provider name **BitRouter** and a
model picker entry named `bitrouter/automatos` by default. The stable OpenCode
selection ID is `bitrouter/automatos`: provider `bitrouter`, model key
`automatos`. The model's `id` is the exact gateway model supplied by `--model`;
its `name` is the independent `--display-name`. A label never creates a gateway
alias. Existing provider entries and the selected `model`/`small_model` stay
unchanged during setup.

The provider SDK matches the selected gateway protocol:

| `--protocol` | OpenCode `npm` | Gateway API |
| --- | --- | --- |
| `chat-completions` (default) | `@ai-sdk/openai-compatible` | `/v1/chat/completions` |
| `responses` | `@ai-sdk/openai` | `/v1/responses` |
| `messages` | `@ai-sdk/anthropic` | `/v1/messages` |

No context limit, output limit, price, modality, or reasoning capability is
invented from a model ID. The connector leaves other providers' metadata intact.
Protocol configuration does not prove a particular upstream's tool compatibility.

## Config instance and surfaces

`--instance` accepts a config file or directory. A directory uses an existing
`opencode.jsonc`, otherwise `opencode.json`. Without the flag, discovery uses:

1. `OPENCODE_CONFIG_DIR`'s config file, when set.
2. `OPENCODE_CONFIG`, when set.
3. `$XDG_CONFIG_HOME/opencode`, otherwise `~/.config/opencode`.

Use the same custom path when inspecting, checking, updating, or removing a
connection. Receipts are keyed by the actual resolved config file. JSONC
comments, trailing commas, and unrelated settings are preserved. Existing
BitRouter provider definitions and provider allowlists that exclude BitRouter
are conflicts; review those settings before connecting. Known global/custom
configuration collisions are rejected. Default discovery also checks ancestor
project configurations. The connector never writes the working project's config
as an implicit destination.

Restart OpenCode CLI/TUI or its local Desktop/Web server after a change, then
select **BitRouter → bitrouter/automatos** in the model picker (`/models` in the
TUI). Reopening a model picker alone may retain an already running server's
cached configuration. A custom config applies only to OpenCode processes that
load it. Project, inline, and managed configuration can have higher precedence.

Desktop starts a local OpenCode server or connects to a selected server. These
local file changes configure the local server that reads the selected instance.
For Desktop/Web attached to a remote server, run the connector on that server
against its actual config. A successful local setup does not configure a remote
server.

## Credentials and restoration

OpenCode template markers `{env:` and `{file:` are rejected in the endpoint,
credential, model ID, and display name. User-supplied values cannot trigger
OpenCode variable/file substitution.

The gateway credential is stored literally in the owned provider's
`options.apiKey`. Configs and connection receipts are private files (0600 on
Unix); newly created directories are private (0700). No OpenCode `auth.json`
entry is added, replaced, or removed. The provider's explicit credential takes
precedence over a pre-existing authentication entry for the same provider in the
native clients tested.

`--remove` deletes only the unchanged owned provider. Later provider additions,
settings, and comments survive. If a current `model` or `small_model` still
selects `bitrouter/automatos`, its pre-connection value is restored; a later
selection of another model is preserved. Native client state outside the config
file is not rewritten. A native-added `$schema` can therefore leave the config
file present after removal. An edited owned provider is a conflict and keeps the
receipt for recovery.

This is durable native configuration. `bro launch opencode` remains a separate,
temporary launch integration. No BitRouter harness process is required after
connecting. `--status` verifies saved file state offline; `--check` additionally
verifies the gateway catalog using the saved credential. Neither sends an
inference request nor proves a running client has reloaded.

See the internal [acceptance record](../../../docs/OPENCODE_CONNECT_ACCEPTANCE.md)
for tested versions, actual tool-roundtrip evidence, and limits.
