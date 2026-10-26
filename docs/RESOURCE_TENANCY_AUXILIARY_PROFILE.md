# Owned OAuth and controlled input counting

These consumers extend the existing execution owner and authority transaction
actor. They add no public endpoint, public authority type, service or Cargo
feature. Production tenant dispatch, revision-bound dispatch and strict receipts
remain disabled until the remaining implementation and qualification gates pass.

## OAuth credential lineage

An installation manifest may explicitly import a Claude Code OAuth seed and its
known Anthropic registration. The profile pins provider, account, token endpoint
and client ID. Production accepts only the existing registration values; tests
use an explicit loopback exception. No provider-name lookup, host credential file
or ambient auth plugin can supply a missing owned credential.

Migration 037 adds encrypted live credential slots and private auxiliary receipt
facts. The retained resource revision describes the immutable seed lineage. The
live slot carries a monotonic generation and Ready, RefreshStarted or quarantined
state. Reimporting an identical seed preserves the live generation and ciphertext;
it cannot restore a missing slot or replace another revision occupying that slot.
Startup authenticates both the encrypted token and its retained resource lineage.

The current OAuth consumer supports retained HTTP Messages routes to Claude Code,
with one candidate per selected chain and no learned policy or passthrough headers.
Reserved auth and Claude Code headers cannot be overridden by fixed header rules.
Native Generation OAuth remains unsupported and is rejected before provider I/O.
Static native Messages requests keep their existing behavior. The native OAuth
refusal is a remaining integration boundary, not evidence that all provider
profiles are complete.

Every refresh belongs to the original execution and exact credential generation.
Current permissions are checked after credential reads and immediately before
the refresh POST and model request. RefreshStarted is committed before sending.
A successful response yields one encrypted replacement retained by the execution
owner until its durable commit is acknowledged. Revocation during refresh can
prevent the model request while still permitting the admitted replacement to be
settled. A stale generation cannot authorize a model send.

The refresh uses one POST with redirects, automatic retries and proxy discovery
disabled. Its budget is at most 30 seconds and never exceeds the execution's
remaining deadline. The response is bounded at 256 KiB, including chunked bodies
without Content-Length. Access and refresh tokens each have a 64 KiB bound;
serialized token JSON is bounded at 256 KiB. At most 256 slots are retained.
Invalid responses and ambiguous sends cannot trigger another refresh. SQL
uncertainty retains the real owner and its database file lease.

## Controlled input counting

The current counting consumer is native Generation through one fixed Responses
candidate using static credentials, with no learned policy or passthrough headers.
It rejects server-managed continuation references and OAuth. The initial count,
optional context reduction and optional second count stay within the original
execution, candidate and credential identity.

The private count proof binds the prompt and actual outbound request. Model send
requires the exact counted prompt, even if an embedding caller omits the public
counting extension. A context rebuild must reduce the original history and is
counted again before dispatch; it cannot substitute an unrelated conversation.
The expected generation request uses the same streaming choice as the actual
executor, including native stream observation. Count payloads omit
generation-only stream fields while the proof still binds the complete
authenticated generation request. Current authority and the original deadline
remain necessary after every wait.

## Receipts and cleanup

Refresh and counting facts occupy mutually exclusive, bounded auxiliary state in
the existing receipt owner. They do not create model attempts or invent model
token usage. Auxiliary facts are committed with the terminal receipt; reads and
startup inventory validate the immutable digest and unknown-work projection.
Started refreshes without a terminal receipt and later unknown terminal receipts
share one capacity entry for their execution.

An unclean auxiliary result prevents a success acknowledgement even when model
usage is known. Joining a known durable terminal record for cleanup does not
acknowledge successful result delivery. Unknown storage, unacknowledged credential
replacement and unfinished producers continue to prevent a clean shutdown.

Capture cleanup also retires a completed oneshot receiver before processing its
result. Cancellation before completion retains the receiver; completion with an
unknown transaction outcome retains the pending input and sticky uncertainty.
Cleanup must not poll a completed receiver or treat its removal as proof that a
request was never admitted.

## Qualification

Qualification is recorded in the implementation tracker after executing the
current source. Required coverage includes HTTP/1.1 and h2c JSON/SSE ingress,
revocation and generation races, real consuming COMMIT/ROLLBACK failures, file
lease custody, auxiliary accounting, reduced features and real database backends.
Ingress HTTP/2 checks do not establish upstream HTTP/2 behavior. These functional
tests do not establish throughput, RSS, provider cost or full cold recovery.
