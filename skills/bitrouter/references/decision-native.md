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
code, task descriptions and evidence summaries. Native commands default to
`bitrouter/auto` when `chat.model` is unset, using the read-only policy shipped
with the binary. This native fallback does not set the model for an external
ACP harness.
Named policy selectors enable model routing automatically; explicit physical
model overrides remain fixed. Effort remains binding in both modes. The decision backend selects bounded evidence
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

Model selection uses the version 4 named policy lock, shared with HTTP requests.
The bundled policy uses the shipped strong default and has no pretrained routes
or progress guard. To customize it, configure `policy.path` and a router whose
`policy` names a lock entry. An existing adjacent `policy-lock.yaml` takes
precedence; an unavailable explicit path is an error. Bundled policies cannot
be published over: initialize a file-backed policy first.
`inherit_defaults: false` disables the bundled defaults. Each tier
is an object with `model`, optional `effort`, and `context: preserve` or `evidence`.
Use that router selector with `bro code --model SELECTOR`.
Create-Thread HTTP/local requests expose the same mode as `model_mode: policy`.

The named policy selects the generation model. The shared planner then chooses
among context views admitted by their owner; it does not optimize a separate
model catalog. `decision_model.policy.generation_models`, global `policy_table`,
scalar tiers and lockfile versions 1–3 are rejected. See the version 4 starter
lock in `templates/auto-router/policy-lock.yaml`.

Native `bro code`, `bro task run` and `bro task managed` accept an optional
`--max-output-tokens` override. When omitted, each step resolves the selected
model's declared output capacity before committing the prompt. Registry canonical
capacities fill missing provider limits; explicit provider values win. Unknown
capacities fall back to 4096, while uncappable providers still require a known
ceiling. The optional override is stored with the Thread and inherited by later
Turns and child agents; `--thread-id` retains it. Native HTTP/local
Create-Thread requests accept the same optional `max_output_tokens` field.
A provider that cannot enforce a request output cap requires a reservation
covering its known model output ceiling; a smaller or unknown bound is rejected.
For Codex subscription setup and a runnable example, see
[harness-codex.md](harness-codex.md).

Native Core checkpoints are bounded at 2 MiB; the artifact journal retains at
most 32 MiB of bodies. Large checkpoints are split atomically across the host's
recovery pages, independently of the online Core capacity. Incomplete parts,
changed hashes or missing Item projections block recovery. These bounds remain
explicit capacity limits; context omission never deletes historical evidence.

Remote managed-core harnesses explicitly negotiate `context_views_v1` and set
task context routing to `auto`. Preserve exact checkpoints before ACK; decision
intent, outcome and applied view are separate durable boundaries. See
[managed-core.md](managed-core.md) for transport and ownership requirements.
