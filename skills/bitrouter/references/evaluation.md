# Typed evaluation (opt-in native hosts)

Stock `bro` serves generation and does **not** mount `/v1/evaluate` or link a
typed-evaluation format. The separate `bro-typesafe` host explicitly registers the
native `typesafe/system_one@1` Rust extension and retains normal BitRouter
generation routes. `TYPESAFE_API_KEY` alone does not enable Jev in stock `bro`.

From this repository, start the opt-in host with a config file:

```bash
TYPESAFE_API_KEY=... cargo run -p bitrouter-typesafe-host -- --config /absolute/path/bitrouter.yaml
```

The host uses the configured `server.listen` (normally `0.0.0.0:4356`); pass
`--listen 127.0.0.1:4356` to override it. A public registry merge activates
the fixed `typesafe/jev-1.13` route only when the key and exact registered
extension are available. An explicitly configured active route with a missing
or wrong-revision extension fails startup. The moving `jev-latest` alias is not
listed.

Send a non-streaming request to `/v1/evaluate`:

```json
{
  "model": "typesafe/jev-1.13",
  "state": {"ticket": "Payment posted twice"},
  "questions": {
    "urgent": {"type": "noul", "instructions": "Needs urgent handling?"},
    "team": {
      "type": "choice",
      "instructions": "Which team?",
      "criteria": {"billing": null, "technical": null}
    }
  }
}
```

The selector can be pinned as `typesafe:typesafe/jev-1.13`. The response
contains the provider-reported model version, typed answers, provider ID, and
usage/cost evidence. `GET /v1/models` advertises `operations: ["evaluate"]`
only for active executable routes. Unknown top-level request fields are
ignored; they do not select a gateway provider or control tracing/streaming.
There is no inbound `/v1/systemone` TypeSafe SDK compatibility route: that
path is used only for the outbound TypeSafe call.

An evaluation-only selector sent to a generation endpoint, or a generation-only
selector sent to `/v1/evaluate`, fails with `model_operation_mismatch` before
upstream dispatch. With `server.skip_auth: false`, `/v1/evaluate` uses the same
`brvk_` virtual-key validation as generation. Evaluation does not run generation
prompt transforms or tools. Do not treat confidence as authorization for an
external action; the caller owns thresholds and fallbacks.

## Local Laya Typed Decisions (Phase 3)

The `bro-laya` custom host registers `laya/local_system_one@1`. It uses the
same public `/v1/evaluate` JSON shape, but a separate Python process performs
inference from the exact `convaiinnovations/laya-typed-decisions` snapshot
`f9ab0b228f0fc0f14d873dbc99038f135c2da1b2`. This local provider is **not
auto-added from the registry**: set it up explicitly. Stock `bro` cannot run
this evaluation, and the TypeSafe key does not authenticate the local process.

Install `laya==0.3.6` in a dedicated Python environment. The checkpoint is
downloaded on first sidecar startup; set `HF_HOME` to a cache location if you
want to retain it. After caching, `LAYA_OFFLINE=1` prevents network downloads.
Use a unique, randomly generated `LAYA_LOCAL_TOKEN` of at least 16 characters
in **both** processes, without putting it in YAML or command arguments. Start
the sidecar first and wait for `GET http://127.0.0.1:8767/health` to return
`status: ready`; it binds only to loopback.

```bash
LAYA_LOCAL_TOKEN="<generated-local-secret>" python3 crates/bitrouter-laya-extension/local_provider/server.py --port 8767
```

The matching `bitrouter.yaml` provider block is:

```yaml
providers:
  laya:
    api_base: http://127.0.0.1:8767
    api_key: ${LAYA_LOCAL_TOKEN}
    operations:
      evaluate:
        endpoint: /v1/decisions
        format: {extension: laya, adapter: local_system_one, revision: 1}
    models:
      - id: laya/typed-decisions-f9ab0b2
        provider_model_id: convaiinnovations/laya-typed-decisions@f9ab0b228f0fc0f14d873dbc99038f135c2da1b2
        operations:
          evaluate:
            question_types: [noul, choice, score]
            max_choice_options: 20
            max_score_levels: 10
```

With the same token exported, run `cargo run -p bitrouter-laya-host -- --config
/absolute/path/bitrouter.yaml` and select either
`laya/typed-decisions-f9ab0b2` or
`laya:laya/typed-decisions-f9ab0b2` in `/v1/evaluate`. The extension and local
process accept at most 16 questions per call and a 1 MiB sidecar body. This is
a provider-specific resource bound, not a new global evaluation limit. The
reported `model` is the full pinned snapshot; `usage` comes from Laya and
currently has no cost evidence, so BitRouter leaves cost unknown. Laya's
auxiliary action head is deliberately not exposed. There is no automatic
cross-provider fallback for a pinned selector.
