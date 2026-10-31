# Codex Desktop connection acceptance — 2026-10-31

Executed in a disposable Runtime Cloud Ubuntu 24.04.5 x86_64 VM, using the
official Linux desktop package 26.1027.30824 and bundled Codex backend
0.166.0-alpha.5.1. BitRouter was built from the PR source with no default
features. The local macOS desktop was not restarted or reconfigured.

The upstream was a deterministic protocol fixture, not a paid or production
model. The native desktop application and its actual file-reading tool were
used without patching the application. The desktop was configured for the
gateway before launch; no ChatGPT account credentials were copied into the VM.

## Native acceptance

```
bro connect codex --base-url http://127.0.0.1:50392 \
  --model bitrouter/automatos --display-name bitrouter/automatos \
  --codex-catalog /workspace/bundled-models.json --yes
```

The catalog was obtained from the installed desktop backend's
`debug models --bundled` command. A dedicated fixture project contained only
`acceptance.txt` with the marker `BITROUTER_CLOUD_FILE_PROOF_20261031`.

- The native model picker displayed and selected `bitrouter/automatos`.
- The desktop sent streamed `/v1/responses` requests with that model ID.
- Through BitRouter and a native Responses upstream, an `exec_command` call
  ran `cat acceptance.txt` in the fixture project. The upstream returned the
  final success marker only after observing the actual tool output.
- A second fresh conversation repeated the successful file-read round trip.
- Fully terminating and reopening the desktop retained the model selection.
- Repeated the picker, actual tool round trip, and full restart with a fresh
  desktop profile and a version-4 receipt after the scoped-recovery fix.
- Repeated setup returned `already_configured_restart_required` after real
  desktop writes. Removal returned `restored`, deleted the owned model catalog
  and receipt, removed the connected default model/provider pointers, and
  retained desktop, plugin and MCP settings written by the application.
- The same bundled native CLI completed streamed text and actual
  file-read tool requests through the gateway with `codex exec` in an isolated
  cloud home. The CLI tool run used the rebuilt gateway after the recovery fix.

## Findings and limits

An initial Chat Completions fixture was refused with
`model_conversion_incompatible`. The desktop included native namespace and
web-search declarations even with conservative catalog metadata. Those
declarations cannot be silently dropped by the gateway's lossless conversion
rules. Switching the test upstream to native Responses succeeded. This test
does not establish arbitrary Chat Completions backend compatibility.

Initial version-3 exact-file recovery also refused ordinary desktop changes to
`config.toml`. Version-4 receipts now scope ownership to the provider and catalog
pointers, preserve unrelated desktop settings, and restore the old default
model/effort only when the connected model remains selected. Changed provider
fields remain conflicts. Older receipts retain their original recovery rules.

Real upstream inference quality, production credentials and billing, other
operating-system GUI behavior, and all native auxiliary model routes are outside
this acceptance. A successful picker entry alone does not establish these.

Sources:
- https://learn.chatgpt.com/docs/linux/linux-app
- https://withruntime.com/llms.txt
- https://withruntime.com/docs/javascript.md
