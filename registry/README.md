# BitRouter Registry

The source catalog of **models** and **providers** that BitRouter blesses by
default. It is generated into `dist/registry/{models,providers}.json` and
consumed by BitRouter — both the hosted service and the
[open-source distribution](https://github.com/bitrouter/bitrouter).

## What this is — and isn't

This is **not** a general-purpose database of every AI model, like
[models.dev](https://models.dev). It is a *curated* catalog with one purpose:
picking good defaults for **agentic and coding** workloads.

- **`models/`** — the canonical catalog: the models BitRouter supports by
  default.
- **`providers/`** — where those models (and more) can actually be served:
  first-party APIs, gateways, and coding-plan subscriptions, including
  **BYOK / BYO-subscription** providers that offer models *beyond* the curated
  set.
- **`agents/`** — the canonical catalog of **ACP-compatible coding agents**
  BitRouter drives and routes by default (Claude Code, Codex, opencode, …).
- **`runtimes/`** — where those agents actually execute. Today that is `local`
  (a child process on this machine); container and remote-sandbox runtimes are
  specified in `docs/AGENT_REGISTRY_SPEC.md` but not implemented.

`agents` / `runtimes` is the same relationship as `models` / `providers`: a
model is routable because an active **provider** serves it, and an agent is
launchable because an active **runtime** can run it.

Being listed in `models/` is an editorial decision. A model that is *not*
curated can still be served by any provider that lists it — it just isn't part
of the blessed default catalog.

## How models are curated

Default models are chosen by performance on **three independent benchmarks** —
independent meaning **not authored or curated by any model vendor**, so no
provider is grading a field it competes in:

- **Terminal-Bench 2.1** — from the **Laude Institute** and **Stanford** with the
  open-source Terminal-Bench community. 89 curated command-line agent tasks
  (software engineering, sysadmin, data processing, security); each runs in a
  Docker container against pytest checks with all-or-nothing scoring.
  [paper](https://arxiv.org/abs/2601.11868) ·
  [leaderboard](https://artificialanalysis.ai/evaluations/terminalbench-v2-1)
- **SkillsBench** — from **BenchFlow** (a research consortium spanning Stanford,
  CMU, Berkeley, and Oxford). ~86 tasks across 11 domains measuring how
  effectively an agent *uses skills* — the modular instructions, scripts, and
  resources it loads on demand. [site](https://www.skillsbench.ai/) ·
  [repo](https://github.com/benchflow-ai/skillsbench)
- **DeepSWE** — from **Datacurve**. 113 *original*, long-horizon software-
  engineering tasks across 91 repositories and 5 languages
  (TypeScript, Go, Python, JavaScript, Rust), written from scratch — not scraped
  from public pull requests — to resist training-data contamination and reward
  real problem-solving. [site](https://deepswe.datacurve.ai/) ·
  [repo](https://github.com/datacurve-ai/deep-swe)

We **deliberately do not** rank on benchmarks authored or curated by first-party
model providers — for example **SWE-bench Verified** (the subset verified by
OpenAI) and **GDPval** (OpenAI). When a model vendor curates a benchmark that
features its own models, independence is harder to guarantee, so we lean on
evaluations run by parties without a model of their own in the race.

## Relationship to BitRouter OSS

The registry is **fetched at runtime, not compiled in**. The build emits
`dist/registry/{providers,models}.json`; BitRouter pulls them over the network
and caches them under `$XDG_CACHE_HOME/bitrouter/`.

At runtime, a model is routable because a **configured provider serves it** —
`GET /v1/models` is the de-duplicated union of every active provider's models.
The curated `models/` catalog is the *default blessed set*, **not** a routing
gate. Two consequences:

- **You don't edit this catalog to use your own model.** An OSS user adds a
  model by configuring a **provider** in their local `bitrouter.yaml` (BYOK: an
  env-keyed provider that lists the model, or `auto_discover`). It becomes
  routable immediately, with no change here and no rebuild.
- **Editing this catalog is an upstream contribution** — it changes the blessed
  default catalog itself.

## Contributing

Source lives in two places; `dist/registry/` is generated — never hand-edit it.

- **`registry/models/<vendor>.yaml`** — one file per vendor, a YAML sequence of
  canonical models. Every id is `<vendor>/<model>` (lowercase). Include only
  facts you can verify (modalities, context/output limits, release date,
  `open_weights`); omit what you can't.
- **`registry/providers/<name>.yaml`** — one provider per file: the models it
  serves, transport, auth, pricing, and `billing`. A provider **may list models
  beyond the curated catalog** (BYOK / BYO-subscription extras) — those are
  allowed and surface as non-failing *advisories*, not errors.
- **`registry/agents/<vendor>.yaml`** — a YAML sequence of ACP agents. See
  *Agents and runtimes* below.
- **`registry/runtimes/<name>.yaml`** — one machine class per file: the agents
  it can run and how it invokes each.

### Model benchmarks

A model may carry a `benchmarks:` block recording independent-benchmark
results, keyed by benchmark. Today that is **Terminal-Bench 2.1**; the shape is
extensible so the other curation benchmarks can follow.

```yaml
- id: openai/gpt-5.6-sol
  # …other model fields…
  benchmarks:
    terminal_bench_2_1:
      accuracy: 88.0        # % of the 89 tasks passed (0–100)
      cost_per_task: 0.75   # USD, average per task
      time_per_task: 4.35   # minutes, average per task
      measured_by: bitrouter        # `bitrouter`, or a third-party source name
      harness: terminus-2           # agent harness the run used
      config: max                   # reasoning-effort / config label
      as_of: 2026-07-17             # snapshot date (YYYY-MM-DD)
      source_url: https://…         # required when `measured_by` is a third party
```

The three metrics are **optional and provenance-first**. A raw score is
meaningless on its own — the same model on the same benchmark version swings by
double digits across harness and reasoning-effort — so `harness` + `config` pin
a run for reproducibility, and `measured_by` (with `source_url`) keeps a cited
third-party number from being mistaken for one we ran. Per the "omit what you
can't verify" rule above, **leave the metrics unset until measured**: we fill
them from our own Terminal-Bench 2.1 runs (the open harness routed through
BitRouter, `measured_by: bitrouter`), not from unverified figures.

### Provider variants — one file per distinct endpoint

A provider file is **one routable endpoint with its own commercial terms**, not a
datacenter. A vendor gets more than one file only along two orthogonal axes:

- **Entity / region.** The default is *global* — **no suffix**. "International"
  is a commercial tier (USD, global signup), **not** a geography, so it never
  gets a suffix. Add a suffixed variant only when the vendor exposes a genuinely
  distinct public endpoint with distinct commercial or legal terms — a different
  base URL **and** a different currency, account/KYC, or data-residency
  jurisdiction. A different datacenter for the *same* product (latency only) is
  **not** a variant.
  - `_cn` — mainland China: separate legal entity, RMB, mainland real-name
    account, non-interchangeable keys. This is the one geographic split that is
    near-universal among Chinese vendors and always a distinct endpoint.
  - Other region suffixes (`_eu`, `_us`, `_apac`, …) are allowed **only** when
    such an endpoint really exists (e.g. a dedicated EU-residency host). Most
    providers will only ever have the default and maybe `_cn`. Do not reserve
    region slots the vendor doesn't offer, and do not split one product across
    per-city gateways (this is why Alibaba's endpoint-less `_hk`/`_jp`/`_eu`
    entries were removed).
- **Billing.** Independent of region: `usage_token` (the default) vs
  `subscription` (a flat-rate plan). Prefer the plan's real product name when it
  has one (`claude-code`, not `anthropic_coding_plan`); the `billing:` field
  carries the semantics regardless.

The name equals the filename stem and the env-var root (`{NAME}_API_KEY`); use
lowercase region codes. In `metadata`, `headquarters` is the home country of the
company behind the brand — **identical across all of that brand's variants** —
while `datacenters` is where the specific variant serves from (so a Chinese
vendor's international endpoint is `headquarters: CN` with `datacenters: [SG]`,
not `headquarters: SG`).

### Pricing and sourcing

- **All pricing is USD per 1M tokens.** Convert any RMB (or other-currency) rate
  to USD and record the source and conversion in a comment — never leave a
  non-USD number in a price field. (`usage_token` providers must price every
  model; `subscription` providers price none — see the validator rules below.)
- **Prefer the models.dev feed when the provider is listed there.** Set
  `auto_sync: { feed: models_dev }` so the daily sync keeps the catalog and
  pricing current. The sync key defaults to the provider `name`; set `key:`
  explicitly when the models.dev key differs (e.g. `siliconflow_cn` uses
  `key: siliconflow-cn`). Providers absent from models.dev are maintained by
  hand from the vendor's published model + price list, and the comment header
  should say so.

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
introduce new ones. See `docs/AGENT_REGISTRY_SPEC.md` §7 and D4.

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

Produce it with `bitrouter agents conformance <runtime>/<harness>`, which
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
`docs/AGENT_REGISTRY_SPEC.md` §9 but not implemented, so no record carries it.

### Status lifecycle

`status` gates routing: **only `active` is served**; `staging`, `suspended`, and
`withdrawn` are not. Use `staging` for a provider scaffolded from research but not
yet confirmed against the live API — keep a `# VERIFY BEFORE ACTIVATING` header
listing the exact fields a human must check (model ids via `GET /v1/models`,
prices, env var, base URL) before flipping it to `active`.

The validator (`cargo run -p dist-helper -- registry validate`) enforces:

- Model ids and provider model ids are lowercase `<org>/<model>`.
- Every id in `models/<vendor>.yaml` has org-prefix `<vendor>/`.
- `billing: subscription` providers carry **no** per-token pricing;
  `billing: usage_token` (the default) providers price **every** model they list.

Before submitting:

```sh
cargo run -p dist-helper -- registry validate   # advisories about non-curated
                                                 # provider models and unpinned
                                                 # agent invocations are expected
cargo run -p dist-helper -- registry build      # regenerate dist/registry
```

The docs site's `supported-*` tables are generated in the **bitrouter-docs**
repo from the committed `dist/registry/` artifacts — there is no `registry docs`
step here.

Commit `dist/registry/` alongside your source changes. The daily automated sync
refreshes provider catalogs from their `auto_sync` feeds and never touches the
curated `models/` files.
