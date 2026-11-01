# BitRouter Registry

The model/provider registry uses the tracked [Registry v1 beta protocol](../docs/specs/registry-v1/README.md). Recommendations and provider inventories have independent membership. Public model/provider artifacts are complete documents at `dist/registry/v1/registry.json`; the same-revision beta stream is in its revision subdirectory. Agent/runtime catalogs retain their independent contract and existing artifact paths.

## Registry v1 operations

The normative model/provider protocol is
[`docs/specs/registry-v1/README.md`](../docs/specs/registry-v1/README.md).
`registry build` and `registry validate` are deterministic offline contributor
checks; they do not claim a fresh OpenRouter audit. `registry sync` always fetches
the complete OpenRouter identity list and gates the complete candidate before
writing any source. `--report` writes JSON, including required-feed failures.

Prefer the provider's own model-list GET feed. Public lists use explicit
anonymous authentication; credentialed lists use their own named listing key
and account scope. A missing account-visible model cannot prove global
withdrawal. Native feeds use merge mode unless provider-authoritative complete
coverage has been established. New heterogeneous offers have empty interfaces
until endpoint support is verified; unknown prices stay absent.

The `models_dev` adapter is a merge-only directory fallback where a native
listing cannot be configured globally or requires interactive subscription
authentication unsupported by CI. It selects an explicit reviewed provider
key, excludes deprecated rows, and imports new offers as staging with empty
interfaces. It cannot establish provider lifecycle, execution, canonical
bindings or tariff authority. It preserves existing manual data. A missing or
rejected key for a usable native list is a credential problem, not grounds for
silently switching sources. Private native candidates can remain manual; a
declared feed must not receive a CI listing credential until its complete
observation and publication admission pass validation.

Feed authentication is independent of the provider's inference authentication.
Configure each listing according to its own endpoint contract. Examples:

| Feed | Listing authentication | CI secret |
| --- | --- | --- |
| [OpenAI](https://developers.openai.com/api/reference/resources/models/methods/list) | Bearer credential | `OPENAI_REGISTRY_API_KEY` |
| [Anthropic](https://platform.claude.com/docs/en/api/models/list) | `x-api-key`, with `anthropic-version: 2023-06-01` | `ANTHROPIC_REGISTRY_API_KEY` |
| [BitRouter](https://api.bitrouter.ai/v1/models) | Explicit `auth: {kind: none}` | None |
| [OpenRouter](https://openrouter.ai/docs/api/api-reference/models/list-all-models-and-their-properties) | Explicit `auth: {kind: none}` | None |

Trusted CI explicitly binds each configured listing variable to the GitHub
repository secret of the same name; local inference keys are not inherited.
When enabling a private feed, validate its complete observation and publication
admission before configuring that secret and its workflow environment binding.
Do not put credential values in feed URLs, command arguments, source, or reports.
Current requests and environment names are declared in `providers/*.yaml` and
`.github/workflows/registry-sync.yml`.
The independent OpenRouter identity request also uses `auth: {kind: none}`.
Its importer excludes `:batch` catalog variants in this release and creates chat
interfaces only when the upstream model explicitly advertises the chat endpoint.
The independent identity audit still reads the complete unfiltered listing.
Missing required secrets fail the run, while an independently validated public
feed can still produce a reviewable proposal after the global audit passes.
The workflow uploads the exact public identity response and JSON report.
Manual runs check out the selected commit, so a contributor can validate a
configuration fix on its branch before merging. Only runs selected from `main`
update `automation/registry-sync` and propose catalog changes; branch runs
perform the same synchronization and checks without publishing a proposal.

The v1 migration retained every native provider offer. Five delisted canonical
bindings (`qwen/qwen3.8-max`, `deepseek/deepseek-v3.1`, and Xiaomi's
`mimo-v2-flash`, `mimo-v2-omni`, `mimo-v2-pro`) are explicitly unbound;
historical correspondence remains in `model-identities.yaml`. This migration
is a reviewed source change, not an automatic sync rule. Azure now requires an
explicit concrete local endpoint; Bedrock's registry endpoint is the concrete
`us-east-1` default. Registry URLs do not interpolate environment templates.

Provider feed references:
- [OpenAI model listing](https://developers.openai.com/api/reference/resources/models/methods/list)
- [Anthropic model listing and cursor contract](https://platform.claude.com/docs/en/api/models/list)
- [OpenRouter complete listing](https://openrouter.ai/docs/api/api-reference/models/list-all-models-and-their-properties)
- [BitRouter API](https://docs.bitrouter.ai)
- [Google native model resources and pagination](https://ai.google.dev/api/models)
- [SiliconFlow authenticated model listing](https://docs.siliconflow.com/en/api-reference/models/get-model-list)
- [models.dev public directory API](https://github.com/anomalyco/models.dev#api)

### Agents and runtimes

An **agent** entry describes what is true about an ACP agent wherever it runs:
its ACP protocol version, how its LLM traffic can be redirected at the gateway,
and — when it has a native TUI — its `interactive_binary`. A **runtime** entry
describes one machine class and lists the agents it can run, each with the
invocation that starts it.

**Agent ids are bare** — `claude-acp`, not `anthropic/claude-acp`. The only
prefix an agent id ever carries is the runtime it is addressed through, so
`local/claude-acp` is the addressable form and `local/` elides. A vendor prefix
would make the two indistinguishable. Ids must be unique across the whole
`agents/` catalog; the filename is filing only, so unlike `registry/models`
there is no id/filename-stem rule.

**An addressable agent is never declared.** `local/opencode` exists because
`registry/runtimes/local.yaml` lists `opencode`, exactly as a routable model
exists because a provider lists it. A runtime **may list agents beyond the
curated catalog**; like non-curated provider models, those are *advisories*.

**Pin every invocation.** The registry is fetched over the network and names
commands BitRouter spawns with the user's privileges, so a floating tag
(`@latest`) means the fetched document chooses which code runs. Unpinned
package-runner specs are advisories today and become errors once an entry
carries a conformance record — a record has to name the `agent_version` it
exercised, which a floating tag cannot. A command that is **not** a package
runner must declare `requires_binary:`: the user installs it, and that
expectation should be stated rather than left to `$PATH`.

**Routing.** `routing.kind` is one of `env` (set variables on the child),
`args` (append config-override arguments), or `config_file` (write a config
into a per-launch directory and point the harness at it). Editing these changes
what a launched harness actually receives — `apps/bitrouter/build.rs` generates
the compiled catalog from `dist/registry/agents.json`, so no Rust change is
needed to add or re-route an agent.

A `config_file` entry carries a JSON `skeleton` plus knobs with **closed sets
of values**: `models.shape` (`map_of_empty` / `array_of_id` /
`array_of_profile`), `models.order` (`catalog_then_model` /
`model_then_catalog`), `default_model.format` (`bare` /
`provider_prefixed`), and `mcp.entry` (`opencode_typed` /
`command_args_or_url`). Nothing is evaluated — a registry entry selects among
behaviours reviewed in this repo, which is why a fetched catalog cannot
introduce new ones. See the [agent catalog profile](../docs/development/catalogs.md).

Placeholders are context-specific, because they resolve at different moments:
`{base_url_v1}` and `{auth}` in the skeleton's string leaves; `{dir}`,
`{file}` and `{auth}` in `env` values; `{default_model}` in `args`. `dir` and
`file` must be relative paths inside the per-launch directory. A key the
renderer fills — `models.at` above all — must already exist in the skeleton, in
the position the harness expects: values are replaced in place, and only new
keys are appended. The validator checks all of this.

**Conformance.** A runtime's agent entry may carry what the ACP-compatibility
suite observed for that (agent, runtime) pair:

```yaml
  - id: claude-acp
    transport: { … }
    conformance:
      acp_compat_1:
        handshake: pass       # the agent answers `initialize` on the declared
                              # ACP version
        routability: pass     # its LLM traffic reaches BitRouter when the
                              # entry's routing block is applied
        suite_version: 1.0.0
        agent_version: 0.70.0 # what the agent called itself, not what the
                              # invocation asked for
        measured_by: bitrouter
        as_of: 2026-09-06
```

Produce it with `bro agents conformance <runtime>/<harness>`, which
prints the block to paste. The suite needs **no provider credentials** — the
agent is launched with its own routing pointed at an ephemeral loopback gateway
that records what arrived — so it runs on a pull request. It does spawn the
agent, so the package or binary has to be installed.

Conformance is to an agent what pricing is to a model: a property of the pair,
which is why it lives here and not in `registry/agents/`. An agent can route
correctly in one runtime and fail in another.

What the validator enforces:

- **A tier that did not run is absent**, never `pass`. `skipped` is reserved
  for the suite deciding there was nothing to check (an own-auth harness has no
  routability to verify).
- **An active runtime may not serve an agent whose own record reports a
  failure.** Recording `handshake: fail` is how you document a broken agent
  without shipping it — put it under a `staging` runtime.
- **A third-party record must cite a `source_url`.** `measured_by: bitrouter`
  means we ran it; anything else needs to be checkable.
- **A record forces the pin.** An unpinned invocation is normally an advisory,
  but it becomes an error once a record exists: the record names an
  `agent_version`, and a floating tag cannot honestly supply one — whatever the
  suite ran is not what the next install fetches.

An entry with no record is an advisory, not an error; that is how a newly
contributed agent reads until someone runs the suite.

The **lifecycle** tier (`session/new` → prompt → cancel) is specified in
the catalog profile but is not implemented, so no record carries it.
