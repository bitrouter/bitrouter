# Harbor Agents Through BitRouter

Use the agent selected by the user. Do not replace Codex or Claude Code with
Terminus 2 for neutrality. Harbor's installed agent list and flags are the
authority:

```bash
harbor --version
harbor run --help
```

Official Harbor agent overview: <https://www.harborframework.com/docs/agents>

## Resolve the common inputs

For every agent, resolve and confirm these three separate values:

1. Harbor agent name and version.
2. Harbor model string sent to BitRouter (the entry preset/alias/model).
3. Every downstream provider/model/fallback reachable from that entry.

Also confirm the name of the BitRouter inbound credential source and each
upstream credential source. Never expose their values. Give the Harbor agent an
inbound BitRouter credential, not the router's upstream provider secrets.

Use the installed `bitrouter` skill or selected BitRouter source for exact
current harness variables and config syntax. The protocol boundaries below are
stable selection guidance, not a substitute for current CLI help.

## Codex

- Select Harbor's installed Codex agent (commonly `codex`).
- Route it through BitRouter's OpenAI Responses-compatible surface.
- The client base normally ends in `/v1`; verify the selected Codex and
  BitRouter versions rather than assuming an environment variable is honored.
- Keep Codex provider configuration scoped to the Harbor job. Do not silently
  rewrite a user's global Codex config.

Smoke with one trivial, non-benchmark instruction that requires a short answer
and no repository mutation. Confirm the request reached the intended BitRouter
entry and a reachable downstream target.

## Claude Code

- Select Harbor's installed Claude Code agent (commonly `claude-code`).
- Route it through BitRouter's Anthropic Messages-compatible surface.
- The Anthropic base normally omits `/v1` because the client appends the
  Messages path; verify this against the selected versions.
- Scope the base URL and inbound auth token to the Harbor agent environment.
  Do not place upstream Anthropic or subscription credentials in a sandbox.

Smoke with Claude Code's non-interactive/headless path. Confirm tool-capable
Messages traffic reaches the expected entry. A Codex smoke does not validate
Claude Code.

## Terminus 2

- Select Harbor's installed `terminus-2` agent.
- Configure its agent kwargs using the installed Harbor schema. Its `api_base`
  can target an OpenAI-compatible BitRouter surface; the model string may need
  the provider prefix expected by the installed LiteLLM integration.
- For an explicit Anthropic-shaped route, omit `/v1` and confirm the selected
  integration supports that path. Do not infer it from an OpenAI-shaped smoke.

Official reference: <https://www.harborframework.com/docs/agents/terminus-2>

## Other Harbor agents

Inspect their official Harbor reference and installed config schema. Identify
the protocol they actually use, map that protocol to a BitRouter surface, and
run the same bounded smoke. If the agent bypasses custom endpoints or its
protocol cannot reach BitRouter, stop rather than substituting another agent.

## Smoke contract

Put the following in the resolved plan and get confirmation before the smoke:

- exact agent, entry model, protocol, and endpoint;
- a content-neutral prompt or tiny local task;
- maximum request count (normally one; higher only for tool-loop validation);
- cost ceiling and timeout;
- retained Harbor and BitRouter evidence;
- cleanup for any temporary agent config.

The smoke validates connectivity, authentication, protocol, and route
resolution only. It is not a scored benchmark result and does not validate a
different agent.
