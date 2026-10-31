# Claude Desktop connection acceptance — 2026-10-31

Tested on macOS arm64 with Claude Desktop 4.4212.0 and a gateway-only BitRouter
build. The upstream was a deterministic local OpenAI Chat Completions fixture,
not a paid or production model. The actual Desktop app and its Code Read tool
were used; no app bundle was patched.

## Configuration and observed behavior

```
bro connect claude --base-url http://127.0.0.1:50392 \
  --model bitrouter/claude-automatos --display-name bitrouter/automatos --yes
```

The local gateway explicitly advertised and implemented the request route.
`--display-name` did not create a gateway alias. Both Code and Cowork displayed
`bitrouter/automatos`; captured Messages requests sent
`bitrouter/claude-automatos`. The formerly used request ID
`bitrouter/automatos` was rejected by Desktop's model-name validation.

- Code: selected a dedicated fixture folder, sent a test request, received a
  Read tool call, read `acceptance.txt`, and returned its marker to the gateway.
  The fixture produced a final response only after seeing that tool result.
- Cowork: displayed the same model label and returned a streamed fixture reply.
  Workspace setup also showed "Not enough disk space" before and after retry.
  The VM installer log required 16.1 GB free and observed 11.1–11.9 GB.
  Cowork sandbox tool execution is therefore **not accepted** by this run.
- Fully quit using the app menu, reopened, and observed the same model option.
- Repeated `connect` succeeded after Desktop sealed the gateway key and updated
  its application preferences.
- Fully quit and ran `connect claude --remove --yes`. Restoration succeeded
  without editing snapshots or current files. The owned profile and receipt
  were removed; unrelated new preferences remained. Reopening showed the
  original Claude account interface and original native model picker.
- No Claude subscription, recharge, paid inference, or upstream account
  credentials were used in this test.

## Scope and remaining acceptance

These results establish local configuration, branding, wire identity, Code tool
round-trip, restart persistence, and scoped restoration. They do not establish
compatibility with real Claude inference or arbitrary non-Claude backends,
production routing, billing, reasoning, context limits, or Cowork sandbox tools.
Claude Desktop's official gateway contract targets Claude models. A recognizable
alias passing a client name filter is not evidence of backend compatibility.
The Desktop interface in this build exposes Cowork and Code navigation; a
separate standard-account Chat surface was not switched to a gateway.

Before production release, run an actual supported Claude upstream with usable
API quota, verify the chosen model's limits and tool behavior, and repeat Cowork
sandbox acceptance on a machine with sufficient disk space.

Sources:
- https://claude.com/docs/third-party/claude-desktop/overview
- https://claude.com/docs/third-party/claude-desktop/gateway
- https://claude.com/docs/third-party/claude-desktop/models
