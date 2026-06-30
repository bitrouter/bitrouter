---
title: Overview
description: Point a coding agent or runtime at BitRouter — Claude Code, Codex, OpenCode, Hermes, OpenClaw, Pi — and run it on any model in the registry.
sourceHash: 85f397c71e014e6dec7bcdb24119abf335a484362d4dccf2cb2e10df212a475d
---

A **harness** is the agent runtime that drives the loop — the CLI or service that reads your prompt, calls tools, and edits files. Each one already speaks some model API; the move is always the same: point it at BitRouter's endpoint (local proxy at `http://127.0.0.1:4356`, or Cloud at `https://api.bitrouter.ai`) instead of the vendor's, and address models by their `provider/model` id. From there the same harness can run on Anthropic, OpenAI, Google, or an open model — with [provider selection](/docs/features/provider-selection) and [fallback](/docs/features/model-fallback) underneath.

<Cards>
  <Card title="Claude Code" href="/docs/integrations/claude-code" description="Anthropic Messages via ANTHROPIC_BASE_URL" />
  <Card title="Codex" href="/docs/integrations/codex" description="Custom provider in ~/.codex/config.toml" />
  <Card title="OpenCode" href="/docs/integrations/opencode" description="Provider block in opencode.json" />
  <Card title="Hermes" href="/docs/integrations/hermes" description="Nous Research's self-improving agent" />
  <Card title="OpenClaw" href="/docs/integrations/openclaw" description="Messaging gateway across many channels" />
  <Card title="Pi" href="/docs/integrations/pi" description="Minimal terminal coding harness via models.json" />
</Cards>

## Harness vs. model source

Two of these share a name with a [model source](/docs/integrations/models), and it's worth keeping them straight:

| You want to… | Use |
| --- | --- |
| Run the **Claude Code CLI** on any model | [Claude Code](/docs/integrations/claude-code) (harness) |
| Spend your **Claude plan** as tokens | [Claude subscription](/docs/integrations/claude-subscription) (model) |
| Run the **Codex CLI** on any model | [Codex](/docs/integrations/codex) (harness) |
| Spend your **ChatGPT plan** as tokens | [Codex subscription](/docs/integrations/codex-subscription) (model) |

A harness is *what runs*; a model source is *where the tokens come from*. You can combine them freely — e.g. drive Claude Code (harness) against your ChatGPT plan (Codex subscription), or against a local Ollama model.

## New to BitRouter?

Start with the [Quick Start](/docs/get-started/quickstart) to get the proxy running, then come back and point your harness at it.
