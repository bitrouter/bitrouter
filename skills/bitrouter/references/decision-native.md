# Decision-native BRO

Configure an independent typed decision backend in `bitrouter.yaml`:

```yaml
decision_model:
  model: YOUR_TYPESAFE_MODEL
  api_key_env: TYPESAFE_API_KEY
  base_url: https://api.typesafe.ai
  timeout_ms: 30000
  max_response_bytes: 1048576
  policy:
    target_context_bytes: 98304
    max_candidates: 32
    candidate_excerpt_bytes: 2048
    max_request_bytes: 131072
    retain_recent_groups: 2
    confidence_threshold: 0.85
```

Set `TYPESAFE_API_KEY` in the server's environment. Use a model name or alias
available to that account. This backend uses TypeSafe's `/v1/systemone` typed
Choice, Score and Noul protocol; an OpenAI-compatible chat endpoint cannot
replace it. Configuration errors or a missing credential fail server assembly.
Credentials are excluded from execution checkpoints.

Native `bro code` and `bro task run` use the Core execution adapter. Without a
decision backend, the same compiler retains complete conservative context.
Their selected generation model still writes responses,
code, task descriptions and evidence summaries. Native `--model` is fixed unless
`bro code --model-policy` explicitly enables model selection; effort remains
binding in both modes. The decision backend selects bounded evidence
representations; code retains control of permissions, required instructions,
tool pairing, budgets, approvals and dispatch.

Each work unit retains immutable source evidence. A view can include the full
source, a previously published exact extract or summary, or omit an optional
group. Omission changes the next prompt, not the retained history. Instructions,
recent protected groups and provider continuation material cannot be silently
discarded. Invalid, failed or low-confidence decisions keep conservative context;
if it cannot fit the hard prompt bound, execution reports a capacity failure.

The generation model can use these Core tools:

- `context_search`: search the task's source inventory, including omitted groups.
- `context_recall`: pin complete source groups into the next view.
- `context_publish`: publish a task-specific summary of one optional source group.
- `context_extract`: publish exact UTF-8 byte spans tied to their source group.
- `context_read_artifact`: page through a complete retained native tool body by
  artifact ID and UTF-8 byte offset; continue with the returned `next_offset`.

Native tool results above their prompt envelope are stored as immutable artifacts
before the result is acknowledged. Their previews include the reference and
checksum. The original body survives cold recovery, and only tasks with the
source evidence and current workspace permissions can read it. A failed artifact
commit blocks continuation instead of silently dropping output. Configured tool
output limits still apply at the tool's source (for example shell output caps).

Thread context-routing events and `bro code` show task status, selected view size,
omitted evidence groups, result references and separate decision usage/estimates.

Worker tasks share admitted evidence references. Each worker compiles its own
view; related tool exchanges are rendered as historical evidence, so their call
IDs cannot become live calls in another worker. Opaque provider continuations
stay with their originating conversation. Fresh or independent-review workers
do not inherit parent evidence. Completion delivers conclusion references and
grants the parent access to portable source groups for search and recall.

Successive native Turns retain the Core session, evidence, views and decisions.
Cold Thread loading is inspection only. The existing explicit recovery operation
still requires a stopped source owner and known effects/accounting before work
continues. An unresolved effect or interrupted usage record is not replay permission.

Optional `decision_model.pricing` contains operator-supplied
`input_usd_per_million` and `output_usd_per_million`. Decision usage and token
estimates are recorded separately from generation and cache counters; neither
is a settled bill. Native estimated spend limits include observed decision
estimates and stop new model work when completed work cannot be estimated.
Set explicit prices when using a spend limit with the decision backend.

To enable joint generation-model/context selection, list up to 16 explicit
`decision_model.policy.generation_models`. Each entry requires `model`,
`description`, `max_prompt_bytes`, `input_microusd_per_million`, and
`output_microusd_per_million`. The model supplied by `--model` must be one of the
entries and is the conservative fallback. Descriptions and prices are operator
assumptions; use normal SDK model selectors and provide actual configured routes.
Create-Thread HTTP/local requests expose the same opt-in as `model_mode: policy`.
Existing requests default to `fixed`.

The planner asks independent Noul suitability questions alongside evidence
Choice questions, then compares each eligible model with a selected view and,
when it fits, a complete view. Prices use a four-bytes-per-token planning
estimate and the output reservation. SDK permission, protocol, capability and
token-capacity admission still run before generation. The default
`minimum_savings_fraction: 0.1` and `model_switch_penalty_microusd: 1000`
discourage small switches. An optional
`prefix_loss_penalty_microusd_per_kib` defaults to zero. Exact shared prompt
prefix bytes are recorded; they do not prove provider cache availability or
discounts. All candidates, assumptions and the selection survive recovery.

Native Core checkpoints are bounded at 2 MiB; the artifact journal retains at
most 32 MiB of bodies. Large checkpoints are split atomically across the host's
recovery pages, independently of the online Core capacity. Incomplete parts,
changed hashes or missing Item projections block recovery. These bounds remain
explicit capacity limits; context omission never deletes historical evidence.

Remote managed-core harnesses explicitly negotiate `context_views_v1` and set
task context routing to `auto`. Preserve exact checkpoints before ACK; decision
intent, outcome and applied view are separate durable boundaries. See
[managed-core.md](managed-core.md) for transport and ownership requirements.
