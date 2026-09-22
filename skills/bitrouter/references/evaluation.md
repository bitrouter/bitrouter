# Typed evaluation (opt-in native host)

Stock `bro` serves generation and does **not** mount `/v1/evaluate` or link the
TypeSafe format. The separate `bro-typesafe` host explicitly registers the
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
