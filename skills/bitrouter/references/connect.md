# Persistent native model options

`bro connect` presents a Codex / Claude menu, previews files and effects, and
asks once before connecting the detected clients. It works in gateway-only
builds (`--no-default-features`); neither `harness` nor `core` is required.
`--model ID` selects an exact gateway model ID. New connections default to
`bitrouter/automatos`. `--display-name LABEL` separates the picker label from
the request model ID. It never creates a gateway alias. Setup configures clients; it does not implement routing.

```sh
bro connect
bro connect codex --dry-run
bro connect claude --yes
bro connect codex --base-url https://gateway.example/proxy/v1 --model team/coder
bro connect claude --base-url https://gateway.example \
  --model bitrouter/claude-automatos --display-name bitrouter/automatos
bro connect codex --remove
```

- `--config PATH` selects the local daemon. The command starts it if needed after
  confirmation. It does not restart an existing daemon.
- `--base-url URL` targets an existing gateway instead, without reading local
  BitRouter configuration or starting a daemon. It conflicts with `--config`.
  Gateway roots and `/v1` URLs, including reverse-proxy path prefixes, are
  accepted. Explicit gateways require `BITROUTER_API_KEY`; remote URLs require
  HTTPS. HTTP is permitted only on loopback.
- `--model ID` is checked against the target gateway's `/v1/models` catalog.
  One connection per client family is managed at a time. Omit the flag on repeat
  setup to retain the recorded model; a different model, explicit display name or endpoint requires
  removal and reconnection. `--remove` uses the receipt, not a model argument.
- Local credential-less setup requires loopback and `skip_auth: true` in the
  selected config. No upstream provider credential is borrowed.
- `--codex-catalog PATH` supplies a complete Codex ModelsResponse, not an OpenAI
  model list. Otherwise the current catalog or models_cache.json is used, with
  the installed Codex bundled catalog as a fallback for a fresh installation.
- `--dry-run` neither starts a daemon nor writes configuration or receipts.
- Noninteractive writes/restores require an explicit target and `--yes`.
- Named remote contexts are rejected: these are local client-file operations.

Both connect and restore require a full client restart. The command never quits
an application, changes tool permissions, or submits an inference request.
It checks that the gateway advertises the exact model before writing clients.
`gateway_catalog_verified` is not inference validation; `inference_verified`
remains false. A preview has state `preview`, a write has
`configured_restart_required`, and repeat setup has
`already_configured_restart_required`.

## Codex

CLI and desktop normally share CODEX_HOME / ~/.codex/config.toml. The command
adds a dedicated model_providers.bitrouter-connect Responses provider and
a complete local model_catalog_json (`bitrouter-models.json`). The existing default, comments, unrelated
settings and model metadata are preserved. Imported models are made API-visible
so they remain selectable under gateway authentication.

All selected models now pass through BitRouter. The x-bitrouter-harness header
retains native model-to-provider mapping. If the selected model already appears
in the source catalog, its capabilities are retained and it is made visible,
without a duplicate entry. Otherwise setup supplies a generic text/tool template
with 64K context and compaction at 48K; these are client configuration defaults,
not verified model limits. The template advertises no reasoning levels, native
web/image/audio tools, code-mode, service tiers or multi-agent backend. Use
`--codex-catalog` with a complete catalog containing the selected model to supply
its actual context limits and capabilities. A `/v1/models` ID alone does not
prove those capabilities or tool-call compatibility.
Desktop can still attach native namespace and web-search tool declarations.
Linux Desktop 26.1027.30824 was verified against a native Responses upstream;
its tool-bearing requests were refused by the gateway's lossless conversion
checks against a Chat Completions upstream. A picker entry alone does not
establish compatibility with arbitrary Chat Completions models.
An active profile overriding provider/catalog is rejected. Project and managed
settings can still take precedence. The fallback runs `codex debug models
--bundled` without authentication/network access. Older clients without this
command need an existing cache or an explicit catalog.

A local catalog is a snapshot. After upgrading Codex or changing available
models, remove and reconnect with a current source catalog. CLI and desktop
using different custom homes require separate configuration.

## Claude

CLI settings use CLAUDE_CONFIG_DIR / ~/.claude/settings.json and the
ANTHROPIC_CUSTOM_MODEL_OPTION, _NAME and _DESCRIPTION variables. Existing
default models and permissions stay intact. The upstream API key field is
cleared and replaced with a gateway bearer token. Conflicting credential helpers,
provider-selection variables, custom headers and shell overrides are rejected.

Desktop does not inherit CLI inference settings. A detected desktop gets a
separate local Claude-3p/configLibrary BitRouter profile, selected through
_meta.json, and deploymentMode: "3p". The schema was checked against Desktop
4.4212.0; the implementation does not install or upgrade it. Existing profile model
entries are retained and the alias is appended. With no explicit model list,
this profile contains only the selected model. Previous profiles remain intact.
Gateway mode does not support Anthropic cloud environments or Remote Control.
Managed and bootstrap configurations are rejected. Standard application
locations or an existing config library (also under CLAUDE_USER_DATA_DIR) are
used for detection; an absent
desktop is reported as skipped.

Desktop filters model IDs that are not recognizably Claude, including the default
`bitrouter/automatos`. For Desktop, select a gateway-advertised Claude route such
as `bitrouter/claude-automatos` and set `--display-name bitrouter/automatos`.
`labelOverride` changes the label, not the request model or capability checks.
The gateway must implement that route; setup does not silently rename it or
certify non-Claude backends. CLI does not inherit this Desktop name restriction.

The documented Claude gateway contract is Anthropic Messages, including SSE
and tool use. Validate the actual routed models against that contract before
claiming inference compatibility. Adding an option does not certify non-Claude
backends or provider-specific features.

## Recovery and ownership

Private receipts live in $BITROUTER_HOME/connections or ~/.bitrouter/connections.
They include original file bytes and gateway credentials; do not publish them.
Files are replaced atomically, receipts precede writes, and symlinks/multiply
linked files are refused. Unix writes use mode 0600.

`--remove` restores unchanged files exactly. Codex config recovery restores the
owned provider and catalog pointers while preserving other settings written by
the app. If the picker still selects the connected model, it restores the prior
default model and reasoning effort; a later selection of another model is kept.
Edits to the owned provider or catalog pointer remain conflicts. The catalog
file and Claude CLI settings retain exact-file conflict checks.
Claude Desktop restoration is
scoped: it removes only the owned profile/list entry, restores the previous
selection only while the BitRouter profile is selected, and preserves unrelated
preferences and later profile selections. Desktop's `__sealed__:` credential
migration is accepted; other edits to the owned profile or its list entry are
conflicts. Resolve those before retrying; validation precedes all restore writes.
If another profile was selected, its deployment mode stays unchanged. Interrupted
transactions remain recoverable. Repeat connect accepts the supported Desktop
normalizations but rejects a changed profile selection. Fully quit the client
before connect/remove to avoid concurrent writers.

Version-4 receipts add Codex config ownership rules. Version-3 receipts record
the endpoint, request model, optional display name and Claude Desktop ownership
rules. Earlier receipts remain readable with their original recovery behavior;
existing Codex exact-file receipts do not silently acquire relaxed checks.
To rotate a key, change endpoints, refresh
a catalog or include a newly installed client, remove and reconnect.

With inherit_defaults: true, a canonical auto router also supplies the default
automatos router using the same policy/defaults/checks. An explicit
routers.automatos is preserved. No implicit alias is added to legacy-only
presets or configs opting out of defaults. The gateway must publish
bitrouter/automatos before connection proceeds.

Sources:
- https://learn.chatgpt.com/docs/model-catalogs
- https://learn.chatgpt.com/docs/enterprise/roll-out-a-gateway
- https://code.claude.com/docs/en/model-config#add-a-custom-model-option
- https://claude.com/docs/third-party/claude-desktop/gateway
- https://claude.com/docs/third-party/claude-desktop/configuration-format
