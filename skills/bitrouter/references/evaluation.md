# Typed evaluation

Default `bro` serves `/v1/evaluate` and compiles the native TypeSafe provider
extension. Jev becomes routable only when a TypeSafe account is active and
credentialed. The public endpoint remains available without an evaluation
provider and returns `evaluation_model_not_found` for a valid but unavailable
model. System One JSON is private to the TypeSafe extension, not a separate
format binding in configuration.

From this repository, start the normal host with a config file:

```bash
TYPESAFE_API_KEY=... cargo run -p bitrouter --bin bro -- serve --config /absolute/path/bitrouter.yaml
```

For an installed CLI, use `bro serve` or `bro start` with the same
`TYPESAFE_API_KEY` environment. The host uses the configured `server.listen`
(normally `127.0.0.1:4356`). The fixed `typesafe/jev-1.13` model is declared
by the provider extension; the public registry only enriches metadata. It
remains usable when registry fetching is disabled. An explicitly configured
active route with a conflicting model, wire id, or endpoint fails startup.
The moving `jev-latest` alias is not listed.

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
