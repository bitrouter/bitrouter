# Spec: the TUI renderer — a retained journal behind a differential writer

Status: **proposed** · Author: Claude (with Spikel) · Date: 2026-08-14 · Rev 3

Companion plan: [`TUI_RENDERER_PLAN.md`](TUI_RENDERER_PLAN.md) — numbered tasks
and gates, per the house pattern set by
[`ACP_TUI_PLAN.md`](ACP_TUI_PLAN.md).

Builds on [`ACP_TUI_SPEC.md`](ACP_TUI_SPEC.md) and
[`ACP_TUI_PLAN.md`](ACP_TUI_PLAN.md), both **complete**. It keeps their
decisions and changes exactly one mechanism: PLAN §C decision 7's
`ratatui::Viewport::Inline`, whose inline / no-alternate-screen property §4
preserves by other means.

**Context.** The previous v1 shipped 1,459 lines — 792 production, 667 test —
turning an ACP `session/update` stream into terminal lines over one-row
`ratatui::Viewport::Inline`. The protocol half is right and the discipline is
right. What is wrong is that the renderer is **append-only** against a protocol
whose entities are **patchable**, and that it can only ever own one row.

> **Rev 3 changelog.** Rev 2 was put through a seven-reviewer adversarial panel
> which upheld 75 findings, including 7 blockers. Rev 3 fixes all of them.
> The blockers were: (a) §16.4's boundary grep still could not pass — it returns
> **four** matches, not three; (b) `scroll_region_up`/`scroll_region_down` are
> behind ratatui's non-default `scrolling-regions` feature and are *not* on the
> trait as we compile it; (c) `MetaRenderer` could express neither of its two
> stated clients, so that registry is **deleted** (§7); (d) `ToolKind` derives
> no `Hash`, so the prescribed `HashMap` does not compile; (e) §4.3's drop rule
> wedged the writer permanently; (f) §4 stated coordinates in a space the
> backend rejects; (g) session-long raw mode destroyed v1's no-panic-hook
> premise with no replacement. Rev 3 also gains the task breakdown every critic
> asked for, now in a companion PLAN.

---

## 1. Motivation

### 1.1 Three defects, one cause

All three follow from "append a line and never touch it again."

1. **A tool call prints once per status change.** `ToolCall` and
   `ToolCallUpdate` each emit a fresh line
   ([`transcript.rs:72`](../crates/bitrouter-tui/src/transcript.rs:72)), so one
   call going pending → in-progress → completed leaves three lines in
   scrollback. `insert_before` content belongs to the terminal the moment it is
   written, so the first cannot be revised.

2. **Tool output is dropped entirely.** `diff_lines`
   ([`transcript.rs:167`](../crates/bitrouter-tui/src/transcript.rs:167))
   matches `ToolCallContent::Diff` and `continue`s past everything else, so
   `Content` — the variant carrying a tool's actual output — and `Terminal`
   render nothing. `block_text`
   ([`transcript.rs:159`](../crates/bitrouter-tui/src/transcript.rs:159)) does
   the same to four of `ContentBlock`'s five variants. An `Execute` tool call
   renders a title, a glyph, and silence.

3. **A one-line edit can commit a thousand lines.** ACP's `Diff` carries full
   `old_text` and `new_text`, not a patch. `diff_lines` prints every line of old
   prefixed `-` then every line of new prefixed `+`.

Defect 1 needs a retained model. Defects 2 and 3 do not.

### 1.2 What the protocol keys today

Two channels, and they are not equivalent — rev 2 conflated them:

- **A true patch channel.** v1's `ToolCallUpdate` patches a tool call by
  `toolCallId`. This is the real thing and the journal's tool half rests on it.
- **A grouping key.** v1's `ContentChunk` carries
  `message_id: Option<MessageId>` (`v1/client.rs:417`, type at `:468`). It is
  optional, no agent is obliged to send it, and nothing in
  `crates/bitrouter-sdk/src/acp/` sets it. So it is a hint for grouping chunks
  into a message, not a guarantee — §3 specifies the fallback.

ACP **v2** makes the message key required and gives it patch semantics. §10 of
the prior spec is right that adopting v2 is an engine rewrite and out of scope;
it does not follow that the renderer's data model should stay append-only.

### 1.3 The one-row ceiling is imposed by the dependency

[`viewport.rs:38`](../crates/bitrouter-tui/src/viewport.rs:38) sets
`LIVE_ROWS: u16 = 1` behind a compile-time `assert!(LIVE_ROWS <= 2)`. ratatui
0.29 keeps `viewport` a private field (`terminal/terminal.rs:70`) and
`Terminal::resize` reuses the stored height, so there is no in-place way to
change a `Viewport::Inline` height. `TerminalOptions.viewport` is public
(`:86`), so a `Terminal` can be reconstructed at a new height — but that clears,
which is why it is not a workaround.

### 1.4 The reference, accurately

**pi-tui** (`earendil-works/pi`, `packages/tui`) is the right reference; opentui
is not. pi's whole widget contract is four members; opentui's `Renderable.ts` is
1,893 lines presuming a native hit grid, a scissor stack, and Yoga compiled into
a Zig library — a toolkit for third-party widget authors across React, Solid and
Three bindings. BitRouter has none.

**Correction to [`ACP_TUI_SPEC.md`](ACP_TUI_SPEC.md) §8**, which reads pi's
`TuiMainScreen`/`TuiAltScreen` split as *inline good, alt-screen bad*. pi treats
them as two modes sharing component instances, and pi's `tui-plan.md` states the
trade: the terminal owns scrolling on the main screen, so an application cannot
provide sticky rows or nested scroll regions there — *"do not pretend the same
constrained viewport semantics exist in `TuiMainScreen`."* Main-screen mode in
pi is a vertically rendered document, not a viewport with a pinned bar. By that
standard BitRouter's one-row status bar is the fake sticky region pi refuses to
build, and §4.5 drops it.

**pi is not an ACP client** — `packages/protocol` is its own CBOR/framing stack.
Nothing here inherits pi's protocol choices.

## 2. Goals / non-goals

**Goals**

- Entities are **retained and patchable**, keyed as the protocol keys them.
- The writer **repaints in place** what is on screen and never corrupts,
  rewrites, or discards what is not.
- Rendering is extensible **by us, internally** — one trait, one registry.
- BitRouter-specific metadata parsing and capability decisions stay in the app;
  `bitrouter-tui` renders typed protocol values and explicit caller-supplied
  decisions.
- The renderer draws every `SessionUpdate` variant the stream already carries.

**Non-goals**

- Everything §2 of [`ACP_TUI_SPEC.md`](ACP_TUI_SPEC.md) excluded stays excluded.
  Single-session is the line.
- §8.3's scope rule still binds: session-scoped data only.
- A third-party plugin API — §11, refused with a trigger.
- A general TUI widget toolkit.
- ACP v2 adoption.
- Mouse, real image rendering, layout engines, flexbox, alternate screen.
  (§6 renders a one-line *textual description* of an image block; that is not
  image support, it is the absence of silence.)

## 3. Decision 1 — a retained journal

```rust
pub struct Journal {
    /// Document order, first-seen. Patches never reorder.
    order: Vec<EntryId>,
    messages: HashMap<MessageId, Message>,
    tools: HashMap<ToolCallId, ToolCall>,
    plan: Option<Plan>,
    commands: Vec<AvailableCommand>,
    mode: Option<SessionModeId>,
    config: Vec<SessionConfigOption>,
    title: Option<String>,
    usage: Option<UsageUpdate>,
    /// Not a `SessionUpdate`: permission arrives on its own request channel.
    pending_permission: Option<Prompt>,
}

impl Journal {
    /// Patch the journal from a session update. Returns nothing: the writer
    /// reads the whole document, so there is no per-update line list for a
    /// caller to mis-order.
    pub fn apply(&mut self, update: SessionUpdate);

    /// Set or clear the open permission prompt. Separate from `apply` because
    /// `session/request_permission` is a request, not an update.
    pub fn set_pending_permission(&mut self, prompt: Option<Prompt>);
}
```

Every field is consumed by v1 scope (§15): `plan`, `mode`, `config`, `commands`
and `title` by §9; `usage` and `pending_permission` by §4.5's footer. `EntryId`
is this crate's own enum over the keyed kinds.

Three load-bearing properties:

- **`apply` returns `()`.** Today it returns `Vec<Line>`, which forces the
  caller to own ordering and forces append-only. Removing it enables patching —
  and creates the scheduling question §5 answers.
- **First-seen order is stable.** A patch to an early entity keeps its place.
- **Message keying is sticky, not per-chunk.** Because `message_id` is optional
  (§1.2), the rule is: each voice has at most one open run; a chunk with
  `Some(id)` that differs from the open run's id closes it and opens a new one;
  a chunk with `None` **continues** the open run rather than starting a
  synthesized one. A run also closes when the other voice speaks or the turn
  settles. Under v2 the id is required and the fallback dies.

**The turn-end flush requirement disappears.**
[`transcript.rs:123`](../crates/bitrouter-tui/src/transcript.rs:123) documents
that `flush()` **must** be called at every turn end, because v1 ends a turn on
the `session/prompt` response, which never appears on the update stream — and
shipping without it meant plain answers were never drawn (`e8c71e93`). A journal
has no buffered state to lose: an in-flight message is a `Message` with
`complete: false`, on screen throughout.

## 4. Decision 2 — a differential writer over ratatui's `Backend`

### 4.1 State, coordinates, and the frame

**Two index spaces, named once so nothing later conflates them.**

- `prev` and `next` are **full-document** vectors of physical rows, in
  **document space**. Length is the whole rendered journal, not the screen.
- `viewport_top: usize` is the document index of the first row still on screen.
- `anchor: u16` is the absolute screen row at which document row `viewport_top`
  is painted. It is read once at construction via
  `Backend::get_cursor_position`, and becomes `0` permanently once the document
  first exceeds the screen.
- **Document row `r` paints at screen row `anchor + (r - viewport_top)`**, and
  is on screen iff `viewport_top <= r < viewport_top + (height - anchor)`.

Only `prev[viewport_top..]` was ever painted; the rest is scrollback or was
dropped by §4.3.

**The frame:**

1. Render the journal to `next` at the current width (§4.2 wraps; §5 says when,
   and the per-entity cache means this is not a full re-derivation).
2. Diff `prev` against `next` over the whole vector for `first_changed` /
   `last_changed`.
3. Nothing changed → do nothing, and skip step 7.
4. **Clamp the paint range** to `[max(first_changed, viewport_top),
   last_changed]`. If `first_changed < viewport_top`, §4.3 applies to the
   clipped-off prefix — the frame still paints whatever remains.
5. `set_cursor_position((0, anchor + (start - viewport_top)))`, then `draw`
   cells at screen coordinates for each changed row. To clear rows `next` no
   longer occupies: position at the first trailing row and issue one
   `clear_region(ClearType::AfterCursor)`, which blanks to the bottom of the
   screen — there is no range-clear on the trait, and this is the only shape
   that expresses it.
6. Growth past the last screen row: `set_cursor_position((0, height - 1))`
   **first**, then `append_lines(n)`. A bare line feed away from the last row
   moves the cursor without scrolling anything, so the positioning is not
   optional. Then `set_cursor_position` explicitly — never infer the column from
   `append_lines` (§4.4). Advance `viewport_top` by `n` and clamp `anchor` to 0.
7. **`prev` is replaced wholesale by `next`**, painted rows and dropped rows
   alike.
8. The whole write is wrapped in a synchronized-output pair (§4.4).

**The writer tracks its own cursor** from the positions it sets and never
queries the terminal on the frame path. `get_cursor_position` on
`CrosstermBackend` issues a DSR query and blocks on the reply, which would both
stall the frame and race §13.1's stdin owner. It is called exactly once, at
construction.

**Full redraw** — on width change, height change, and content shrinking below
the high-water mark — means **repaint every row from `viewport_top` to the last
screen row**. Rows already in scrollback belong to the terminal and are never
rewritten. On a width change the old `viewport_top` is meaningless because
re-wrapping moves every row, so it is recomputed as
`next.len().saturating_sub(height)`.

### 4.2 Rows, not lines — and where the wrap comes from

The diff index space must be **physical rows**. If one `Line` wraps to three
rows, arithmetic over logical-line indices drifts by two and the drift compounds.
So wrapping happens in step 1 and the writer never sees a logical line.

**ratatui's own wrap is not reusable.** `mod reflow;` is private
(`widgets.rs:36`), and `Paragraph::line_count(width)` returns only a count.

So the wrap is ours, over public API: `Line::styled_graphemes(base_style)`
(`text/line.rs:477`) yields `StyledGrapheme`s — ratatui already depends on
`unicode-segmentation` and does the clustering for us — and each grapheme's
display width comes from `unicode-width`. Spans are split at the break with
style carried through.

**Dependency consequence, stated rather than buried:** `unicode-width` becomes a
direct dependency of `bitrouter-tui`. It adds **zero new crates to the build
graph** — it is already in `Cargo.lock` as a ratatui dependency — but it is a
manifest change and §18 carries it.

`Paragraph::line_count(width)` stays useful as a **test oracle**: the workspace
already enables `unstable-rendered-line-info` for it, and cross-checking our row
count against ratatui's is a cheap invariant.

### 4.3 The above-viewport rule — a deliberate divergence from pi

A change can land on a row that has already scrolled off. pi detects this and
escalates:

```
// Differential rendering can only touch what was actually visible.
// If the first changed line is above the previous viewport, we need a full redraw.
if (firstChanged < prevViewportTop) { fullRender(true); return; }
```

pi's `fullRender(true)` emits `\x1b[2J\x1b[H\x1b[3J` — clear screen, home, clear
scrollback (`tui-main-screen.ts:215`) — and then re-emits the complete document.
**Being fair to pi:** it does not lose its own transcript, since it reprints it.
What it loses is anything the *terminal* held that pi did not author — output
from before launch, other programs' output, and any of pi's own history past the
emulator's scrollback cap. For a tool whose transcript is the artifact and which
shares a terminal with the user's shell, that is the wrong trade.

> **Rule.** The paint range is clamped to `[max(first_changed, viewport_top),
> last_changed]` (§4.1 step 4). A change above `viewport_top` is **not painted**,
> and `prev` is still updated (§4.1 step 7). Scrollback is never cleared.

The `prev` update is what makes "drop" mean *a decision not to paint* rather
than *a decision not to record*. Without it, `prev` and `next` would stay
divergent above the fold, `first_changed` would resolve there on every
subsequent frame, and the writer would never paint anything again.

**The accepted cost.** A tool call that scrolls off while still in progress
keeps its last-painted status forever; scrolling back shows `◍ Edit src/lib.rs`
on a call that has since completed. §14.2 carries the obligation to document
this where a user meets it.

### 4.4 What we build on

The writer targets **`ratatui::backend::Backend`**, not raw crossterm. Verified
present on the trait and implemented by both `CrosstermBackend` and
`TestBackend` under the features this workspace pins: `draw`, `append_lines`,
`set_cursor_position`, `get_cursor_position`, `clear_region(ClearType)`, `size`,
`flush`.

**Not available:** `scroll_region_up` / `scroll_region_down` are behind
`#[cfg(feature = "scrolling-regions")]` (`backend.rs:360`, `:382`; the backend
impls at `crossterm.rs:278` and `test.rs:369` carry the same gate). That feature
is non-default and this workspace does not enable it, so those methods are not
on the trait as we compile it. §19.4 prices enabling them.

| | |
|---|---|
| Drop | `ratatui::Terminal`, `Viewport::Inline`, `insert_before`, `LIVE_ROWS` and its const assert, the `unstable-rendered-line-info` feature comment (the feature itself stays, now for tests) |
| Keep | `ratatui::{Line, Span, Style, Buffer}`, `Line::styled_graphemes()`, `Backend`, `CrosstermBackend`, `TestBackend` |
| Add | `unicode-width` as a direct dependency (§4.2 — zero new crates), and one synchronized-output seam |

**The synchronized-output seam.** `BeginSynchronizedUpdate` /
`EndSynchronizedUpdate` (DECSET 2026) are not on the `Backend` trait. Rather
than special-casing crossterm inside the writer, define a one-method
`SyncSink` that `CrosstermBackend` satisfies through its ungated `io::Write` and
`TestBackend` satisfies as a no-op. **Emit mode 2026 unconditionally** — an
unrecognised DEC private mode is ignored by every conforming terminal, so the
worst case is the brief tear §4.4 already accepts. No capability detection, no
query round-trip.

**Testing, scoped honestly.** `TestBackend` maintains a real `scrollback:
Buffer` with `scrollback()`, `assert_scrollback()`, and
`assert_scrollback_empty()` (`backend/test.rs:110–167`), populated by
`append_lines` when the count exceeds the rows below the cursor (`:325–338`). So
the writer's *intent* — region bookkeeping, clear ranges, scrollback accounting
— is testable with no new dependency. What that does **not** test is the escape
bytes, which are `CrosstermBackend`'s business, and the two backends' cursor
models differ: `CrosstermBackend::append_lines` is `for _ in 0..n { Print("\n") }`
(`crossterm.rs:246`) while `TestBackend::append_lines` sets
`new_cursor_x = min(cur_x + 1, width - 1)` (`test.rs:317`). Hence §4.1 step 6's
rule that the writer always sets the cursor explicitly afterward.

### 4.5 The footer replaces the status row

Cost, route, mode, and the open permission prompt become the **last rows of the
document**, repainted in place. No pinned region, no borrowed rows to return.
`finish()` reduces to leaving the cursor below the document.

The footer is re-emitted as the document tail on every frame, so it is always
the newest content and always in the live region — which is what makes §4.3's
stale-glyph cost bounded to individual scrolled-off tool calls rather than to
the session's own summary. §19.4 is therefore about whether the footer should
*additionally* be pinned, not about whether it is reachable.

### 4.6 Diagnostics and force-redraw

§8.2 and §11.1 of the prior spec put BitRouter's tracing and the agent child's
stderr into one per-session file so nothing else writes to this terminal. A
writer owning more rows makes that more important. **Ctrl-L** forces a full
redraw for when something writes anyway.

## 5. Decision 3 — render scheduling

`apply` returning `()` removes the old answer to *who renders, when*. Streamed
chunks arrive per token; rendering per chunk would repaint hundreds of times a
second.

- The journal bumps a **per-entity revision**; it never writes. *As built it
  does not also carry a dirty flag: the driver owns the one that decides when a
  frame is due, and a second copy in the journal was written on every update and
  read by nobody.*
- The driver renders **at most once per ~30 ms tick**, and only when dirty.
- **Immediate, bypassing the tick:** a permission request appearing or clearing,
  a turn settling, and a key press. These are user-visible latency, not
  streaming noise. *A resize trigger is specified below but not yet wired — see
  the note under §5's resize bullet.*
- **Preemption:** an immediate render cancels any pending tick render and clears
  the dirty flag, so at most one frame is ever in flight.
- **Resize** is observed as `crossterm::event::Event::Resize` on §13.1's single
  stdin owner, `apps/bitrouter/src/chat/input.rs`, and triggers an immediate
  full redraw.

  > **Not yet wired.** Nothing observes `Event::Resize`, so there is no resize
  > trigger and the scheduler carries no variant for one. A resize is still
  > *handled* — `Writer::frame` re-reads the terminal size and re-wraps on every
  > frame — but only on the next frame something else schedules, so a resize at
  > an idle prompt is not repainted until the user types. Wiring this is the
  > remaining work for this bullet.

**Per-entity render cache.** Each journal entity caches its rendered rows keyed
on `(width, revision)`, invalidated when `apply` touches it. A frame therefore
concatenates cached row vectors and re-renders only dirty entities, so §4.1
step 1 is not an O(document) re-derivation 33 times a second. §6's hunk
computation is part of the cached value and runs once per diff, not once per
frame.

**Relation to pi, stated accurately.** pi uses a trailing-edge throttle with a
16 ms floor that paints on the next tick when idle. Ours is the same shape with
a longer interval, chosen because an agent transcript has no animation and 30 ms
halves the wakeups. It is not "pi minus configurability" — pi hardcodes its
interval too.

## 6. Decision 4 — fix the three defects

- **Tool calls render as one entity.** Status, title, and content live on the
  `ToolCall` and repaint in place, subject to §4.3.
- **`ToolCallContent::Content` renders.** Text renders as text; a `Terminal`
  embed renders as a labelled reference. Non-text `ContentBlock`s render a
  one-line description (`[image 1.2 MB]`, `[resource: file:///…]`).
  **Capped at 40 rows**, then `… 1,240 more lines`, for the same reason diffs
  are capped: unbounded output is unrevisable once it scrolls past §4.3's fold.
- **Diffs render as hunks.** Line-diff `old_text` against `new_text`, show
  changed hunks with **three lines of context**, summarize the remainder
  (`… 480 unchanged lines`), and cap one diff at **one terminal height** of
  rows. **The summary always names the absolute file path**, because
  expand/collapse is deferred (§15) and a summary with no path is a dead end.

**The line diff needs an implementation the workspace does not have.** The only
`diff` crate in `Cargo.lock` is 0.1.13, transitive under `pretty_assertions`
(dev-only). See §19.2 — recommendation is a production dependency, because
subtly wrong diff output is worse than a dependency.

## 7. Decision 5 — one trait, one registry, one caller-supplied footer

Rev 2 specified two registries. The second could not express either of its only
two clients, so it is **deleted**.

```rust
pub trait ToolRenderer {
    fn render(&self, ctx: &ToolContext<'_>) -> Vec<Line<'static>>;
}

pub struct ToolContext<'a> {
    pub id: &'a ToolCallId,
    pub kind: ToolKind,
    pub status: ToolCallStatus,
    pub title: &'a str,
    pub content: &'a [ToolCallContent],
    pub width: u16,
}

/// Registry key. `ToolKind` itself cannot be one: the schema derives
/// `PartialEq, Eq` but neither `Hash` nor `Ord` (`v1/tool_call.rs:428`), and
/// the orphan rule blocks a local derive. This mirror also gives the
/// `#[non_exhaustive]` wildcard a defined landing spot.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolKey { Read, Edit, Delete, Move, Search, Execute, Think, Fetch, SwitchMode, Other }

impl From<ToolKind> for ToolKey { /* unknown variants fold to Other */ }
```

`ToolContext` omits `expanded` (expand is deferred, §15), `raw_input`, and
`locations` — no v1 renderer reads any of them, and adding a field before a
reader exists is rule-4 dead code.

**Why the second registry is gone.** Its two intended clients were the cost line
and the provider picker, and neither is a `_meta` surface:

- The cost line needs `usage.cost.{amount, currency}` from the typed
  `UsageUpdate`; only the *scope* lives in `_meta` (`cost.rs:73–86`). A renderer
  handed a bare `_meta` `Value` cannot produce `USD 0.4200`.
- The picker is not a session update at all. `Picker::open` takes
  `&[ProviderInfo]` from an out-of-band `providers/list`
  ([`picker.rs:44`](../crates/bitrouter-tui/src/picker.rs:44),
  [`acp_cli.rs:938`](../apps/bitrouter/src/acp_cli.rs:938)), and `choose(char)`
  needs key input no render trait expresses.

**What replaces it: a caller-supplied footer.** The document renderer takes
`footer: &[Line<'static>]` from the driver. The app composes it — cost, route,
and any router surface — from data the app already holds. This is the same
exemption §10 grants `log_tail`: not everything needs a registry, and a registry
with no expressible entries is exactly the dead weight §11 refuses.

One registry keyed on `ToolKey` remains, with a default renderer for anything
unregistered. v1 registers three or four.

## 8. Decision 6 — the crate boundary moves to where it was aimed

> **PARTLY SUPERSEDED 2026-08-16.** `picker.rs` moved back into
> `crates/bitrouter-tui`, and `cost.rs` split rather than staying whole. The
> rule this section states — the boundary is drawn on BitRouter-specific
> *knowledge*, not on the fact of rendering — is unchanged and is what the
> reversal applies. What was wrong was the classification, on evidence this
> section did not check:
>
> - **`picker.rs` names no BitRouter knowledge.** `ProviderInfo` is an
>   `agent-client-protocol-schema` type (`src/v1/agent.rs`, and it survives
>   into `v2`), and the crate already resolves `unstable_llm_providers`, so the
>   type was in scope there the whole time. The argument below — *"`providers/*`
>   is BitRouter's, no generic agent serves it"* — is about who implements the
>   **server** side, which is not what the charter asks. `Cargo.lock` carries
>   one schema version, so the move was a `git mv` plus one import line.
> - **The "caller composes it" rationale was already satisfied by a parameter.**
>   `Picker::open(available, …)` takes the capability answer as an argument; the
>   module's *location* never contributed to it.
> - **`cost.rs` splits cleanly**, contrary to the claim below that it cannot.
>   That claim holds against the split rev 2 proposed — `from_usage` reads the
>   constant — but not against the one that ships: `Cost::new(amount, currency,
>   scope)` renders in the crate, while `COST_SCOPE_META_KEY`, the wire spelling
>   and `from_usage` stay in the app. That is §4.9's existing idiom, where what
>   the protocol does not carry arrives from the caller.
>
> Also in that change: `tui/lifecycle.rs`'s crossterm half moved to the crate
> (terminal custody was split across the boundary with an ordering contract
> documented on both sides), and `snapshot.rs`'s `Scope` was deleted — with
> `as_wire`'s only caller gone it had no reader, and `Snapshot.scope` never had
> one. Before the watcher was removed, **`status --watch` therefore drew an
> unlabelled spend figure**, which is the honesty rule below applied to `chat`
> and not to the bar. The current replacement is `status --requests`; it is a
> text snapshot, not a ratatui bar.

`bitrouter-tui`'s doc says it *"must never depend on `bitrouter`"* and that the
build enforces it. True — but the useful boundary is against **BitRouter-specific
knowledge**, not just against the **crate**. The shipped split keeps the
`bitrouter/costScope` `_meta` spelling in the app, while the crate renders typed
cost scope and ACP `ProviderInfo` values supplied by the app.

- `crates/bitrouter-tui` keeps the writer, journal, `ToolRenderer`, the registry,
  the generic ACP renderers, the cost-line renderer, and the provider picker.
  It may know ACP schema types and typed values supplied by the app; it still
  must not depend on the `bitrouter` app crate.
- `cost.rs` stays split at the boundary that compiled: the crate renders
  `Cost` and `Scope`, while the app owns the `bitrouter/costScope` `_meta`
  spelling and converts `UsageUpdate` into that typed scope before calling the
  renderer.
- `picker.rs` stays in the crate because it renders `ProviderInfo`, an ACP
  schema type. The app decides whether the `providers/*` surface is available
  and performs any `providers/set` call after a selection.

There is deliberately no `ratatui` dependency in the app crate; constructing
terminal rows remains inside `bitrouter-tui`. The app names ACP schema types
directly where it has to bridge protocol updates into renderer inputs — §19.5.

**A guard, because BitRouter-specific knowledge still crosses the boundary.**
The renderers may not reference `Config`, the metering store, or the control
socket, and take only values already on the ACP wire or explicit caller-supplied
scope/availability decisions. §16 asserts it.

**`Capabilities` is deleted, not collapsed.** It is constructed nowhere outside
its own tests today — the live gate is the app's `can_reroute()`. `providers`
becomes "the app composes a route footer when it has one"; `load_session` is a
behavioural capability with no current reader and goes with it.

## 9. Decision 7 — draw every variant the stream already carries

**Correcting rev 2's premise.** Rev 2 framed this as "`translate.rs` forwards
them and the renderer drops them again." That is the wrong path: the TUI
subscribes to `session.raw_updates()`
([`engine.rs:496`](../crates/bitrouter-sdk/src/acp/engine.rs:496)), which is the
untranslated stream, so these variants **already reach the renderer**. They die
at [`transcript.rs:107`](../crates/bitrouter-tui/src/transcript.rs:107)'s
`_ => self.flush()`. `translate.rs` matters for the manager path, not this one.

Render all five:

| Variant | Where it lands in the document |
|---|---|
| `AvailableCommandsUpdate` | the agent's slash commands, listed on request |
| `Plan` | a plan block in document order |
| `CurrentModeUpdate` | the footer |
| `ConfigOptionUpdate` | the footer |
| `SessionInfoUpdate` | the title, in the footer |

`PlanUpdate` / `PlanRemoved` are **excluded**: they sit behind the schema's
`unstable_plan_operations` feature and the workspace pins only
`unstable_llm_providers` (`Cargo.toml:57`), so they do not exist in the compiled
schema. `Plan` itself is unconditional in v1.

`AvailableCommandsUpdate` matters most: it is how an agent advertises its slash
commands, and today `/route` is hardcoded while the agent's own are invisible.

**And wire cancel.** `Session::cancel()` exists
([`engine.rs:478`](../crates/bitrouter-sdk/src/acp/engine.rs:478)) and `chat`
never calls it; the turn loop
([`acp_cli.rs:850`](../apps/bitrouter/src/acp_cli.rs:850)) has no key-event arm.

**Key bindings, v1** (§18 carries them to `CLI.md`):

| Key | Meaning |
|---|---|
| `Esc` | consumed by the innermost open modal (permission, then picker); reaches turn-cancel only when none is open |
| `Ctrl-C` | cancels a running turn; exits when idle |
| `Ctrl-D` | ends the session (idle only) |
| `Ctrl-L` | force full redraw (§4.6) |

Cancelling a turn with a permission outstanding **auto-denies it** via the
existing `Prompt::deny()` path, so a cancelled turn never resolves to consent.

## 10. Decision 8 — the surfaces v1 already ships

- **`permission.rs`** — stays in the crate, still a modal, but rendered as
  footer rows (§4.5) and repainted in place. Safe defaults kept verbatim. Its
  **4 tests survive as-is**.
- **`log_tail.rs`** — stays, still called on abnormal exit, still the pure
  `(&Path, &str, usize) -> Vec<Line>` it already is. It is written **directly to
  stdout after teardown has restored the terminal**, not through the writer:
  abnormal exit is exactly when `prev`/`anchor` are least trustworthy. **3 tests
  survive as-is; 1 has its fixture normalized** (§16.4).
- **`cost.rs`** — stays in the crate as the renderer for typed `Cost` /
  `Scope`; app-side `_meta` parsing supplies the scope (§8). **3 tests stay in
  the crate; app-side scope parsing keeps its own tests.**
- **`picker.rs`** — stays in the crate because it renders ACP `ProviderInfo`;
  the app supplies availability and performs `providers/set` after selection
  (§8). **5 tests stay in the crate.**
- **`viewport.rs`** — **deleted** with its **3 tests**, which assert behaviour of
  the `Inline` type being removed. §16.3 replaces them.
- **`transcript.rs`** — becomes `journal.rs` plus renderers. Its **8 tests** are
  ported.
- **`lib.rs`** — `Capabilities` deleted (§8), so its **2 tests** are deleted.

Against the 31 tests passing today, the shipped split keeps the cost/picker
renderer tests in the crate, ports the transcript coverage to journal/render
tests, replaces the deleted viewport tests, and deletes only obsolete
`Capabilities` coverage.

## 11. Decision 9 — no error isolation, no plugin loading, and the trigger

pi wraps extension renderers in try/catch and substitutes an error line
(`coding-agent/src/modes/interactive/components/custom-entry.ts`, ~12 lines of
isolation in a ~60-line file). opentui goes further with a 402-line
`SlotRegistry`.

**We do neither.** Every registered renderer is code in this repository, and
CLAUDE.md rule 3 already forbids the panics such isolation would catch.

**Trigger to revisit:** the first renderer registered from outside this
workspace. Then pi's shape — not opentui's — is the one to copy.

## 12. Sizing

Today: **792 production / 667 test / 1,459 total.**

New and rewritten work:

| | production lines |
|---|---|
| Differential writer (wrap, diff, cursor, `append_lines`, sync seam) | ~450–600 |
| Journal | ~250 |
| Renderers (message, thought, tool, diff-hunks, plan, commands, footer) | ~500 |
| Trait + registry + `ToolKey` | ~100 |
| Stdin owner + single-line editor (§13.1) | ~300–450 |
| Driver changes (scheduling, cancel arm, footer composition) | ~200 |
| **Total new/rewritten** | **~1,800–2,100** |
| Tests, at the existing ratio | ~1,500 |

Roughly **3,300–3,600 lines** of new and rewritten code. Surviving production
code retained across the boundary (permission, log_tail, cost, picker ≈ 390
lines) is not counted here. Like its predecessor, this spec is not a subtraction.
§4 is the majority of the risk.

## 13. Prerequisites

### 13.1 One owner of stdin, in raw mode, with a line editor

The driver today reads prompts via `tokio` `BufReader::lines`
([`acp_cli.rs:823`](../apps/bitrouter/src/acp_cli.rs:823)) while `pick_provider`
([`:948`](../apps/bitrouter/src/acp_cli.rs:948)) and `answer_permission`
([`:1032`](../apps/bitrouter/src/acp_cli.rs:1032)) each open a crossterm
`EventStream` on the same fd and toggle raw mode per modal. That is already a
hazard — read-ahead can swallow typed bytes and raw mode flips under the user
mid-type — and §9's escape binding, which needs a key reader live *during* a
turn, makes it permanent.

**Decision: one owner, raw mode for the session.** The unavoidable consequence,
which rev 2 hid: raw mode clears ISIG and canonical line discipline, so **we own
Ctrl-C and Ctrl-D** (§9 defines them) and we must supply echo, backspace,
word-delete, and bracketed paste ourselves. That is a v1 line editor — single
line, no history — sized at ~300–450 lines in §12 and listed in §15. The
multi-line composer with history stays deferred.

### 13.2 Terminal restoration

Session-long raw mode destroys the property
[`viewport.rs:21`](../crates/bitrouter-tui/src/viewport.rs:21) is proud of —
*"no alternate screen, no lifecycle save/restore, and no panic hook"* — because
nothing was taken from the terminal. Now something is.

**Reuse what the repo already has.** `apps/bitrouter/src/tui/lifecycle.rs`
survived Phase 1.2 and provides a handle-free `restore()` (`:44`) plus
`install_panic_restore()` (`:56`), written so a panic hook or a signal branch can
call it; the same module now owns `Shutdown::install()` for INT/TERM/HUP. All
three exits — normal, panic, signal — must call `restore()`, which at minimum
disables raw mode.

### 13.3 Non-TTY

A differential writer piped to a file is worse than v1's. In v1 `chat` gates on
`std::io::stdout().is_terminal()` and falls back to plain line output when
false, so §15's deferred plain-fallback row is a feature gap rather than an
unhandled path.

### 13.4 Caveat, not a blocker

The final gate still needs the normal CI suite; earlier drafts called out a
separate `ubuntu-latest` hang, but the plan should now treat §16.5 as the source
of truth for current verification.

## 14. Risks

1. **The writer is the hard part.** Cursor bookkeeping across wrapping, resize
   and scroll is where pi spends most of 586 lines and keeps a `PI_DEBUG_REDRAW`
   hatch. Bias to full redraw whenever state is uncertain, keep the same debug
   hatch, and land it behind §16's harness first.
2. **§4.3's stale rows will be reported as bugs.** A deliberate trade; §18
   carries the obligation to document it in `references/cli.md`'s `chat`
   section.
3. **Rows vs lines is the likeliest corruption source.** Every wrap must be
   exercised with CJK and emoji fixtures, cross-checked against
   `Paragraph::line_count`.
4. **`TestBackend` does not model escape bytes**, so a green harness is not
   proof of correct output. §16.3 pairs it with one manual smoke.
5. **Boundary drift can reintroduce app knowledge into renderer code.** §8's
   guard plus §16's check.
6. **Scope creep toward a toolkit.** §2's non-goals and §11's trigger.

## 15. Scope by version

Proposed for v1, **contingent on §19.1**:

| | v1 | later |
|---|---|---|
| Journal replaces `Transcript` (§3) | ✅ | |
| Differential writer over `Backend`, incl. §4.3 (§4) | ✅ | |
| Render scheduling + per-entity cache (§5) | ✅ | |
| Tool calls render as one entity (§6) | ✅ | |
| `Content` and non-text blocks render, capped (§6) | ✅ | |
| Hunked diffs with path-naming summary (§6) | ✅ | |
| One trait + one registry + caller footer (§7) | ✅ | |
| `cost.rs` / `picker.rs` remain in `bitrouter-tui`; app supplies metadata scope and availability (§8) | ✅ | |
| Five forwarded variants render (§9) | ✅ | |
| Key bindings: Esc, Ctrl-C, Ctrl-D, Ctrl-L (§9, §4.6) | ✅ | |
| Single stdin owner, raw mode, single-line editor (§13.1) | ✅ | |
| Terminal restoration on all three exits (§13.2) | ✅ | |
| Non-TTY gate (§13.3) | ✅ | |
| Expand/collapse a tool result or diff | | ✅ |
| Multi-line composer with history | | ✅ |
| Theming, `NO_COLOR`, non-TTY plain rendering | | ✅ |
| Unknown-update-kind registry (needs v2) | | ✅ |
| Pinned footer via `scrolling-regions` (§19.4) | | ✅ |
| Error isolation / external renderers (§11) | | on trigger |

## 16. Acceptance

1. **Journal tests are pure.** All 8 `transcript.rs` tests get equivalents,
   including `interleaved_voices_keep_their_order_and_nothing_is_lost`. Plus:
   the sticky message-keying rule (§3) with `None` chunks continuing a run.
2. **Writer tests use `TestBackend`**, asserting the visible region via
   `buffer()` and finalized content via `assert_scrollback()`. Scrollback
   assertions require a fixture document taller than the backend.
3. **Named regressions:** one tool call yields one entity across three status
   updates; an `Execute` call's output appears and is capped; a one-line edit to
   a 500-line file produces a bounded row count naming the path; teardown leaves
   every rendered row either in `buffer()` or `scrollback()` with none erased; a
   resize repaints the live region without corrupting it; CJK and emoji lines
   wrap to a row count matching `Paragraph::line_count`; **a change above
   `viewport_top` is not painted, `prev` is still updated, and the next frame
   still paints a below-fold change** (§4.3); one manual smoke against a real
   terminal, because §14.4.
4. **One assertion per remaining v1 row:** a registered `ToolKey` dispatches and
   an unregistered one falls through to the default; each of §9's five variants
   renders; `Esc` cancels a turn and auto-denies an outstanding permission;
   `Ctrl-L` forces a redraw; the scheduler coalesces N chunk updates into one
   frame and renders a permission immediately; the non-TTY gate refuses.
5. **Boundary checks, all satisfiable.**
   ```bash
   grep -rn "bitrouter/" crates/bitrouter-tui/src   # must return nothing
   ```
   The crate contains no app-specific route namespace or `_meta` key spelling.
   Additionally: `cargo tree -p bitrouter-tui` shows no dependency on the
   `bitrouter` app crate; `crates/bitrouter-tui/src/viewport.rs` no longer
   exists; `crates/bitrouter-tui/src/{cost.rs,picker.rs}` exist and contain no
   app-crate imports; and `apps/bitrouter/Cargo.toml` has no direct `ratatui`
   dependency (§8's guard).
6. Full workspace `cargo nextest run --all-features`, `cargo clippy
   --all-features`, `cargo fmt -- --check` clean — see §13.4 on sequencing.

## 17. Rejected alternatives

- **Keep `insert_before`; fix defects 2 and 3 only.** The correct fallback if §4
  is rejected. It cannot fix defect 1, cannot stream, and keeps the one-row
  ceiling. Reduced v1 scope under this fallback: §6's second and third bullets
  and §9, nothing else.
- **pi's escalation on above-viewport changes.** Correct for pi, wrong for us
  (§4.3).
- **Raw crossterm writes instead of `Backend`.** Forfeits `TestBackend` and its
  scrollback model, and forces a VT parser into the dev-dependency graph Phase
  1.3 emptied.
- **A second registry for `_meta` surfaces.** Rev 2's design; neither of its two
  clients fits it (§7).
- **opentui's architecture.** Aimed at third-party widget authors across three
  framework bindings; `Renderable.ts` alone exceeds this whole crate.
- **opentui's `split-footer` mode.** Reintroduces the pinned region §1.4 argues
  against, and its capture machinery duplicates the per-session log.
- **Alternate screen with a sticky dock.** pi's `TuiAltScreen` is the honest
  mechanism if a fixed composer above a scrolling transcript becomes the
  requirement. Rejected here because it forfeits real scrollback, terminal
  search, and native selection.
- **Waiting for ACP v2.**

## 18. Lockstep obligations

This spec **does** change user-facing surface.

- [`CLI.md`](CLI.md) and `skills/bitrouter/references/cli.md` gain §9's key
  binding table **and** a note that `chat` now holds the terminal in raw mode
  for the session, so enter, Ctrl-C and Ctrl-D are handled by our line editor.
- `references/cli.md`'s `chat` section documents §4.3's stale-row behaviour
  (§14.2).
- `Cargo.toml` gains `unicode-width` (§4.2) and, if §19.2 resolves that way, a
  diff crate. Neither is conditional on the other.
- [`ACP_TUI_SPEC.md`](ACP_TUI_SPEC.md) §8 gains a note that its reading of pi's
  main-screen/alt-screen split was half the story (§1.4) and that its status
  line is superseded by §4.5's footer.
  [`ACP_TUI_PLAN.md`](ACP_TUI_PLAN.md) §C decision 7 and task 4.5 gain a pointer
  here, since `Viewport::Inline` is the one mechanism this spec replaces.
- No flag, port, env var, or default config changes, so the plugin manifests at
  `.claude-plugin/`, `.codex-plugin/`, and `.agents/plugins/marketplace.json`
  need no edit. [`docs/README.md`](README.md) indexes by glob.

## 19. Open questions

1. **Is §4 accepted?** The load-bearing decision. §17's first entry is the
   fallback, and names the reduced scope.
2. **Line diff: dependency or hand-rolled?** (§6.) Recommendation: a production
   dependency. Needs approval — it is the only new *shipped* crate, since
   `unicode-width` adds none.
3. **Is §4.3's stale-row trade acceptable?** It is the price of never clearing
   scrollback. If not, the honest third option is the alternate screen (§17),
   which reopens a settled decision.
4. **Should the footer additionally be pinned?** §4.5 makes it the document tail,
   so it is always the newest content — but it still scrolls with the document.
   Pinning it needs `Backend::scroll_region_up`/`_down`, which means enabling
   ratatui's `scrolling-regions` feature (§4.4) — a workspace manifest change.
   Recommend deferring (§15) and revisiting if users report losing the cost
   readout.
5. ~~**How does the app name ACP schema types after §8's split?**~~ Answered:
   `apps/bitrouter` imports `UsageUpdate` and `ProviderInfo` through
   `agent_client_protocol::schema::v1::*`; the renderer crate depends directly
   on `agent-client-protocol-schema` for the typed `ProviderInfo` it renders.
