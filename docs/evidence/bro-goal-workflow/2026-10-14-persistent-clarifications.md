# Persistent clarification and completion boundary

The first actual counterexample used production head `5a18a35e` plus a new
database test: a ready delivery received a requested change, understanding
asked a clarification, then an unrelated query was answered. Committing the
old delivery was still accepted. The v1 test failed, exit 100; its log is
retained. This is a reproduced completion gap, not only source inference.

Unresolved requests now have independent durable identities, source versions,
questions and scopes. The original user bytes are retrieved from immutable
input receipts for the same owner/Goal. Reasoning receives at most 16 original
contexts and 256 KiB of their serialized content, with omitted context counted
explicitly. `/input <id> <reply>` prioritizes one exact open question without
granting permission or changing the current Goal scope. A resolution absent
from the supplied original context cannot be committed by the runtime.

Runtime validates source version, original scope, explicit reference and
recorded reasoning. Applied requires a saved effect in the original scope;
answered cannot close a known change request through commentary alone;
withdrawn records the requested withdrawal and explanation. Resolution is
atomic with requirement/control/permission intent, and is not worker adoption
or result verification. Ordinary queries and silence do not clear requests.
Late source/version changes preserve the request without saving stale effects.
Permission consent still requires a current actual Decision/card and option.

Unknown understanding and change ambiguity fence completion. Independent work
continues under its actual run intent. Query-only clarification stays visible
without adding a completion condition or requiring artificial human acceptance.
Old latest-only snapshots retain their question across the next query and
recover original Task/Decision scope from the actual input receipt.

v2 passed 32 selected cases. v3 run
`73ac5de1-673b-4498-b223-1a0012deaf0f` passed 36, exit 0, 0.341 seconds,
32 filtered. It includes completion fencing, withdrawal, query-only completion,
identity/version/scope rejection, multiple questions, foreign Goal rejection
and actual legacy SQLite payload recovery. v4 did not execute tests: a new
rollback assertion used the wrong error type. That assertion was repaired;
v5 has its own 102-file source identity and actual coordinator/UI checks.

All model judgments in these fixtures are fixed. They do not prove intent
accuracy, real typed/provider/harness behavior or complete MVP acceptance.
The existing aggregate Goal-record bound remains; this slice does not claim
unbounded head/history archival. This turn remains local and unpushed.

v5 run `e8e2b145-6bb4-464a-a97c-f2c1238d86bb` passed 89 cases, exit 0,
121.096 seconds, four slow cases, no LEAK or timeout, 2,196 filtered. A forced
SQLite receipt-insert failure preserved the entire previous head and question
set, without a requirement or resolution commit. The actual ACP resource-edit
fixture required the old original input receipt in the reasoning HTTP request,
saved its applied disposition only with the versioned replacement, retained
the independent execution and completed real file handoff/checks. All four
existing Goal PTY scenarios passed. The 102-file v5 source is frozen for the
separate full-workspace/static/doc run; selected success is not full-workspace
or hosted CI success.

The frozen v5 source subsequently passed the full workspace: run
`6dfc1568-7810-4dfe-af8b-c6b31ce1d0cd`, 4,589 passed, 22 existing opt-in
skips, 71 binaries, 658.539 seconds, nine slow cases, no LEAK or timeout.
Compound session 30488 exited 0 after strict workspace/all-target/all-feature
Clippy (70 seconds), separate doctests (six passed, one existing ignored),
formatting, diff and 102 source-hash checks. These are local macOS checks.

The published PR remains head `5a18a35e`: Windows job 115746223590 passed;
Linux lib job 115747439321 failed with an opaque model source/owner/durable
acknowledgement error. That original gate/CAS/ack cause and safe recovery still
need investigation. Local v5 success does not establish hosted Linux success.
An explicit withdrawal path that remains available without a model or budget
is also unfinished; natural-language disposition still uses reasoning. The
full input evaluation, real service evidence and acceptance gates remain open.
