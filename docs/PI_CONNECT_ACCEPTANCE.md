# Pi CLI/TUI persistent connector acceptance

Date: 2026-10-31. Scope: persistent `bro connect pi` configuration for Pi CLI/TUI.

## Configuration contract

- Agent-directory precedence: explicit `--instance`, `PI_CODING_AGENT_DIR`,
  `~/.pi/agent`; `~/` expansion matches Pi. SDK `agentDir` cannot be discovered
  from another process and must be supplied as the explicit instance.
- `models.json` gains an independent `providers.bitrouter` with the exact
  gateway model ID, display/detail name, protocol, base URL, bearer-auth setting
  and `x-bitrouter-harness: pi-acp`. Existing providers are retained.
- `auth.json` stores a private `bitrouter` API-key credential. Literal `$` and
  leading `!` are escaped for current Pi's configuration-value resolver.
- `settings.json` retains existing bytes and startup defaults at connect. An
  absent file becomes an empty object so the original selection can be tracked.
  Removal restores only a saved default pair that still selects the connected
  BitRouter model. Later native defaults and unrelated edits survive.
- Provider/auth edits under the owned `bitrouter` keys are conflicts. Recovery
  validates the transaction before removing any owned data; native additions,
  credentials and preferences remain outside that ownership.

The connector omits context/output limits, cost and capability declarations.
Pi's displayed fallback metadata is not an upstream guarantee. A picker entry
and `/v1/models` catalog check do not certify inference. Reports retain
`inference_verified: false`.

## Initial direct-fixture native acceptance

Installed client: `@earendil-works/pi-coding-agent` **1.2.0**, Node **22.22.3**,
macOS arm64. All activity used a fresh test agent directory, a controlled test
project and dummy credentials. The user's Pi agent directory was untouched.

Setup and removal used the actual Rust `connect/pi.rs` and transaction
`connect/files.rs` through a small adapter executable. Unrelated client
adapters were stubbed in that executable; no result here certifies their
recovery behavior. The normal `bro` command is separately covered by
`apps/bitrouter/tests/connect_pi.rs`.

1. Seeded one authenticated native provider and a native startup default.
   Connected BitRouter with a persisted credential; native defaults retained.
2. Opened the installed Pi CLI/TUI in a PTY. `/model` displayed the retained
   native entry and `bitrouter/automatos [bitrouter]`. Selected BitRouter in the
   native picker.
3. Submitted a prompt to a deterministic local OpenAI Chat Completions SSE
   fixture. Both requests carried the exact `bitrouter/automatos` wire model,
   the expected bearer credential, `stream: true` and the harness header.
4. The first response streamed text and a `read` call. Pi executed its real
   built-in read tool against the controlled project file. The second request
   contained that file's marker. Pi displayed
   `PI_TOOL_ROUNDTRIP_OK: PI_READ_TOOL_ACCEPTANCE_20261031`.
5. Reopened `/model`; the connected option remained singular. `Ctrl+S` saved
   the connected model as the startup default. After unrelated preference edits,
   status remained installed; removal restored the original native default and
   retained those edits. The native model list then reported no BitRouter match.
6. Reconnected in a second cycle while an isolated Pi TUI was open. `/model`
   reloaded and showed the newly added BitRouter provider plus a later native
   model addition. Saved that native model through `Ctrl+S`. Removal retained
   both the later native default and native model addition.

Evidence artifacts delivered with the task:

- `pi-native-tui.log`: native picker, selection, streaming, read-tool result,
  repeat picker and connected-default save; includes terminal control sequences.
- `pi-native-later-choice-tui.log`: live model reload and later native default.
- `pi-native-wire.json`: sanitized request assertions and tool-result marker.
- `pi-native-preservation.json`: default/provider/credential preservation checks.
- `pi-adapter-tests.log`: four Pi adapter tests using the real transaction code.

## Native Pi through the actual BitRouter gateway

A second acceptance ran on the shared Linux x64 test VM with the current npm
package `@earendil-works/pi-coding-agent` **1.4.0**, Node **24.21.0**, and the
built `bro` executable. This is separate from the earlier direct-fixture PTY
exercise above.

The actual `bro connect pi --base-url http://127.0.0.1:50412 --yes` command
configured an isolated agent directory. Native Pi then ran in JSON print mode
with `--provider bitrouter --model bitrouter/automatos --tools read`, using that
directory and **without `BITROUTER_API_KEY` in the Pi process environment**.
The full path was:

```text
native Pi -> capture proxy :50412 -> bro serve :50411 -> fixture :50410
```

Both client requests carried `model: bitrouter/automatos`, `stream: true`, the
expected persisted bearer header and `x-bitrouter-harness: pi-acp`. BitRouter
routed both requests to `fixture:test-model`. Native Pi emitted `text_delta`,
`tool_execution_start` for `read`, and a successful `tool_execution_end`. The
second upstream request contained the controlled file's marker; native output
ended with `PI_BRO_GATEWAY_TOOL_ROUNDTRIP_OK`.

The test gateway used `skip_auth: true`; this proves that the persisted client
bearer header was sent, not that gateway credential enforcement was exercised.
The configured upstream bearer header was also observed. The upstream remained
a deterministic SSE fixture, with no live model inference.

Using the latest rebuilt `bro`, the same isolated instance then passed:

- Offline `--status` without a shell key, repeat connect, and `--check` with a
  deliberately incorrect shell key while the saved credential was used.
- `--update` without a shell key, retaining its saved credential. The connected
  saved default safely returned to the prior native default.
- `--remove` preserving later native default/model/credential/preference edits;
  native Pi's model list then confirmed the connected model was absent.

All setup/lifecycle reports kept `inference_verified: false`. The three
`connect_pi` and four shared `connect_native` CLI integration tests also passed
on the cloud build.

Additional delivered evidence: `pi-bro-gateway-summary.json`,
`pi-bro-gateway-wire.json`, `pi-bro-native-events.jsonl`,
`pi-bro-native-events-summary.json`, `pi-bro-connect-report.json`,
`pi-bro-lifecycle.json`, and `pi-bro-gateway.log`. Test driver/fixture sources
are provided as `pi-bro-gateway-*.cjs` and `pi-bro-lifecycle-run.cjs`.

## Automated coverage

The Pi adapter tests cover independent provider/auth setup, exact wire metadata,
all three protocol base paths, exact restoration, scoped unrelated edits,
later default choices, connected-default recovery, literal token escaping,
invalid profiles, literal-only saved-credential decoding and atomic refusal on
owned credential edits.

The `connect_pi` integration target covers the real `bro` CLI: environment and
explicit agent directories, repeat configuration/status, independent instances,
catalog-only validation, no-write previews/missing models, preserved native
edits, connected-default cleanup and owned-edit conflicts. Run it with:

```sh
cargo test -p bitrouter --no-default-features --test connect_pi
cargo test -p bitrouter --no-default-features --test connect_native
```

The shared lifecycle target checks offline status, normalized instance identity,
checking with the saved credential, catalog/builder replacement failures, and
resumption after an interrupted rollback. Successful updates restore a saved
BitRouter default to its former native default; select BitRouter again after
updating if desired.

## Limits and version boundary

The native stream/tool exercise used a deterministic protocol fixture. It proves
Pi's actual model selection, persisted authentication, SSE handling and built-in
read-tool roundtrip against that fixture. It does not establish a live upstream
model's tool capability, context capacity, price, reasoning or reliability.
Native Responses and Messages inference were not exercised here; configuration
paths for those protocols are covered by adapter tests.

Pi picker rows use model IDs. A different `--model` remains visible under its
own exact ID even when its detail name is `bitrouter/automatos`; a name cannot
silently alias the wire model. Fully qualified Pi IDs include the provider
prefix, so the default can appear as `bitrouter/bitrouter/automatos` in default
confirmation messages while its request model remains `bitrouter/automatos`.

Accepted clients are 1.2.0 for the PTY picker exercise and 1.4.0 for the actual
gateway CLI exercise. Legacy `@mariozechner/pi-coding-agent` releases were not
native-tested. Their configuration-value resolver differed; compatibility must
not be inferred from the shared `pi` command name.

Primary references:

- [Current model setup and auth precedence](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/models.md)
- [Agent-directory configuration](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/configuration.md)
- [Model metadata defaults](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/models-configuration.md)
- [Legacy 0.58 value resolution](https://github.com/earendil-works/pi/blob/v0.58.0/packages/coding-agent/src/core/resolve-config-value.ts)
