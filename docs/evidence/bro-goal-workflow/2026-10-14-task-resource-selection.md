# Task resource selection and supplied ACP material

The Goal planner receives host-owned configured MCP IDs and local skill
metadata. It may select a bounded subset for each task, alongside relative
context file paths. Skills bind both the stable material ID and advertised
SHA-256, so equal names do not select an arbitrary directory and changed bytes
do not silently replace the planned version.

The runtime reads context from the isolated execution workspace after checked
dependency handoff. It captures exact context/skill bytes and the complete
execution prompt in private, content-addressed Goal artifacts. A new
`PrepareResources` workflow fact commits those references before dispatch;
the Goal/task versions, owner, intent, workspace identity and budget are checked.
Nonempty selections cannot dispatch without their prepared material. Descriptor
credentials remain transient; the public head contains selected IDs and a
configuration digest. Empty selections omit the new TaskSpec field on
serialization, preserving the older receipt payload shape.

The runtime forwards only selected MCP descriptors through the existing ACP
`session/new` surface, preserving configured arguments, environment and HTTP
headers. The shared session host checks negotiated HTTP/SSE support before
opening a session and shuts down an unsupported adapter. Stdio descriptors are
the ACP baseline. The read-only branch view exposes supplied bytes and versions.
Supplying/configuring a resource does not demonstrate tool use or private skill
activation. Actual provider model identity remains unreported when unavailable.
Model selection and runtime resource replacement are separate remaining work.

Unix reads use descriptor-relative openat, no-follow on every descendant,
nonblocking opens, regular-file checks and byte/UTF-8 bounds. Non-Unix reads
perform component/link checks and Windows reparse-point opening checks. The
local run does not prove Windows runtime behavior. Up to 16 selections of each
kind and 256 KiB of combined material are allowed; prompts are bounded to 1 MiB.
No resources grant new tool permissions or implicitly resume paused work.

## Observed verification

The first two builds failed before tests because the FIFO test used `mkfifo`
and then `mkfifoat` from rustix, which does not expose those operations on
Apple. The test uses the existing nix safe wrapper with its fs feature instead;
production descriptor-relative reads still use rustix. Original failure logs
are retained.

v3 ran seven tests: six passed, one skill-fixture test failed, and one passing
context test reported LEAK. The skill fixtures used directory names different
from their frontmatter names. They now use the same valid skill name in two
different selected roots. The actual coordinator/SQLite/parallel ACP/handoff/
delivery resource case passed: its saved prompt matched the ACP prompt bytes,
`session/new` received only the selected descriptor, and its environment
credential did not enter public Goal metadata. This demonstrates transmission,
not actual MCP invocation or semantic skill use. Provider decisions are fixed
fixtures and do not establish real-model quality.

v4 added advertised skill version binding and the legacy empty-selection
serialization check. All eight cases passed without LEAK: run
`f11cecb9-8ead-4bd8-83e8-dcdb8834331f`, exit 0, 105.264 seconds. That run used
one test worker. It does not establish the cause of v3's LEAK report.

v5 added an actual ACP capability rejection/cleanup case and the original
coordinator regressions. Its test compilation rejected an ambiguous `.into()`
at an `impl Into<String>` constructor. The test now passes string literals;
v6 runs thirteen cases with two workers against a frozen 102-file manifest.
Its outcome and final checks are recorded in the implementation ledger.

v6 passed all thirteen cases: run `520fa422-6e72-4846-8551-661267726c1b`,
exit 0, 124.310 seconds, two slow cases, no LEAK or timeout. The actual
capability-negative case verified one initialize request, no session/new or
prompt for either HTTP/SSE, and confirmed adapter cleanup. The original
parallel, paused edit, active edit and uncertain unfinished-tool cases also
passed. All 102 source hashes matched; formatting/diff and three plugin JSON
manifests passed. This is still a partial check.

Before the full workspace build, only about 5.3 GiB was free. All owned test,
Cargo and fixture processes had exited. The same worktree target directories
were revalidated; a package-scoped clean dry run showed 1,820 regenerable
BitRouter artifacts totaling 13.5 GiB. Only this worktree's BitRouter package
artifacts were cleaned, preserving sources, evidence and dependency caches.
The full workspace run uses four test workers and the existing pressure-group
serialization and deadlines. It does not change product concurrency limits.

The actual package clean removed 1,789 files, 12.5 GiB, restoring about 17 GiB
free. All 102 source hashes still matched. Full workspace run
`d149233a-a6d5-43a5-b465-697f4fc1d9a3` completed with 4,562 passed, 22 existing
skipped, exit 0, 755.708 seconds, ten slow cases and no LEAK or timeout. The
four actual Goal PTY cases passed with the new resource projection. Static and
doc check results for this source version are recorded separately in the ledger.

The v6 Clippy check rejected a redundant `Ok(...?)` return. v7 returns that
same Result directly; only resources.rs differs in the 102-file manifest.
Workspace/all-target/all-feature Clippy with warnings denied then passed.
Run `91de484a-6a97-482f-ae79-17ff63b893bd` passed all seventeen relevant cases,
exit 0, 146.728 seconds, four slow cases, no LEAK or timeout. This includes
all four actual Goal PTY cases. Separate doctests passed six cases, with one
existing ignored example across seven suites. Formatting, diff checks and all
102 source hashes passed. These results do not describe a second full v7 run.

No original runtime or protocol case is removed. The repository skill, CLI
reference and three plugin manifests describe this actual resource wiring and
its limits. Full input evaluation, real harness/provider/typed backend evidence
and final G/A/F/workspace acceptance remain outstanding.
