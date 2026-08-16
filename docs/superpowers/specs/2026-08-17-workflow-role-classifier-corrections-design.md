# Workflow Role Classifier Corrections Design

## Objective

Correct the five routing failures reproduced by the adversarial review while
preserving the unified `agent_route/v1|<task-family>|<role>|<risk>` interface,
the deterministic scorecard, and the existing policy fallback order.

## Scope

The change is deliberately limited to:

1. Scope action history to the latest causal instruction epoch so a completed
   prior request cannot override a new user request or keep its risk guarded.
2. Use the existing ASCII boundary semantics for role instruction phrases as
   well as task-family phrases.
3. Accept one explicit `implement`, `verify`, or `finalize` instruction at the
   existing minimum score and resolve mixed mutate/review wording by the first
   action expressed in the instruction.
4. Resolve code-review versus code-debugging precedence by the first explicit
   intent, rather than by the mere presence of `review` or `audit` anywhere.
5. Classify shell actions by a bounded command head, with read commands taking
   precedence over test-like text in their arguments.

The change does not add task families, multilingual lexicons, statistical or
neural classifiers, workflow state machines, or new fallback layers.

## Design

### One causal instruction value

Replace the text-only instruction lookup with one private value containing the
lowercased instruction text and the source message index. Compute it once in
`predict_next_step` and pass its text to both classifiers.

Structured tool calls and results are accumulated only from that message index
forward. A genuine later user instruction therefore starts a fresh epoch. For
Terminus-style normalized action history, consume the aggregate only when an
actual normalized assistant action follows the selected instruction; otherwise
the aggregate belongs to an older epoch and is ignored.

This retains the useful progression inside one request:

```text
user instruction -> read -> edit -> test -> next prediction
```

and resets it at a new request:

```text
old instruction -> edit -> test -> new user instruction
                                      ^ new epoch
```

### Shared boundary matcher

Rename the existing task-only boundary helper to a general private phrase
matcher and use it for role terms too. Keep raw substring matching only for
concrete evidence markers such as file suffixes and `src/`, where word-boundary
semantics would be wrong. Remove trailing spaces from read-action terms so they
remain valid bounded phrases.

### Immediate-action ordering

When both mutation and verification phrases are present, compare the first
bounded occurrence of each. The earlier phrase describes the immediate next
step. This makes `Fix the bug found in code review` an implementation request
and `Review the bug fix` a verification request without adding parsing layers.

The task-family classifier applies the same ordering only to review versus
debugging intent. Existing review-first cases remain review, while fix-first
cases become debugging.

Raise the ordinary mutation, verification, and finalize score weights to the
existing minimum top score of five. Concrete mutation retains its higher
weight.

### Command-head classification

Match read and test command terms only at the trimmed command start with an
end-or-whitespace boundary. Check read heads before test heads. Thus
`rg 'cargo test' README.md` is a read while `cargo test -p bitrouter` remains a
test. No shell grammar or command execution is introduced.

### Contract and template evidence

Increment the signed algorithm component versions affected by causal history,
instruction matching, task classification, and action classification. The
compiled predictor digest therefore changes. Refresh the auto-router template's
predictor descriptor and its canonical compiler/evidence hashes using the
repository's existing digest tests; do not introduce compatibility admission
for the old descriptor.

## Testing

Add real-path regression tests for:

- a summarize request after a completed read;
- a new implementation request after a successful old mutation/test sequence;
- old failures not carrying guarded risk across a new instruction epoch;
- `latest`, `address`, `explanation`, and `await` not producing action signals;
- plain implement, verify, and summarize instructions selecting their roles;
- review-first and fix-first mixed instructions selecting matching family/role;
- read commands containing test text and genuine test commands;
- normalized history being retained within an epoch and ignored after a pivot.

Run the focused predictor and cross-harness tests, template/lock tests, then the
repository-required full test, Clippy, format, and diff checks.

## Non-goals

- Changing route-key schema or policy lookup order.
- Adding new policy tiers, routes, models, or task families.
- Expanding the classifier to arbitrary languages.
- Reclassifying generic shell/file pipelines as agent workflow execution.
- Adding fallback, retry, or defensive parsing layers.
