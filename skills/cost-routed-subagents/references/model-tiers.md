# Model tiers: choosing the cheapest model that can do the job

The controller's job is to spend flagship tokens on **judgment**, not typing. Pick
the cheapest tier that can finish a sub-task from fully-provided context; the
controller's review is the safety net.

## The three tiers

| Tier | Use for | Risk |
|---|---|---|
| `cheap` | Mechanical, well-specified, 1–2 files, light tool-use: implement a spec'd function, fix one isolated test, rename, draft boilerplate/docs. | Lowest cost; weakest at multi-step tool-use. |
| `standard` | Multi-file integration, pattern-matching, moderate judgment, heavier tool-use loops. | Mid cost; usually reliable at agentic edits. |
| `flagship` | Architecture/design, the final code review, and anything the cheaper tiers got wrong. May be a flagship model *through* BitRouter, or the controller itself. | Highest cost; reserve it. |

**Complexity signals** (from the controller's vantage):

- Touches 1–2 files with a complete spec → `cheap`.
- Touches several files with integration concerns → `standard`.
- Needs design judgment or broad codebase understanding → `flagship`/controller.

## Populate tiers from the live model list

Tier env vars hold real model ids. A BitRouter model id is addressed as
`provider/model`; discover exactly what your endpoint serves via the API (no CLI
needed):

```bash
curl -s "$BITROUTER_BASE_URL/v1/models" \
  -H "Authorization: Bearer $BITROUTER_API_KEY" | jq -r '.data[].id'
```

Then assign, for example:

```bash
export BITROUTER_MODEL_CHEAP="opencode-go/glm-5.1"           # cheap OSS — verify against /v1/models
export BITROUTER_MODEL_STANDARD="anthropic/claude-haiku-4-5" # verify against /v1/models
export BITROUTER_MODEL_FLAGSHIP="anthropic/claude-sonnet-4-5" # verify against /v1/models
```

> The ids above illustrate the `provider/model` **shape** only. Always confirm the
> exact ids against your endpoint's `/v1/models` — provider catalogs and version
> suffixes change. You can also append a routing variant (e.g. a `:cost` preference)
> if your BitRouter config defines one.

## Tool-use reliability matters more than raw price

Claude Code drives workers through the Anthropic tool-call format; BitRouter
translates that to each provider. Cheaper open-source models vary in how reliably
they emit well-formed tool calls and avoid loops. Practical guidance:

- For **tool-heavy** sub-tasks (lots of Read/Edit/Bash), prefer a `standard`-tier
  model known to be strong at agentic tool-use over the very cheapest option.
- For **single-shot, low-tool** sub-tasks (draft a doc, transform one file), the
  `cheap` tier is usually fine.
- If a worker returns `BLOCKED` or produces malformed output, **re-dispatch one
  tier up** rather than retrying the same model unchanged.

## Verify the savings

Cost reduction is a claim — measure it. Compare token spend before/after on a
representative task using your BitRouter endpoint's own usage/observability
surface. Don't assert savings you haven't observed.
