---
type: removed
breaking: true
title: "The origin MCP server's `complete` tool is removed"
pr: 875
---

MCP is BitRouter's control and introspection surface; running a completion is
what the daemon's HTTP inference API is for. Use it instead — it is the
transport built for inference, and it always had the surface the tool did not:
streaming, the full parameter set, and the metering path.

```jsonc
// before — MCP tools/call
{ "name": "complete",
  "arguments": { "model": "openai/gpt-4o",
                 "messages": [{ "role": "user", "content": "hi" }] } }
```

```bash
# after — the daemon's HTTP API (OpenAI-shaped; /v1/messages is the
# Anthropic-shaped twin)
curl http://127.0.0.1:4356/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"model":"openai/gpt-4o","messages":[{"role":"user","content":"hi"}]}'
```

What remains on `bitrouter mcp serve` is `list_models`, `status`,
`route_preview` and the skills pair: what is routable, where it would go, and
what it has cost. The stdio profile is now `list_models` + `status` +
`route_preview` + `skills_search` + `skills_get`; the HTTP profile is
`list_models`, plus `status` where the backend can answer it.

The one-line spend footer went with it. It only ever rode successful `complete`
results; `status` reports the same figure as typed structured content under
`spend`, from the same metering database, with `unpriced` and the remaining cap
a footer had no room for.

**Rust API:** `bitrouter_mcp::backend::Backend::complete` and the
`CompleteRequest` / `CompleteResponse` / `Usage` types are gone; `Backend` is now
a pure port-handover trait (`status_port` + `models_port`). Also removed:
`server::Builder::completion`, `server::CostFooter`,
`BitrouterMcp::with_cost_footer`, `server::CompleteArgs`, and
`ServeOptions::cost_footer`. `server::serve_stdio` loses its `cost_footer`
parameter and now takes the handler alone. `serve_http_on`, `CloudBackend` and
`CloudAuth` are unchanged.
