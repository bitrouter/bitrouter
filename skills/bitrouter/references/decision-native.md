# Native context and SDK routing

BRO's native Core no longer calls a decision backend for skill relevance,
task classification, or selecting lossy context representations. Skills are
explicit `--skill` constraints. Context preparation keeps complete admitted
source groups; exceeding the hard bound is a capacity failure, not permission
to omit evidence automatically. Native execution, fixed/policy model routes,
permissions, budgets, checkpoints and conditional recovery remain available.

The generation model can still request `context_search`, `context_recall`,
`context_publish`, `context_extract`, and `context_read_artifact`. These operations
retain source provenance; publishing a summary does not automatically select it
for a later prompt. Root/child evidence scope and protected tool pairing remain
owned by Core.

`decision_model:` remains an optional SDK routing assessment backend. Its typed
executor and `/v1/systemone` configuration serve model-policy evaluation for
caller-owned requests; they do not enable native automatic context or skill
selection. The separate public `/v1/decisions` operation remains supported.
Do not remove SDK routing or provider Decisions support when disabling native
agent decisions. Those paths have separate consumers and tests.

Historical acknowledged decision records remain readable for accounting and
recovery validation. Interrupted historical attempts are not repeated. New
native and ACP skill preparations produce no semantic decision request.

Use `bro skills list/show/read/write` on existing local directory files. Install
or acquire updates with `npx skills`. A native Turn retains its original file
manifest and verifies bytes before use; there is no retained-version directory.
MCP skills retain independent source grants, explicit content consent, origin,
complete manifests, on-demand verification and conditional cache reuse.
