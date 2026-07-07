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

The validator (`cargo run -p dist-helper -- registry validate`) enforces:

- Model ids and provider model ids are lowercase `<org>/<model>`.
- Every id in `models/<vendor>.yaml` has org-prefix `<vendor>/`.
- `billing: subscription` providers carry **no** per-token pricing;
  `billing: usage_token` (the default) providers price **every** model they list.

Before submitting:

```sh
cargo run -p dist-helper -- registry validate   # advisories about non-curated
                                                 # provider models are expected
cargo run -p dist-helper -- registry build      # regenerate dist/registry
```

Commit `dist/registry/` alongside your source changes. The daily automated sync
refreshes provider catalogs from their `auto_sync` feeds and never touches the
curated `models/` files.
