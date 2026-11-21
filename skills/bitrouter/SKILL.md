---
name: bitrouter
description: >
  Use when installing, configuring, running, or troubleshooting BitRouter from
  its CLI — a self-hosted LLM proxy on 127.0.0.1:4356 routing OpenAI- or
  Anthropic-shaped traffic to any provider, via a coding-agent subscription,
  hosted BitRouter, or your own keys. Covers gateway setup, provider
  credentials, MCP configuration, local BRO tasks, ACP prompts, routing, and spend.
  Trigger on bitrouter.yaml, port 4356, brk_ keys, "replace litellm", or
  pointing a coding agent at a proxy.
license: Apache-2.0
metadata:
  author: BitRouterAI
  tags: [llm, proxy, routing, cli, ai-gateway, claude-code, codex]
---

# BitRouter and BRO

Use `bitrouter` on the gateway host to manage providers, tenants, policy revisions and Eval. Use `bro` on a developer machine for native tasks, ACP and local thread history. Run each executable's `build-info` to identify an installation. Use `bitrouter update` for the installed service and `bro update` for BRO. Restart the gateway manually after an update. See [release channels and updates](references/updating.md).

## Configure the gateway

1. In a source checkout install the service with `cargo install --locked --path apps/bitrouter`. The minimal service uses `--no-default-features`. Use the release installer or archive for the service package when available.
2. Set provider credentials in the service environment. Examples include `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, `OPENROUTER_API_KEY`, and `OPENCODE_ZEN_API_KEY`. Use `bitrouter providers list` to see the actual configuration.
3. Run `bitrouter init --yes --use-detected --after exit` to scaffold a configuration. Start the foreground process with `bitrouter serve` or select an explicit file with `--config PATH`. The default address is `127.0.0.1:4356`.
4. For authenticated access mint a tenant-scoped virtual key or use the operator's existing client credentials. Configure an explicit authentication policy before exposing a listener outside loopback.
5. Point a compatible model client at `http://127.0.0.1:4356/v1`. Use `bitrouter/auto` after configuring an `auto` router and Policy, or select a permitted model from `bitrouter models`.

Use `bitrouter policy` for routing-policy source and publication. Use `bitrouter access-policy` for access control. An evaluator may submit outcomes and freeze evidence through the SDK. Activation of a new tenant revision requires an authorized, explicit operation. See [remote administration](references/remote-administration.md) and [provider setup](references/providers.md).

## Run a local task

Install BRO separately with `cargo install --locked --path apps/bro` from source. Set `BITROUTER_ENDPOINT` to the gateway origin and `BITROUTER_API_KEY` to its client bearer token. The default origin is `http://127.0.0.1:4356`. BRO stores its own state under `BRO_HOME` or `~/.bro`.

```sh
bro task run 'Summarize this project' --workspace . --read-only --model provider/model
bro thread list
bro thread history THREAD_ID
bro thread pending THREAD_ID
bro thread answer THREAD_ID --turn TURN_ID --request REQUEST_ID allow
bro thread resume THREAD_ID --turn TURN_ID
bro thread cancel THREAD_ID --turn TURN_ID
```

Replace `provider/model` with a model shown by `bitrouter models`. Use the IDs returned in the event stream. Start a new turn on an idle thread with `bro thread start THREAD_ID 'next instruction'`. If recovery reports an unresolved model request or tool effect, inspect the journal before taking further action. A lost client connection can leave inference usage unknown.

Create a child with `bro agent create PARENT_UUID CHILD_UUID`. Choose the child UUID once and reuse it on retry. Queue a turn with `bro agent delegate PARENT_UUID CHILD_UUID --request-id KEY 'task'`. Use the returned turn ID with `bro agent wake PARENT_UUID CHILD_UUID TURN_ID`, `bro agent wait PARENT_UUID CHILD_UUID TURN_ID` or `bro agent cancel PARENT_UUID CHILD_UUID TURN_ID`. Wake runs the local worker in the foreground. Child turns use the parent's workspace permissions and the ordinary workspace admission fence.

For model-directed delegation, create a thread with `bro thread create --managed` or run `bro task run PROMPT --managed`. Each `spawn_agent` call assigns a fresh read-only child to the current workspace. Calls in one batch run concurrently. The parent receives each conclusion through its tool call. The root checkpoint retains child histories, including gateway receipts. A tree admits 32 agents including the root, four levels of children, four concurrent model requests and 128 model attempts per turn.

## Prompt an ACP adapter

```sh
bro acp list
bro acp serve codex
bro acp --workspace . prompt --agent codex 'Summarize the project'
```

The catalog identifies adapters that can bind to the selected gateway. The prompt command emits NDJSON and denies external permission requests. For an ACP manager, `serve` forwards session updates and permission requests over stdio and supervises the adapter until disconnect. Use `bro acp serve --agent NAME --direct --config agents.yaml` to launch an explicitly configured local stdio command. Native BRO tool approvals use the thread commands above.
