# bitrouter-ai

Model semantics, bidirectional wire codecs and selected-target transports used
by BitRouter integrations and callers. Import each API from its owning module:

```rust
use bitrouter_ai::types::{Prompt, GenerateResult, StreamPart};
use bitrouter_ai::protocol::{InboundAdapter, OutboundAdapter, OutboundDispatch};
use bitrouter_ai::stream::{SseFrame, StreamError};
use bitrouter_ai::target::ModelTarget;
use bitrouter_ai::client::{HttpTimeouts, ModelClient};
```

The three built-in codecs support Chat Completions, Responses and Messages,
including ingress/egress and stream conversion. Transports
accept an effective `ModelTarget`, after the caller resolves routing, account
selection and credential/endpoint overrides. The target carries connection
values and model compatibility, with redacted credential debug output.

`ModelClient::generate` and `ModelClient::stream` invoke a single selected target
using a caller-supplied `CancellationToken`. Rendering clones the source prompt
and applies the selected model/stream mode to that projection. The client owns
connect/read timeouts and an optional total deadline; dropping a returned stream
releases its response. Stream EOF without a terminal part is an invalid response.
Previously emitted parts and usage remain visible before a later error.

Codex subscription calls require upstream SSE. `generate` selects that mode and
uses `stream::collect::collect_generate` to return one `GenerateResult`, leaving
the source prompt unchanged. The SDK uses the same collector while retaining
gateway request policy and its execution envelope. Collection waits through EOF,
preserves decoded block boundaries/tool identities and uses the last usage report.
Completed Responses reasoning uses the redacted `types::NativeReasoning` capsule
on `Content::Reasoning` and `StreamPart::ReasoningEnd`. The decoder retains the
completed `output_item.done` payload, preserving encrypted content, identity,
status and summary/content boundaries, including empty-summary items. Visible
summary and reasoning-text deltas keep their text lane, and missing terminal
suffixes are surfaced once. Responses output keeps each reasoning item in its
content position. A retained snapshot must match current visible text so it
cannot undo result/stream hooks. Native payload Debug omits the entire item.

Responses ingress also retains each native reasoning item, including encrypted-only
items, in source order. Malformed native input is a safe request error. The protocol
tag establishes format, not selected-target/account replay authority.

`conversion::ConversionReport` records content-free refusal reasons, structural
locations, wire categories, semantic effects and dispositions. `OutboundAdapter::admission`
and `ModelClient::render_request` share initial ingress-independent history projection,
native-reasoning and structured-output support rules. Native history remains rejected until authority is proven; the other
three client codecs return typed output incompatibility rather than losing the capsule.
The SDK's HTTP executor uses the same request preparation in router preflight. Excluded
candidates have their own observation callback and are not counted as attempted provider
failures. Each admitted candidate renders a fresh projection from the original Prompt.
An all-excluded chain returns the collected typed report; it never resets history.

Built-in `InboundAdapter::parse_request` also checks initial ingress omissions before
returning a Prompt. Unknown Responses items, unknown/unmapped message and tool-output
blocks, unsupported content values, system payloads that would lose structure produce request-ingress reports with original wire
indices. Unsigned Messages thinking is rejected rather than erased. These refusals
occur before gateway route selection or model execution and contain no raw type names
or payloads. Known heterogeneous input and promoted tool declarations remain supported.

Request projection also refuses the currently detected history omissions: reasoning
or citation/source blocks that would disappear, provider execution/server identity
that would become an ordinary client call, unsupported approval history/reasons,
and media/file references omitted from tool results. Native continuity tokens cannot
be discarded across protocol families or forwarded as plain reasoning text.

Provider-defined declarations use their existing native-family identity: Chat's
omission is refused, and foreign/unclassified translation is excluded. Non-object
provider-tool arguments cannot be replaced by an empty object. Same-family projection
is not a native-schema or model-capability certificate.

A Responses denial tagged for approval suppression must have a matching denied
approval response; otherwise its result cannot disappear. This local pairing check
is not complete tool-causality or selected-account replay authority validation.

Projection admission also checks explicit function/structured-output strict flags,
structured-output names/descriptions, omitted filenames and tool-result error/denial
status. Filename and schema-metadata omissions remain unclassified rather than assumed
nonessential. Messages retains error flags; an ordinary text result cannot stand in
for a typed error or denial. A paired Responses denial cannot silently lose its reason.
Google Chat uses per-model compatibility and a conservative supported schema/option
subset. Unknown constraints, explicit cache references and unverified strict flags
exclude the Google target without changing the source request.

Initial JSON/content-boundary admission rejects malformed argument strings where
Messages would replace them with `{}`, unsupported Chat ordering/concatenation,
and selected ingress attributes that disappear before a Prompt exists. Chat and
Messages preserve text-only tool-result boundaries; canonical JSON encoded into
string-only slots retains the complete value. Error/denial status is checked
independently. Reports retain classified JSON encoding in `admitted` and refusals
in `issues`.
`ModelClient::render_request_with_report` returns the selected body and assessment
without I/O/authentication. Older serialized reports default to no admitted effects.
The SDK observes eligible effects through `ObserveHook::on_conversion_admitted`
before hop start; an admission callback alone does not prove provider execution.
Custom executor preflight now returns the assessment, and the pipeline rechecks it.
Empty/unchanged assessments produce no conversion callback. Refused candidates
retain exclusion observations; local equivalent effects never waive a refusal.

A report without refusals passes only these initial rules. Remaining ingress attributes, media
subtypes/options, other JSON/content-boundary effects, explicit
classified degradation and target/account replay proofs remain
implementation work. Stream summary/content indexes and other native
MCP/approval/output fields also remain fidelity work.

Built-in protocols use effective credentials or explicitly registered auth
mechanisms. `AuthAppliers` may prepare a body, resolve the selected account and
perform at most one recovery after HTTP 401. `OAuthSession` owns admitted
refresh/commit work through an injected credential-store transaction. Invocation
reads no ambient credentials, discovers no accounts, initiates no login and never
switches accounts or providers. Catalog loading/refresh is a separate explicit
operation; the application supplies its offline baseline and persistence.

Anthropic Platform API authentication and Claude Code subscription request
shaping live in `providers::{anthropic,claude_code}`. Construct their appliers
with an injected credential backend or `OAuthSession`. Environment capture,
Claude CLI adoption, Keychain/file access and OAuth client registration are
application-side inputs. AI applies the resulting selected credential and
preserves the source prompt while projecting the provider request.

Native Gemini Generate Content and the private Antigravity subscription backend are
retired. Metered Google uses the official Chat endpoint. Individual tool signatures
map to canonical Google metadata; actual selected calls attach a replay proof bound
to the selected static key, endpoint, provider, model, account and exact call bytes.
Gateway clients must preserve both `extra_content.google.thought_signature` and
`extra_content.bitrouter.google_replay_proof`. The proof is removed before upstream
dispatch. Unrelated target/account replay is refused before authentication or I/O.
Available provider usage remains attached to late continuity validation failures.
See [retirement progress](../../docs/GEMINI_PROTOCOL_RETIREMENT_PROGRESS.md) for the
actual Antigravity SDK limitation and pending live-provider gates.

Custom adapter/transports use an injected `OutboundDispatch`. Token cancellation
waits for custom authentication to finish, then prevents dispatch; dropping the
call future can still drop authentication. A transport that rotates credentials
must own that operation's completion/persistence; built-in OAuth sessions use the
shared transaction contract and report the backend's actual durability.

The optional `hosted` feature provides `providers::hosted` request auth,
origin-bound credentials, metadata discovery and the full token-envelope decoder.
Inject a `HostedSession` with a selected `CredentialKey`, a
`HostedCredentialStore` and HTTP client; its owned refresh/commit work preserves
issuer, namespace, subject, scope, client identity and refresh-token expiry.
`BitrouterAuthApplier` also receives application-owned onboarding text. Cloud
model calls, management and telemetry can share this session. File paths,
login/logout, environment policy and storage durability remain caller-owned.
The application file backend uses process-local leases and compare-and-commit;
its pending rotations do not survive process/runtime loss. Default AI builds
keep hosted chrono dependencies disabled.

`ModelError` carries model-domain diagnostics and native provider statuses.
The caller decides gateway status, retry/fallback and public-message policy.
`StreamError` carries that caller-selected public presentation into an SSE
encoder; encoding does not sanitize arbitrary diagnostics.

AI has no SDK, router, application configuration, server or agent-runtime
dependency. Consumers depend directly on `bitrouter-ai`; the SDK retains no
aliases for the moved types/codecs/framing. SDK routing targets and pipeline
envelopes remain under `bitrouter_sdk::model_call::types`.

Remaining application/provider glue retirement, Core integration and conversion-loss admission
are tracked in the
[refactor spec](../../docs/BITROUTER_AI_REFACTOR_SPEC.md). The current codecs
retain baseline conversion behavior, including unresolved losses documented in
the [audit](../../docs/MODEL_HISTORY_COMPATIBILITY_AUDIT.md). Their extraction
does not make unclassified cross-provider history safe.


Explicit login is separate from model invocation. The default build includes
`auth::device_code` authorization and deadline-bounded polling over explicit
inputs. Enable `pkce` for `auth::{pkce,auth_code,listener,login}` and provider
registration inputs in `providers::login`. `run_login` accepts a caller-owned
HTTP client, registration, interaction callbacks and total timeout. It returns
the token for explicit persistence. The caller supplies browser/terminal actions,
callback-page HTML, account selection and storage paths. Cancellation/timeout
releases the listener and pending async callbacks; caller-started blocking work
cannot be interrupted by dropping its async wrapper.

Enable `hosted-login` for hosted device authorization, polling and revocation
in `providers::hosted::flow`. `LoginParams` contains explicit AS/client/scope
inputs, with no CLI/default resolution. Complete issued token envelopes retain
scope, refresh expiry, namespace and subject. `hosted` alone keeps request-time
refresh without login's random-number dependency; default/hosted builds do not
pull in `rand` or `keyring`. Login builds add random generation, while PKCE also
adds Tokio loopback I/O. No feature adds SDK, application, Axum, ACP or MCP.

The former `bitrouter_providers::oauth::{device_code,pkce,auth_code,listener,login,registry}`
and `hosted::account::flow` owners are deleted. Import the AI modules directly;
there is no forwarding facade. `PkceProvider` carries registration constraints
without CLI labels; `LoginUx::show_authorize_url` receives `manual_only`, and
callback pages belong to its caller. The application still owns file stores,
credential import/adoption and Cloud settings during staged retirement.


Enable `file-store` for explicit-path ordinary credential files:
`auth::file::snapshot::CredentialStore` supplies reads and administrative
mutations; `auth::file::backend::FileCredentialStore` implements the injected
selected-account transaction contract. The feature has no production dependency
additions. Neither interface discovers a default directory or account; relative
paths bind against the caller's cwd, and existing filesystem aliases resolve to
the selected file. The application owns XDG/home/default filename policy.

Legacy flat credentials load into the existing default slot without a read-time
write. Mutations retain labeled format, unrelated slots, Unix owner-only file
permissions and atomic replacement. Snapshot Debug omits paths, identities and
credential values. Administrative mutation and refresh share file binding, so
logout through a file alias cannot leave the refresh backend on old credentials.
Composite backends can use `FileCredentialStore::path` for the same binding.

Locks and retained replacements coordinate this process only. Staged rotation
survives dropped leases/failed writes; an externally replaced or removed selected
credential rejects stale commit. These are not cross-process exclusion or crash
recovery guarantees. The previous providers `oauth` module is deleted; import the
AI owners directly. The hosted envelope file backend remains application glue
and keeps its separate injected contract during staged retirement.

### Application provider package retirement

The alpha workspace removes `bitrouter-providers` without forwarding exports.
Model/auth/catalog consumers use `bitrouter_ai` directly. Product configuration,
activation and permitted external CLI/Keychain/binary discovery live in
`bitrouter::providers::{apply,builtin,entry,registry,import,claude_code,antigravity}`.
Cloud account files, settings and shared session assembly live in
`bitrouter::cloud::account::{credentials,manager,settings}`; the transaction
backend is private. Product-only imports do not become AI dependencies.

Persisted credential schemas, default paths, CLI commands and environment policy
are preserved. Both file stores still coordinate only within one process;
relocation does not add crash recovery or cross-process exclusion. Local workspace
migration is separate from external consumer migration and publication.

Google reasoning uses the canonical declared `reasoning_effort` path (`minimal` through `high`) plus scoped `include_thoughts`. Raw `thinking_level`/`thinking_budget`, `none`/`xhigh`/`max`, seeds and penalties remain excluded until selected-model equivalence is demonstrated. Static-auth validation rejects duplicate bearer headers and conflicting Google key headers/query credentials.

Classifier calls use `classifier::{ClassifierRequest, ClassifierResult}` and
`ModelClient::classify` with an explicit Decisions or System One target.
`protocol::classifier::ClassifierCodec` provides pure native conversion without
stream methods. System One supports structured state/instructions and named
question maps; destination confidence and usage are projected under explicit
conversion admission. The canonical JSON encoding is an internal typed format;
HTTP bodies must be rendered through the selected native codec. See the
[classifier spec](../../docs/CLASSIFIER_API_SPEC.md) for supported directions,
partial usage, refusal handling and source migration.
