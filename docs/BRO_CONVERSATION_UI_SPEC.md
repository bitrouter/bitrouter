# BRO Conversation and durable Threads navigation

Date: 2026-10-04. Approved scope: integrate PR #952's native-scrollback UI into
PR #945; BRO Threads populate the native Agents menu, while explicit ACP entry
points retain their supervisor inventory. This contract supersedes the native
presentation in the Thread/Turn unification contract. The ACP navigation contract
in [CODE_TUI_CODEX_NAVIGATION_SPEC.md](CODE_TUI_CODEX_NAVIGATION_SPEC.md) remains
applicable to explicit ACP sessions.

## Ownership and entry

`bro code` opens BRO Conversation. `bro code <agent>` remains ACP. Remote and
operations-only entry points retain their target-specific behavior. No default
ACP picker is introduced into BRO's native path. A missing model opens a separate
model editor; selection does not consume or submit the conversation draft.

ThreadService remains the execution, queue, context and commit authority. UI
state contains presentation and an in-process editor, not a second queue or
model loop. The existing Writer, styled dock conversion, Markdown renderer,
Editor and AgentsMenu are shared. No transcript database is introduced.

Conversation and native Agents remain in the normal terminal buffer. The
Conversation is chronological, with committed entities keyed by server Item ID
and an independently replaceable live tail. Stable user/assistant/tool/
verification entities preserve their first-seen position when updated. Native
history uses bounded server pages at one durable cutoff. Reconnect deduplicates
transactions by sequence; expired observation history triggers durable history
reconstruction. Unknown cost is never shown as zero.

## Input and navigation

Enter starts a Turn when idle, or enqueues through the service while busy/paused.
Ctrl-Enter is explicit targeted steering. Ctrl-R explicitly resumes the service
queue. Accepted input clears the draft; an uncertain result retains its original
key and mode. Ctrl-C cancels the originating active Turn; Ctrl-D detaches without
answering approvals or submitting drafts. Existing stable control IDs remain.

An unmodified Left press opens Agents only with an empty composer, no pending
approval, and no nonempty model selection. Nonempty drafts keep Left for editing;
release/repeat cannot trigger navigation. Search and preview consume their own
input. While Agents is open, foreground observations keep updating state but the
Conversation projection remains frozen until return. An arriving approval does
not move focus and menu typing never grants approval. Terminal resize and
suspend/resume retain the same draft, selection and service identities. Below
40×16, submission/approval are disabled while detach remains available.

The native menu shows durable BRO Threads, never synthetic ACP process identities.
Rows show actual model/directory/Thread/Turn/queue/permission facts. Needs input,
Working, Inactive, Paused and Recovery are based on supplied public state.

- Up/Down and PageUp/PageDown select within the current directory page.
- Tab/Shift-Tab cycle filters; `/` searches that page locally.
- Enter opens metadata preview. Selection and preview never attach or mutate.
- `o` in preview explicitly restores and observes the selected Thread.
- Esc leaves search/preview, then returns to the original Conversation.
- `r` refreshes directory membership; `n`/`p` page when available.

An unresolved acceptance or nonempty draft blocks switching. The previous
Thread's execution, approvals and durable queue remain server-owned. Opening
history does not execute, resume or grant recovery. Errors preserve the previous
Conversation and display an unavailable/stale directory state. Recovery-required
Threads can be inspected but cannot start new work through this view.

## Local protocol and directory

Local protocol is v15; HTTP v2 is unchanged. All local clients use the same
version and negotiated instance. No old-protocol fallback or implicit resubmit.
`ListThreads { after, cutoff, limit }` permits 1–16 roots per request and uses the
durable root index. Membership cutoff is stable across pagination; each row's
status is a current public projection rather than a frozen global snapshot.
Caller ownership and current workspace/profile grants filter results. Empty
filtered pages can still advance their opaque root position. A byte bound and
existing cold query bounds apply. Listing installs no context, worker, subscriber,
lease or queue runner, and performs no durable writes. Each page refreshes every
two seconds while the menu is resident; refresh explicitly resets membership.

When integrated with main's daemon upgrade mechanism, the standalone native
endpoint holds daemon admission for its lifetime. ACP/HTTP idleness alone cannot
authorize replacing its epoch. Automatic replacement is deferred; explicit
restart remains available until native queue/worker handoff is implemented.

## Acceptance

1. Directory pagination includes unloaded durable roots, filters foreign callers
   and revoked grants, and does not change residency, journal or execution.
2. Opening restores committed user/assistant history; continuing sends that same
   Thread's retained context through the routed SDK model request.
3. Browsing/preview/open causes zero new model requests or tool effects.
4. Working Enter stays FIFO; Ctrl-Enter stays steering; unknown acceptance keeps
   draft/key; cancellation and detach preserve their distinct server behavior.
5. Approval arrival/search/menu typing never approves; reconnect retains draft,
   queue and pending approval; repeated detach frees observation subscriptions.
6. PTY navigation, resize, suspend/resume and terminal restoration preserve
   normal-buffer history without alternate-screen or scrollback-clear sequences.
7. Required workspace tests, Clippy, formatting and documentation checks pass.
   Local fixtures, hosted CI and real provider/platform proof remain distinct.
