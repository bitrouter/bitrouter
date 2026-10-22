# BitRouter Registry v1

Status: **normative beta protocol, `1.0.0-beta.3`.**

Date: 2026-10-22. Wire version: **`"version": 1`**.

MUST and MUST NOT express mandatory requirements; SHOULD expresses a recommended
practice with a justified exception; MAY expresses an optional behavior. The
[conformance requirements](conformance.md) define observable outcomes for
implementations. The [worked examples](examples/README.md) are informative
synthetic examples. Publication of this contract does not assert implementation
availability in a particular release.

Beta revisions may change incompatibly; stable v1 has the compatibility
commitments in section 3. Beta 3 requires a complete OpenRouter identity audit
before publishing a sync. Historical evidence cannot replace current membership
validation.

## 1. Scope and decisions

The registry has two independently maintained model views:

| View | Meaning | Authority over membership |
| --- | --- | --- |
| `models` | The rolling, curated selection for automatic routing | Explicit curation policy and validated discovery evidence |
| `providers[].models` | A provider's inventory, including active models outside the curated selection | That provider's feeds and explicit maintainer overrides |

Removing an entry from `models` MUST NOT remove, suspend, rename, or change the
price of a provider model. Discovering a provider model MUST NOT automatically
promote it into `models`. A provider inventory does not need to resolve every
entry against the current curated selection.

The other decisions are:

1. Protocol identifiers describe wire protocols: `chat_completions`, `messages`,
   `responses`, `decisions`. Vendor names remain valid provider/model identity
   components, but `openai` and `anthropic` are not v1 protocol identifiers.
2. Model identity, model classification, operation, wire protocol, modalities,
   and billing are separate concepts. No one field implies the others.
3. Provider inventory membership does not require known pricing or a locally
   implemented adapter. Cataloguing, invoking, automatic selection, and pricing
   each have separate eligibility checks.
4. A feed is a bounded declarative import, with explicit mapping and credentials.
   A provider can use its own listing for membership and another feed for prices.
5. Readers accept additive data, preserve unfamiliar information when rewriting,
   and execute only semantics they understand.
6. Verified provider-to-model bindings use explicit `canonical_id` values from
   OpenRouter's model identity vocabulary, independently of curated membership.
   Registry input errors are recoverable errors, never reasons to crash a host.

This specification covers model/provider YAML sources, deterministic JSON
artifacts, feed reconciliation, and their consumer contract. It does not revise
agent/runtime records, inference request/response formats, account entitlements,
or the learned router's scoring algorithm. Native decision request/response
semantics are defined by the wire contracts referenced in section 5.

## 2. Files and versioned envelopes

The source paths stay in place; model/provider artifacts gain an explicit major
version directory:

| Path | v1 content |
| --- | --- |
| `registry/registry.yaml` | Required source root: `version: 1`, `spec_revision: 1.0.0-beta.3`, UTC `as_of`, and source-only `identity_source` (section 10.1) |
| `registry/models/<vendor>.yaml` | Sequence of curated model records |
| `registry/providers/<provider>.yaml` | One provider record, including source-only sync configuration |
| `registry/model-discovery.yaml` | Curation policy; never provider inventory policy |
| `registry/model-identities.yaml` | Source-only OpenRouter identity evidence and verified provider bindings; not a recommendation list |
| `dist/registry/v1/{models,providers}.json` | Separate public envelopes below |

The source root versions the whole model/provider source tree; individual YAML
files do not repeat it. Agent/runtime sources and artifacts keep their existing
contract and are not implicitly declared v1 by this manifest.

Each model/provider artifact has this envelope (example contains no records):

```json
{
  "version": 1,
  "spec_revision": "1.0.0-beta.3",
  "as_of": "2026-10-22T00:00:00Z",
  "snapshot": "2d999e69709897a44f21c39bf2ff70c054f763ca5aeca26972e10bded282e58d",
  "requires": [],
  "data": []
}
```

`version`, `spec_revision`, `as_of`, `snapshot`, and `data` are required. `requires`
defaults to an empty list. `data` is always an array, even when empty. The
artifact path determines its record type; it is not an inference `/v1/models`
response and need not imitate that API's `object` field.

For published artifacts, `snapshot` is a lowercase SHA-256 hex digest identifying
the resolved pair. First render both envelopes without their `snapshot` member,
using the builder's deterministic JSON serializer. Feed `models.json`, NUL, its
rendered UTF-8 bytes, NUL, then `providers.json`, NUL, its rendered bytes, NUL into
SHA-256. Insert the resulting digest in both final envelopes. The serializer
sorts object keys, uses two-space indentation, LF line endings and one terminal
LF; record arrays sort by identity while interface preference order is retained.
Both artifacts have the same snapshot even if fetched separately. Builds do not
introduce wall-clock timestamps. Identical inputs and compiler version MUST
produce byte-identical artifacts. This digest detects mixed generations; it is
not a signature or independent proof of publisher authenticity.

`as_of` is the source manifest's explicit publication evaluation time, copied
unchanged into both envelopes. Curation/expiry validation and reverse-index
generation use it, never the builder's wall clock. A sync/publication transaction
advances it explicitly and must not move it backward; it does not refresh any
provider's individual availability evidence. A later rebuild with unchanged
inputs remains identical even across an expiry boundary. Runtime expiry checks
use the reader's current UTC time, so an expired cached entry is still excluded.

Readers MUST validate both envelopes before publishing a new combined view.
Mismatched snapshots, unsupported versions, and incompatible beta revisions
leave the previous valid pair intact; they MUST NOT combine new providers with
old curated models. A fetcher SHOULD fetch both files at one immutable Git
revision, or retry a mismatched pair within its bounded retry budget. Packaged
fallback and disk caches obey the same rule.

JSON duplicate object keys are invalid. YAML must be representable as JSON:
string mapping keys, no custom tags or non-finite numbers, and no duplicate
mapping keys. YAML aliases may share values but have no identity semantics.
Optional fields are omitted for unknown information; `null` is invalid in
registry records unless a field explicitly permits it. Empty arrays mean an
explicitly empty set, not an unknown set. Strings are case-sensitive unless a
field says otherwise. Dates use `YYYY-MM-DD`; timestamps use UTC RFC 3339 with
`Z`. All expiry comparisons use UTC, independent of the reader's timezone.

## 3. Compatibility and extension rules

### 3.1 Version lifecycle

`version` is an integer compatibility major. Missing, non-integral, or unsupported
values reject the artifact; there is no v0/unversioned fallback in the v1 reader.
`spec_revision` identifies a revision of this contract, independently of the
catalog snapshot. Its major MUST match `version`.

During beta, readers MUST accept only explicitly supported prerelease revisions;
support for `1.0.0-beta.3` does not imply support for `beta.4` or stable `1.0.0`.
An incompatible beta edit increments `spec_revision`, even though `version`
remains 1. Production defaults MUST NOT follow an unpinned beta artifact stream.

After `1.0.0` freezes, stable `1.x.y` revisions are compatible within version 1:

- Optional fields, new named values, and new optional feature definitions may
  be added with safe old-reader behavior.
- Existing field types, units, identity rules, default values, and meanings
  MUST NOT change. New fields MUST NOT become universally required in v1.
- A deprecation is documentation, not permission to remove a stable v1 field.
- Incompatible required core semantics require version 2. A new optional
  interface can instead be isolated with the feature mechanism below.

Old snapshots becoming editorially obsolete is a data lifecycle change, not a
schema break. Reusing an old identifier for a different model is a break.

### 3.2 Open data, explicit execution

All objects are open to additional members. All named vocabularies in this
specification are open string sets, including protocol, operation, model type,
protocol profile, question type, modality, capability, lifecycle status, auth
kind, billing kind, and feed kind.
Unknown is different from malformed: a string with an unfamiliar value is
unknown; an object where a string is required is malformed.

| Unfamiliar value or field | Required reader behavior |
| --- | --- |
| Optional metadata field | Accept; ignore for decisions it does not understand |
| Model type | Preserve; do not assume it is a language model; type-based selection cannot match it |
| Protocol, operation, or protocol profile | Preserve the interface; exclude it from invocation unless a registered adapter explicitly supports that triple |
| Decision question type | Preserve; do not send a request requiring it through a codec/converter that lacks its semantics |
| Additional input/output modality | Preserve; a request needing that modality cannot use an adapter that lacks it |
| Capability | Preserve; never count it as proof of another capability |
| Provider/model status | Preserve; only the exact known `active` status is eligible |
| Auth/access/required configuration kind | Disable that provider's invocation until understood; no auth fallback |
| Price kind, meter, or applicability condition | Price unavailable for the affected interface; never zero or a cheaper fallback |
| Feed/auth/pagination/transform kind | Do not run that feed; do not reconcile removals from it |

`requires: [<feature-id>, ...]` is available on envelopes, providers, model
records, interfaces, prices, and feeds. These lists are unordered and unique;
they declare semantics the reader MUST implement before using the containing
object. Requirements are inherited by children. An unsupported envelope
requirement rejects that artifact; an unsupported provider requirement disables
that provider; an unsupported model/interface requirement disables only that
model/interface. An unsupported price requirement blocks pricing, not catalog
retention. An unsupported feed requirement blocks that feed, not unrelated feeds.

Feature IDs are stable strings, such as `pricing.metered.v1`. A required feature
is a declaration of built-in reader support, never a URL to fetch code from.
New fields that constrain credentials, request interpretation, route eligibility,
or billing MUST either have harmless behavior when ignored or carry a new
requirement at the smallest affected scope. Writers MUST NOT hide mandatory
restrictions in an unknown optional field. Unrecognized executable discriminants
already fail closed by the table; `requires` also protects changes to otherwise
recognized objects.

Parsers, sync writers, builders, and caches that reserialize data MUST preserve
unknown fields and raw unknown values at every nesting level, including opaque
interfaces and prices. A read-only consumer may project only known fields into
its execution view, but a cache MUST retain the complete document. Source lint
SHOULD warn about unfamiliar spellings to catch typos without making the reader
vocabulary a closed enum. Private descriptive extensions SHOULD live under
`extensions`, keyed by a reverse-domain namespace.

The identity ledger is source-only. The documented source-only containers
(`auto_sync`, `sync`, `defaults`) are excluded from public artifacts as a whole.
Their unknown members remain intact
in source rewrites; this exception does not permit dropping unfamiliar members
of published records or cached documents.

A malformed known field is a validation error, not a compatibility extension.
The publisher MUST reject an invalid build. A runtime MUST reject an invalid
new pair and retain the last valid pair. Merely unsupported records remain in
the parsed pair with diagnostics and disabled execution; they are not invalid.

### 3.3 Recoverable errors and upgrade guidance

Every failure caused by registry input or forward incompatibility MUST be
returned as an ordinary structured error. Parsing, validation, mapping, identity
lookup, compatibility negotiation, and price resolution MUST NOT panic, abort,
terminate the daemon, or use an unchecked assumption about catalog contents.
Unknown enum strings and malformed/oversized/deeply nested input are included;
parsers must enforce bounded sizes/depth before resource exhaustion. This is not
a claim to recover from an unrelated internal program defect or a killed process.

Diagnostics contain a stable code, affected artifact/provider/model/interface,
field path where applicable, observed version/profile/feature, supported values,
and a recovery hint. Codes include `registry.unsupported_version`,
`registry.unsupported_revision`, `registry.unsupported_interface`,
`registry.unsupported_feature`, `registry.invalid_data`, and
`registry.pricing_unavailable`. Compatibility errors MUST recommend upgrading
BitRouter or using a registry revision supported by the installed version. Do
not claim that an available upgrade implements a feature without evidence.
Malformed source or missing credentials also identify the actual corrective
action; repeatedly refreshing the same incompatible data is not recovery.

A refresh failure retains the last complete valid pair without refreshing its
evidence timestamp. With no previous pair, try a compatible bundled pair; if
none is usable, the registry view remains empty. The host remains alive with
explicitly configured usable routes and its diagnostic/control surfaces.
Requests requiring unavailable registry routes receive a normal error response.
An explicit request for an unsupported interface returns an error with upgrade
guidance; it does not silently reroute to another operation. Known independent
routes continue to work. Harmless unknown optional metadata needs no warning.

One-shot validation/sync commands may return a normal nonzero exit status with
diagnostics; this is not a crash. The long-running host must contain the error
at the refresh/request boundary. Do not use a blanket panic catcher to disguise
internal bugs as compatibility errors. Conformance tests must assert process
survival and continued service, not just that deserialization returns `Err`.

## 4. Identity and the two catalogs

### 4.1 Curated model record

| Field | Contract |
| --- | --- |
| `id` | Required canonical ID from the independent identity ledger below; unique across `models` |
| `name`, `description`, `family` | Optional display/grouping strings; not identity or ordering rules |
| `model_types` | Optional unique string array; e.g. `language`, `decision`, `image_generation`, `audio_generation`, `video_generation`, `embedding`, `reranking` |
| `input_modalities`, `output_modalities` | Optional unique arrays of known model-level facts; not endpoint support guarantees |
| `max_input_tokens`, `max_output_tokens`, `context_window` | Optional positive integers; combined context is not inferred from either separate limit |
| `release_date`, `knowledge_cutoff`, `open_weights` | Optional verified facts; `open_weights` is a boolean |
| `benchmarks` | Open object keyed by benchmark/version; results retain harness, configuration, measurer, date, and source provenance |
| `curation` | Required object described below |
| `providers` | Generated reverse index only; absent from sources |

For a model with an established OpenRouter correspondence, the canonical ID is
the exact `data[].id` of its underlying model in OpenRouter's full model list.
The [OpenRouter response](https://openrouter.ai/docs/api/api-reference/models/list-all-models-and-their-properties)
also exposes `canonical_slug`; that is recorded as optional source evidence,
not silently substituted for the chosen public `id` vocabulary. This is a
BitRouter normalization decision, not a claim that OpenRouter IDs are immutable.

IDs are preserved exactly, with a nonempty namespace and model portion separated
by `/`, no whitespace/control characters, and no reserved `~native/` namespace.
Do not lowercase, strip suffixes, or manufacture IDs by concatenation alone.
The namespace determines the source model filename stem. Provider-routing
variants such as `:free` or `:nitro`, and moving `latest` aliases, cannot establish
model equivalence merely by stripping their suffix; their underlying model
requires a verified explicit binding. Incompatible model revisions stay distinct.

`registry/model-identities.yaml` is a sequence of records with required
`canonical_id`, `source_url` (the OpenRouter catalog), and `observed_at` (UTC
timestamp), optional `canonical_slug`, and optional `bindings`. Each binding is
`{provider, provider_model_id, source_url, verified_at}`: an exact native offer
and evidence that it is the same model. Canonical IDs and bound native pairs
are unique in this ledger. Runtime providers need no live OpenRouter lookup to run.
Identity discovery uses the full OpenRouter catalog independently of curated
family/retention filters. Failed or partial discovery preserves previous evidence.
New verified identities and bindings are staged with affected provider records;
publication validates that complete source transaction before replacing either
artifact. An identity discovery failure does not prevent staging an unbound
native offer from a successful provider feed, but the global identity audit in
section 10.1 must pass before any sync candidate is published.

This ledger includes identities outside `models` and retains historical verified
bindings when OpenRouter delists a model or discovery fails. Its sync does not
delete provider inventory. A changed upstream ID or conflicting binding requires
an explicit reviewed migration; never silently repoint an old ID to a different
model. A reviewed migration moves the current binding atomically and retains
the previous mapping in Git history; it does not create two current ledger
bindings for one native offer. Historical evidence alone does not validate a
current published ID.
Every canonical ID referenced by either candidate artifact must exist in the
full OpenRouter response fetched for that publication run. A delisted ID blocks
publication pending reviewed resolution; it does not delete provider inventory
or invalidate a previously admitted runtime snapshot retroactively.
The ledger is source-only; provider artifacts materialize the bindings needed
for routing, so consumers do not need a third network artifact.

If no reliable OpenRouter correspondence exists, omit `canonical_id` from the
offer and keep its native identity. This revision does not invent a substitute
OpenRouter ID. Such offers remain usable through explicit provider/native routes
and user-defined candidate sets. They do not enter the default curated vocabulary
until a canonical identity is established.

`curation` contains required `reason` (nonempty string), `source_url` (HTTPS
evidence), and `checked_at` (UTC timestamp), and optional `expires_at` (UTC
timestamp). Absence of `expires_at` means no scheduled editorial removal, not
permanent upstream availability. Only verified, currently useful models enter
this list. Inclusion is a routing recommendation, not proof that any particular
account can invoke the model.

A multimodal model expresses multiple modalities. It need not acquire a new
exclusive `type: multimodal`. A model may have both `language` and `decision`
types; those labels alone grant neither operation. A decision result's modality
may be `structured`, rather than pretending it is generated prose.

### 4.2 Provider and provider model records

The provider has required `name`, `status`, and `models`. `name` equals the
filename stem, matches `[a-z0-9][a-z0-9_-]*`, and is unique. The generated `id`
duplicate is removed; `name` is the only provider identity field.

The following existing descriptive/provider fields keep their meanings:
`display_name`, `metadata`, `contact`, `submitted_at`, `community` (default
false), `kind`, `weight` (positive finite number, default 1), `doc_url`,
`api_base`, `access` (default `api_key`), and `required_config` (default `[]`).
Metadata is descriptive, never an auth or routing authority. An unknown provider
`kind` gets no first-party preference; it need not prevent explicit invocation.

`status` is required: `active`, `staging`, `suspended`, `withdrawn` are known.
Only `active` permits invocation. An active provider MAY temporarily have an
empty inventory; it then offers zero static routes. Sync never changes provider
status merely because the curated list or a fetched inventory becomes empty.

`auth` is explicit when invocation requires credentials. Known shapes are
`{kind: bearer, env: ...}`, `{kind: header, env: ..., header: ...}`, and
`{kind: oauth|native, handler: ..., params: {...}}`. Optional `extra_headers`
contain public constants. `kind: none` explicitly declares unauthenticated
invocation. An absent auth declaration is unknown and cannot enable an automatic
provider. Credential discovery from `access` or a provider name is not permitted.
`auth_scheme` and generated `byok` are removed; consumers derive presentation
from the explicit auth/access declaration. Feed auth is separate (section 8).

Each provider model contains:

| Field | Contract |
| --- | --- |
| `provider_model_id` | Required nonempty upstream string, preserved exactly, including case and slashes |
| `canonical_id` | Required whenever a verified current OpenRouter correspondence exists; otherwise absent; independent of `models` membership |
| `status` | Required; same known vocabulary as provider status; active means advertised as callable, not necessarily recommended |
| `name`, `description`, `model_types` | Optional provider-local metadata |
| `interfaces` | Required ordered array; may be empty when endpoint support is unknown |
| `available_until` | Optional UTC timestamp: upstream retirement, not editorial expiry |
| `upstream_lifecycle` | Optional raw lifecycle label, e.g. an upstream's still-callable `deprecated` |
| `availability` | Optional provenance: `source_url`, `checked_at`, and `scope` (`public` or `account`) |
| `sync` | Source-only ownership/observation data, defined in section 9 |

The storage identity is **(provider name, provider_model_id)**. `canonical_id`
is not the inventory primary key. Multiple upstream IDs may map to one canonical model;
each remains a separate offer and selection must be deterministic. A native ID
with no verified canonical identity is retained with `canonical_id` absent.
It can be explicitly addressed through its provider and native ID, but it is not a
cross-provider alias and cannot enter curated automatic routing.

Identity assignment precedence is an explicit maintainer binding, an explicit
feed mapping to `canonical_id`, then no binding. A reviewed mapping may attach
a fixed verified vendor prefix to a native ID, using the `prefix` transform in
section 8; this makes first-party discovery work without joining the rolling
curated list. The resulting candidate must match an independently observed
OpenRouter identity and have verified same-model evidence before assignment;
a matching string alone is insufficient. Record the binding in the ledger.
If correspondence remains uncertain, retain the native offer unbound and report
it; do not drop the offer or present a guessed canonical ID. Preserve any existing
verified binding when a new candidate is unresolved. Feed imports MUST NOT infer
identity from suffix matching, lowercasing native IDs, vendor ownership strings,
or the rolling `models` list. A conflicting binding is reported for review;
sync MUST NOT silently rename an existing offer. Maintainer bindings live on
the ledger and survive curation changes. A known binding with an ID present in
the current identity audit is materialized by sync/migration and rejected if
still missing or different at publication validation. A historical binding whose
ID is absent is reported, never silently cleared or reassigned to pass the audit.
Provider-model `id` is removed in this beta;
curated `models[].id` equals the corresponding `canonical_id`.

Generated `models[].providers` contains only references
`{provider, provider_model_id}` to statically active, unexpired offers with the
same canonical ID. It contains no duplicated prices, capabilities, or auth.
Consumers join against the providers artifact from the same snapshot and apply
their own supported-interface/account gates. This index is a convenience, not
an entitlement or current-health assertion.

### 4.3 Automatic and explicit routing

Default automatic model selection uses the intersection of:

1. An unexpired curated record.
2. An active provider and active, unexpired provider model with that canonical ID.
3. A supported interface matching the requested operation, protocol conversion
   constraints, modalities, and required capabilities.
4. Configured credentials, account entitlement, runtime health/admission, and
   any caller requirements such as known cost.

Unknown metadata MUST NOT be treated as a positive capability or infinite limit.
Type-based selection can use understood type labels; a known interface remains
usable even if the model has an additional unfamiliar classification label.
No eligible intersection means no automatic candidate, not a fallback into the
full provider inventory. Explicit model selection, by canonical ID or
provider/native ID, does not require curation, but still observes status, expiry,
operation, auth, and execution constraints. Resolving an explicitly requested
canonical ID uses verified bindings across the full provider inventory, not
membership in the curated vocabulary. Choosing a provider for an already
specified model is different from automatically choosing which model to use.
Existing account/subscription opt-in restrictions apply to both paths.

A user-defined router may explicitly enumerate canonical or provider/native
targets outside `models`, including a native decision model not listed by
OpenRouter. That configured candidate set replaces the default curation gate;
it does not relax interface, auth, status, or billing requirements. A missing
benchmark or future capability-quality score does not prohibit manual routes
and is not treated as zero. A scoring policy that requires unavailable scores
must report missing evidence or exclude that candidate explicitly. Verified
execution capabilities belong to the provider interface and remain required;
quality/recommendation metadata is a separate layer.

The public inference `/v1/models` projection continues to describe the configured
providers' callable inventory, not just the curated list. Consumers MUST NOT
deduplicate unrelated unbound native IDs across providers. Unbound IDs require
an unambiguous provider-qualified API identity; listing and resolving that
identity must round-trip without changing the native ID.

The reserved explicit selector is `<provider>:~native/<encoded-native-id>`.
Encode the native ID's UTF-8 bytes: keep ASCII letters, digits, `.`, `_`, and
`-`; encode every other byte as `%HH` with uppercase hex. Decode exactly once,
reject malformed escapes/UTF-8, and perform exact native-ID lookup within that
provider. The `~native/` prefix cannot collide with the canonical ID grammar.
This selector works for bound and unbound offers; listing uses it when there is
no canonical binding. Existing canonical selectors keep their meaning. Native
IDs must not be empty, whitespace-only, or contain control characters.

## 5. Interfaces, modalities, and operation-specific data

An offer's `interfaces` array is ordered by preference, most preferred first.
Each entry describes one **(protocol, operation, effective protocol_profile)**
triple; duplicates are invalid. An absent profile means `default` only for a
protocol whose default wire contract is defined. Profile names below are
BitRouter identifiers for documented wire contracts, not upstream version claims.
For unfamiliar protocols, preserve an absent profile as absent without inferring
a codec. For a known default, omission and explicit `default` identify the same
interface and therefore cannot appear as separate entries.

| Field | Contract |
| --- | --- |
| `protocol` | Required open string; names the wire format, not the vendor |
| `operation` | Required open string; semantic operation, never inferred for unknown protocols |
| `protocol_profile` | Optional open string selecting a precise wire dialect; required for `systemone`; unknown profile disables only that interface |
| `question_types` | Optional unique open string array of verified decision question types in this interface's own profile; absence means unknown |
| `base_url` | Optional HTTPS API base; overrides provider `api_base` for this interface |
| `input_modalities`, `output_modalities` | Optional unique string arrays; absence means unknown |
| `capabilities` | Optional unique verified feature names; absence/empty grants none |
| `limits` | Optional open object; known positive integer token limits: `max_input_tokens`, `max_output_tokens`, `context_window` |
| `rate_limits` | Optional positive integer `requests_per_minute`, `tokens_per_minute` |
| `compatibility` | Optional protocol-specific object interpreted only by that protocol's adapter |
| `reasoning_effort` | Optional existing levels/default configuration; requires verified `reasoning` capability |
| `pricing` | Optional interface-specific price (section 6) |
| `requires` | Optional mandatory feature IDs |

The following wire contracts have profile `default`:

| Protocol | Operation | Relative endpoint |
| --- | --- | --- |
| `chat_completions` | `generation` | `/chat/completions` |
| `messages` | `generation` | `/messages` |
| `responses` | `generation` | `/responses` |
| `decisions` | `decisions` | `/decisions` |

### 5.1 Models with both generation and decision interfaces

A single provider model can declare `model_types: [language, decision]` and
both `responses/generation` and `decisions/decisions` interfaces, with the same
`provider_model_id` and `canonical_id`. Do not duplicate its identity, require
exclusive model types, or infer Decisions support from ordinary structured
generation. OpenAI's [Decisions guide](https://developers.openai.com/api/docs/guides/decisions)
uses `gpt-6-luna` for native decisions; its ordinary LLM interfaces retain their
own capabilities and prices. Capability is verified per provider, not inherited
automatically by every provider serving that model.

### 5.2 Different decision wire formats

`operation: decisions` is the routing category. `protocol` and
`protocol_profile` select the actual request, response, errors, and usage codec:

| Protocol/profile | Operation | Wire contract |
| --- | --- | --- |
| `decisions` / `default` | `decisions` | OpenAI Decisions contract |
| `systemone` / `vllm_systemone_v1` | `decisions` | vLLM Semantic Router System One wire |
| `systemone` / `noulxp_http_v1` | `decisions` | NoulXP HTTP binding as documented by its serving API |

The [vLLM System One endpoint](https://decision.vllm-sr.ai/docs) uses `state`
and question/answer objects keyed by IDs; it documents `/v1/decisions` as an
alias for that wire. An endpoint path ending in `decisions` therefore does not
prove the OpenAI codec is appropriate. The
[NoulXP serving API](https://systemonemodels.ai/docs/inference) documents its
own question representation. Keep the profiles distinct unless an adapter has
verified the relevant common subset. A similar question-type name does not
establish interchangeable semantics.

A shared operation does not authorize arbitrary format conversion. Selection
must verify an adapter/converter can preserve the specific question types,
identifiers/order, typed choices, probabilities/confidence, refusal/partial-error
semantics, modalities, and usage basis required by the request. Unsupported
profiles or lossy conversions return a recoverable compatibility error. No
fallback to text generation, guessed schema, dropped question, or invented
probability is permitted. An implementation may retain either format without
invoking it; invocation requires explicit support for its wire contract.

### 5.3 Interface defaults and expansion

The base URL includes the API prefix (typically `/v1`); it excludes the operation
suffix. The registered adapter owns suffix and transport behavior. An interface
without a concrete base may use a declared `required_config: [base_url]` or an
explicit native handler; no URL is guessed from the protocol name.

The source may declare `defaults.interfaces` once per provider. A model omitting
`interfaces` in source inherits that whole ordered array; an explicit array
replaces it in full, including `[]`. The build materializes the result on every
offer and removes `defaults`. There are no implicit per-field merges or glob
rules. Feed defaults apply to imported new records, not a blanket claim about
every model a provider might expose. Heterogeneous feeds must map verified
interfaces or retain unknown ones as `[]`.

Model-wide protocol globs, `api_protocol`, `protocol_endpoints`, and
`pricing_by_protocol` are not v1 interface declarations. Support and tariffs
belong to the specific interface; do not combine image output on one interface
with tools on another, or apply generation tariffs to Decisions. Compatibility
fields such as `google_extensions` remain protocol details rather than new
manufacturer-named protocols.

Future `image_generation`, `audio_transcription`, `video_generation`, embedding,
or other operations can add their own protocol/operation pairs, limits, and
pricing. These example names do not establish execution support.
Old clients retain the records and skip unsupported interfaces. An unfamiliar
registry interface MUST NOT be treated as generation by default. A registered
adapter must declare its supported operation and fidelity explicitly.

## 6. Billing and pricing

Provider `billing` is an open string. Known values are `usage` and `subscription`;
it is required in v1. `usage_token` migrates to `usage` because not all future
models are priced in tokens. Unknown billing disables cost-aware selection and
settlement until supported; it does not erase the inventory.

Each interface owns its entire tariff; there is no cross-protocol fallback.
The full tariff lookup includes provider, native model, operation, protocol,
effective protocol profile, and applicable deployment/processing terms. A shared
canonical ID or the same `decisions` operation is not permission to reuse a
different provider's, dialect's, or generation interface's price.
Omitted pricing means **unknown**, not free. A provider model with unknown
pricing can remain active and be explicitly invoked where application policy
does not require a price. A selection or settlement path that requires prices
MUST reject unknown/inapplicable pricing. Subscription is not a synthetic
zero-dollar token tariff; subscription entries omit usage prices in this revision.

Known `pricing.kind: token` retains the existing numerical USD-per-million-token
fields: `input_tokens.{no_cache,cache_read,cache_write}`,
`output_tokens.{text,reasoning}`, `context_tiers`, and `endpoint_profile`.
`currency: USD` is required. Rates are finite nonnegative JSON numbers; omission
is unknown and explicit zero is a known zero rate. `context_tiers` have unique,
ascending `above_input_tokens` positive integers; the greatest threshold strictly
below the request input count applies to the entire request. A tier overrides
specified rates and inherits unspecified rates from the base, never another
interface. An unknown endpoint profile or unverified effective deployment makes
the tariff inapplicable. Existing metering evidence requirements still apply;
a tariff does not establish cache-counter semantics.

For decision-count, request-count, and other non-token billing, v1 defines an
explicitly gated shape:

```json
{
  "kind": "metered",
  "requires": ["pricing.metered.v1"],
  "currency": "USD",
  "meters": [
    {"id": "image.standard", "unit": "image", "quantity": 1, "amount": "0.04"}
  ]
}
```

Meter IDs are unique within a tariff. `quantity` is a positive integer and
`amount` is a nonnegative decimal string without exponent notation. A runtime
must explicitly implement the meter's usage counter and applicability; a label
such as `second`, `image`, or `request` is not enough. Unknown meters make the
whole affected tariff unavailable, rather than dropping charges. Additional
conditioned/tiered meter semantics require their own feature IDs. An
implementation may retain these tariffs without a billing engine, but MUST
report pricing as unavailable until it supports all applicable meters.

### 6.1 Decision billing and usage semantics

Do not assume every decision model uses generation token rates or charges once
per HTTP request. As checked on 2026-10-22, the
[OpenAI Decisions guide](https://developers.openai.com/api/docs/guides/decisions#pricing-and-availability)
describes `gpt-6-luna` Decisions as input-only billing, with region and long-context
terms; its other interfaces have separate tariffs. This fact belongs to the
offer's sourced tariff, not a global special case for all decision models.

Metered tariffs may use `unit: decision` or `unit: request`. Each meter's
registered `id` defines which observable counter is billable. For example, a
synthetic `answered_question` meter counts billable answered questions, not one
request or every supplied question. Unknown refusal, partial-error, retry, or
batched-state counting rules make that meter unavailable until implemented and
verified. The [System One serving API](https://systemonemodels.ai/docs/inference#plans-and-limits)
illustrates why question counts and request counts must remain distinct.

Any per-question/per-input token repetition must come from the selected codec's
documented usage contract. Do not multiply a shared input count by the number
of questions, or subtract a reported cached subset, merely because another
decision format does so. The normalized usage must retain the native counters
and provenance needed to verify applicability. Missing counters are unknown,
not zero; declared zero charges are different from an unimplemented meter.
No supported billable path may silently drop a meter or borrow another tariff.

Subscription allowances and rate-limit quotas are not monetary per-decision
prices. A subscription with usage overages requires an explicit supported billing
extension; it cannot be flattened into a free token tariff. Unknown billing
semantics yield `registry.pricing_unavailable` and upgrade/configuration guidance
where prices are required, while retaining the offer for inspection.

## 7. Curated discovery and rolling validity

The existing family selection policy remains explicit: approved family patterns,
numeric version ordering, `retain_versions` (default 2), variant exclusions, and
a configurable grace window (default 30 days). Discovery is evidence for
curation; the provider listing and provider sync are independent jobs.

Within an approved family, only validated, eligible, non-retired releases count
toward the retention window. A newer unusable/withdrawn release MUST NOT push out
a usable older release. Numeric components compare numerically with trailing
zero components equivalent; timestamps and ID lexical order do not choose the
latest version. All eligible variants of retained versions may be included.
Excluded variants do not by themselves establish an eligible version.

| Event | Curated effect | Provider effect |
| --- | --- | --- |
| Valid new release in an approved family | Add after identity/metadata/availability validation | None; provider feed decides its offers |
| Healthy release leaves retention window | Set `curation.expires_at` once, to now plus grace | None |
| Expiry reached | Remove from curated artifact; reader also excludes an expired cached record | None |
| Release re-enters window before expiry | Clear scheduled editorial expiry | None |
| Confirmed globally unsuitable/broken release | Remove immediately and retain curation tombstone with reason/evidence | None; explicit invocation remains a separate policy decision |
| Provider withdraws an offer | Recompute availability; do not recommend a model with no eligible offers | Update only that provider offer |
| Discovery failure or an unverified/partial response | Preserve existing membership; elapsed explicit expiries still apply | None |

`retired_models` becomes an explicit curation tombstone list with `id`, `reason`,
`source_url`, and `recorded_at`. Tombstones prevent rediscovery until deliberately
removed. They are not a global provider denylist. A global execution ban, if
needed, belongs to a separately explicit routing policy and MUST NOT be inferred
from editorial membership. The old ambiguous `deprecation_date` is removed:
editorial expiry is `curation.expires_at`; upstream end-of-service is
`available_until`. Do not mechanically translate one into the other.

For this specification, “valid” means identity and required routing facts have
verified provenance, the model is not tombstoned/expired, and at least one
active compatible offer exists in the published snapshot. It is not a promise
of live uptime, universal account access, or measured model quality beyond the
cited curation evidence. Runtime health and admission remain additional gates.
The build fails if a newly curated entry has no active offer with a declared
interface; when a provider sync removes the last offer, the same publication
removes that now-unavailable curated record. Restoration requires fresh evidence
and curation policy, never arbitrary provider promotion.

## 8. Provider sync feeds

### 8.1 Feed declaration and authority

`auto_sync` is source-only and contains `feeds`, an array of feed objects. Each
feed has a unique stable `id`, a `kind`, and an explicit `writes` array selected
from `models`, `metadata`, and `pricing`. Unknown write domains disable the feed.
One provider may have a membership feed and a separate pricing feed; two feeds
MUST NOT own the same domain on the same offer. `writes: [pricing]` cannot add,
remove, rename, or reactivate inventory entries.

Known kinds are `v1_models`, `models_dev`, and `agentic`. The latter two retain
dedicated adapters; they must emit the same normalized observations and obey
the same ownership rules. `models_dev` is enrichment by default, not evidence
that a first-party endpoint still serves a model. `agentic` produces a reviewed
proposal, not an authoritative complete snapshot for automatic removal.

`v1_models` requires the `request`, `response`, and `pagination` blocks below.
`models_dev` requires `request` with the same HTTP/auth rules and a nonempty
`key` naming the exact provider object in its catalog; its dedicated adapter
defines normalization, so generic `response`/`pagination` blocks are invalid.
It may write inventory only in documented provisional `merge` mode. `agentic`
requires a nonempty `urls` array of HTTPS evidence pages and accepts no
credential-bearing `request` block. Its generated proposal must pass the same
domain/ownership validation before being applied; it cannot assert a complete
snapshot or alter sync configuration. No feed kind guesses its source URL.

A provider's own documented feed is preferred for membership. A provider with
no usable machine feed may remain manual or use a documented, reviewed proxy
source. Do not describe third-party metadata as authoritative provider lifecycle.

Example of the source shape (fictional API, not a real credential request):

```yaml
auto_sync:
  feeds:
    - id: catalog
      kind: v1_models
      writes: [models, metadata]
      request:
        url: https://api.example.com/v1/models
        headers:
          example-api-version: '2026-01-01'
        auth:
          kind: bearer
          env: EXAMPLE_REGISTRY_API_KEY
      response:
        path_syntax: dotted
        items_path: payload.models
        mapping:
          model.id: provider_model_id
          model.canonical_id: canonical_id
          model.display_name: name
          lifecycle: upstream_lifecycle
          features.interfaces: interfaces
        defaults:
          status: active
          interfaces: []
        filter:
          path: lifecycle
          in: [active, deprecated]
          known_values: [active, deprecated, retired]
      pagination:
        kind: cursor
        cursor_path: payload.next_cursor
        has_more_path: payload.has_more
        parameter: after
        max_pages: 100
      inventory:
        mode: snapshot
        scope: public
        complete: true
        allow_empty: false
        missing_runs: 2
        minimum_missing_hours: 24
```

The endpoint must actually document `deprecated` as still callable for that
filter to be correct. Unsupported future lifecycle values are not silently
mapped to `active`; section 9 defines how incomplete interpretation affects
removal. `v1_models` names the JSON-list importer, not a promise that all model
listing APIs have the same response shape.

### 8.2 Request and credentials

`request.url` is the exact absolute HTTPS listing URL, including `/models` or
another documented path. No suffix is appended. It has no userinfo, fragment,
or secret-bearing query parameter. Optional `request.query` is a string-to-string
map of public parameters; it must not collide with URL query names or the
pagination parameter. Method is GET in this revision. `request.headers` carries
public constant headers only.

Feed `request.auth` is required: `none`, `bearer` with `env`, or `header` with
`env` and `header`. `env` matches `[A-Z_][A-Z0-9_]*`. Header names are
case-insensitive; duplicate names or collisions between constant headers and
the auth header are errors. Header names/values must be valid HTTP headers;
CR/LF are rejected. `bearer` produces `Authorization: Bearer <value>`; `header`
places the value exactly in the configured header. OAuth/SDK/signing support can
be a future feed-auth kind and is not emulated as an unauthenticated request.

Secrets are read only from the named process environment variable at execution.
Missing or empty values produce `missing_secret`, no request, and no catalog
mutation. There is no fallback to provider runtime auth, local OAuth caches,
inferred `{PROVIDER}_API_KEY`, another feed's key, or an anonymous retry. This
permits a public feed for an authenticated inference provider and a separate
least-privilege CI credential for a private listing.

Secrets MUST NOT appear in source, dist, cache, command arguments, reports,
request/body debug logs, or PR text. Responses are untrusted data, never
instructions or code. Authenticated requests do not follow redirects in this
revision; a changed endpoint requires updating the configured URL. Requests
use bounded timeouts, response sizes, and retries. Retry transient timeouts,
429, and 5xx with backoff; do not retry authentication failure anonymously.

### 8.3 Mapping and normalization

The mapping direction is **source path -> normalized provider-model path**:
`"a.b.c": "x.y.z"` reads `a.b.c` from one upstream item and writes `x.y.z`
on that item's normalized record. A value may also be a nonempty array of
destination paths, copying the same source value to each target before their
individual transforms. This permits `id: [provider_model_id, canonical_id]` without
duplicate YAML keys. There is no implicit passthrough of the raw item and no
default canonical-ID inference.

`response.path_syntax` defaults to `dotted`. Dotted paths use `.` between object
keys, `\.` for a literal dot, and `\\` for a literal backslash; other escapes,
empty segments, and trailing escapes are invalid. On an array, a segment is a
zero-based decimal index without leading zeros. On an object the same segment
is a literal key. Empty string denotes the root, allowed for `items_path` and
read paths, not mapping destinations. No wildcards, recursive descent, filters,
expressions, function calls, or evaluation are supported.

`path_syntax: json_pointer` is also supported for keys that dotted paths cannot
represent, following [RFC 6901](https://www.rfc-editor.org/rfc/rfc6901).
The selected syntax applies to all response, transform, and pagination paths
in that feed; mixing syntaxes is an error. JSON Pointer destinations cannot use
the root or the append token `-`.

`items_path` is required and is evaluated against each response document. It
must resolve to an array; absent, null, scalar, or object is a feed failure,
never an empty inventory. Each item must be an object. `mapping` is required;
the minimal conventional list mapping is `{id: provider_model_id}` with
`items_path: data`. `provider_model_id` must resolve to a nonempty string.

Destination paths may target `provider_model_id`, `canonical_id`, `name`, `description`,
`status`, `model_types`, `upstream_lifecycle`, `available_until`, and known
members of `interfaces`. They MUST NOT target provider configuration, auth,
feed configuration, curation, `requires`, ownership, or arbitrary filesystem
paths. Future mapping destinations require importer support before use.
`availability` is generated from the feed's request/scope and successful run.

Mapping is simultaneous, not order-dependent. Duplicate destinations and
ancestor/descendant collisions (such as `interfaces` and
`interfaces.0.protocol`) are configuration errors. Missing intermediate object
members may be created. Destination arrays must already exist in `defaults`
with an in-range fixed index; no sparse creation or append is allowed. Unsafe
object keys such as `__proto__`, `prototype`, and `constructor` are rejected by
the importer. Defaults use normalized nested objects, not path-expression keys.

`defaults` provides reviewed constants for new entries and destination array
shape. A present mapped value overrides the default. A missing source path
produces **no observation**: existing data remains unchanged; a new record may
use its default. Null optional metadata is an explicit clear when the feed owns
that field; it is emitted as omission in the registry. Null identity, status,
interfaces, or other required data fails the feed. Neither null nor a type
error activates a default. No coercion occurs without a declared transform.

Optional `transforms` is a destination-path-to-ordered-steps map. v1 knows only:

- `{kind: prefix, value: <string>}`: prepend a fixed public string to a string
  value, e.g. `openai/` on the `canonical_id` target only; no interpolation,
  lowercasing, alias resolution, or modification of the native ID occurs.
- `{kind: parse_decimal}`: a finite base-10 decimal string to a JSON number;
  reject empty, exponent, nonnumeric, overflow, and non-finite input.
- `{kind: multiply, factor: <finite-number>}`: multiply a numeric value, checking
  range; used for explicit unit conversion, such as per-token to per-million.
- `{kind: value_map, values: [{from: <JSON>, to: <JSON>}, ...]}`: exact typed
  equality lookup, with unique `from` values; unmapped input is an error.

Transforms run after reading a present non-null source value and before target
validation; they do not run on missing/null values or defaults. Each transform
target must be a mapped destination. Arithmetic for money uses checked decimal
operations, not accumulated binary floating-point arithmetic. No arbitrary
scripts, templates, regex replacements, or automatic currency conversion are
allowed. Dates and model IDs require correct source values or a dedicated
adapter, rather than speculative generic transformations.

For example, given a default `interfaces[0]` with a verified token tariff shape,
an upstream per-token decimal string may map as follows:

```yaml
mapping:
  id: provider_model_id
  cost.input: interfaces.0.pricing.input_tokens.no_cache
transforms:
  interfaces.0.pricing.input_tokens.no_cache:
    - kind: parse_decimal
    - kind: multiply
      factor: 1000000
```

The feed must document the upstream unit and currency. Absence of feed prices
MUST NOT remove models, fabricate a zero price, or copy another provider's
prices. Fields outside `writes` are rejected in the mapping rather than silently
written. Identity reads are allowed for joining a pricing-only observation.

Optional `filter` has `path` and nonempty `in` array of exact typed values. It
runs before normalization. A present recognized value outside the allowlist is
an excluded observation, not malformed data. A missing/null filter value makes
the run incomplete. A feed using lifecycle filtering must additionally declare
`filter.known_values` (including both allowed and intentionally excluded states)
before it can assert a complete snapshot. A value outside `known_values` makes
the run incomplete and forbids removals. `known_values` must include every value
in `in`; duplicate values in either list are invalid. The example's full
lifecycle vocabulary is fictional and must be verified for an actual provider.

### 8.4 Pagination and complete responses

`pagination` is required; `kind: none` explicitly asserts one response is the
complete listing within the declared scope. `kind: cursor` requires a response
`cursor_path`, query `parameter`, and positive `max_pages` (default limit 100
when omitted). `has_more_path` is optional.

For cursor pagination, the first request has no cursor. With `has_more_path`,
each response must contain a boolean: false completes the listing; true requires
a nonempty string cursor. Without it, the cursor field must exist: null or an
empty string terminates, a nonempty string continues. A missing field or wrong
type is failure, not completion. Cursors are opaque and URL-encoded as query
values; pagination never changes host, path, or auth.

Repeated cursors, exceeding `max_pages` while continuation remains, conflicting
duplicate native IDs, any failed page, or any item normalization error invalidate
the whole feed transaction. Identical duplicate observations may be deduplicated.
An empty intermediate page with a valid next cursor continues. A feed that needs
offsets, continuation headers, or another pagination mechanism requires an
explicit supported adapter; declaring `none` is not a workaround.

## 9. Reconciliation, ownership, and failures

`inventory` is required for a feed writing `models`:

| Field | Meaning |
| --- | --- |
| `mode` | `merge` only adds/updates; `snapshot` may also withdraw missing offers |
| `scope` | `public` provider inventory or credential-scoped `account` visibility |
| `complete` | Explicit boolean assertion of complete coverage of this feed's owned inventory |
| `allow_empty` | Default false; whether an observed empty complete listing is meaningful |
| `missing_runs` | Positive integer, default 2 |
| `minimum_missing_hours` | Nonnegative number, default 24 |

`snapshot` withdrawals require `complete: true`, all pages and items interpreted,
and a documented provider-authoritative membership source. An account-scoped
listing is not evidence of global withdrawal. In this revision `scope: account`
is restricted to `merge`; a separately scoped inventory can be introduced later.
Feeds that cannot establish completeness must use `merge` and report that limit.

Source-only `sync` records the membership `owner` feed ID, `last_seen_at`, and,
when missing, `missing_since` and `missing_runs`. `last_observation_id` prevents
reapplying the same membership observation. `sync.field_owners` maps model field
paths to feed IDs; `sync.interface_owners` is an array of objects containing
`protocol`, `operation`, effective `protocol_profile`, and their own
`field_owners` map of interface-relative paths to feed IDs. Ownership paths use
JSON Pointer, never mutable interface
array indices; parent/child ownership claims cannot overlap. For example,
`/status` may belong to the membership feed while an interface's `/pricing`
belongs to its pricing feed. Absence from an ownership map means manually owned
for an existing field. `sync.owner` grants membership reconciliation, not
permission to overwrite a manually owned status. Existing manually maintained
paths are not silently claimed: migration or a maintainer explicitly assigns
ownership. New imports assign the fields they observed/defaulted to the feed's
declared domains. A manual override removes that path from feed ownership.
Identity binding is maintainer-owned unless explicitly assigned to a feed.

Domain boundaries are fixed: `models` owns membership, `status`, and
`available_until`; `metadata` owns descriptive fields and verified interface
support/capabilities; `pricing` owns interface tariff subtrees. A `models` feed
can create identity and an empty `interfaces` array without metadata authority.
Deleting/replacing a metadata array must preserve separately owned prices and
manual fields, joining interfaces by `(protocol, operation, effective protocol_profile)`,
never by incidental array position. If it cannot do so unambiguously, reconciliation fails and
requires a reviewed change.

For each feed, the importer MUST:

1. Validate configuration, feature support, and environment prerequisites.
2. Fetch and normalize the entire listing into a staged observation set.
3. Resolve exact native IDs and explicit canonical bindings; detect conflicts.
4. Apply only owned field changes. Preserve unobserved, manually owned, and
   unknown data. Report conflicts instead of choosing an arbitrary winner.
5. For a complete authoritative snapshot, count absence only for that feed's
   owned offers. Withdraw only after both `missing_runs` and
   `minimum_missing_hours` are satisfied by consecutive successful complete
   runs in the sequence of successful complete observations. A failed/incomplete
   run pauses that sequence without incrementing or clearing evidence; a
   successfully observed present offer clears the window. Time elapsed during
   failures cannot substitute for a new complete absence observation.
6. Validate the entire candidate provider, then publish its changes atomically.

Withdrawal sets model `status: withdrawn`; it does not erase the record, identity,
manual metadata, or last known prices. Inventory may retain inactive records for
audit; only active ones are callable. A later complete/present observation can
reactivate a feed-owned status, but MUST NOT override a manual suspension or
withdrawal. Manual pruning of inactive historical rows is allowed; stable IDs
must not be reassigned. Missing inventory data never suspends the provider.

If membership belongs to a feed but a status override is manually owned, an
absence-driven status change is a reported ownership conflict, not an overwrite.
The maintainer must resolve that conflict explicitly before automatic withdrawal.

An empty listing without `allow_empty: true`, 401/403, missing secret, timeout,
invalid JSON, malformed row, failed page, unsupported feed semantics, or
uninterpretable lifecycle state MUST NOT count as missing models. Keep the last
good inventory and report the failure. Validating a failure is not a successful
availability refresh.

Provider jobs are isolated: failure of provider A does not prevent a validated
transaction for provider B from being proposed. Curation discovery and provider
sync stages do not write each other's source trees. A final publication step
reconciles curated availability, audits all canonical IDs against OpenRouter,
and builds a coherent artifact pair; it never deletes provider offers because
of curation. Dry-run computes the same candidate
and validation as write mode, without changing source/dist files.

The machine-readable report includes provider/feed ID, outcome (`updated`,
`unchanged`, `missing_secret`, `failed`, `unsupported`, `incomplete`), scope,
completeness, counts, changed field paths, proposed withdrawals, and sanitized
diagnostics. It never includes credentials or raw response bodies. Repeating
the same run observation must not double-count absence; each scheduled run has
a unique observation ID, and reapplying that ID is idempotent. Observation times
and IDs belong to source sync state/reporting, not invented build timestamps.

## 10. CI and release contract

### 10.1 Mandatory OpenRouter canonical identity audit

Every scheduled/manual sync and publication run MUST fetch OpenRouter's full
model list from `https://openrouter.ai/api/v1/models`, using the
[documented list API](https://openrouter.ai/docs/api/api-reference/models/list-all-models-and-their-properties).
Omit `offset`, `limit`, and all filtering parameters to request the full list.
Do not substitute a user-filtered catalog, a curated family subset, a provider's
own feed, or the previous run's cached response. Request execution remains
bounded and uses the explicit authentication rules in section 8.2.

The source root has `identity_source: {kind: openrouter, request: ...}`. Its
`request` uses the same URL/header/auth shape as provider feeds, with the fixed
full-list URL above and no query filters. Unknown identity-source kinds fail
the audit. Authentication is explicit; the documented bearer configuration uses
`auth: {kind: bearer, env: OPENROUTER_REGISTRY_API_KEY}`, bound to a repository
secret by the trusted workflow. An explicitly configured `none` requires
verified anonymous full-list support; a failed authenticated request never
downgrades to anonymous. This source-only configuration is not published.

The workflow MUST perform these stages in order:

1. Fetch and validate one complete identity snapshot for the run. Require a
   successful response with a nonempty `data` array, well-formed unique IDs,
   and understood full-list semantics. Missing/malformed IDs, duplicate IDs,
   truncation, an unexpected continuation, or unsupported pagination make the
   audit unavailable; a successful HTTP status alone is insufficient. Unknown
   optional metadata remains acceptable. If the API changes its completeness
   contract, update the adapter before using it as authority.
2. Stage provider imports, identity bindings, curation, and any agentic enrichment
   against that same snapshot. These stages may proceed for diagnostic purposes
   when fetching fails, but cannot commit/publish a sync candidate in that run.
3. After all mutations, scan the complete final candidate: every `models[].id`
   and every present `providers[].models[].canonical_id`, including unchanged,
   non-curated, manually maintained, suspended, and withdrawn records. Every
   value MUST exactly match a `data[].id` in the run's OpenRouter snapshot.
   `canonical_slug`, suffix stripping, case folding, and historical evidence
   are not substitutes. Also validate each provider binding's same-model evidence;
   merely finding a real but unrelated OpenRouter ID does not validate a mapping.
4. Check the source ledger for missing required current bindings and conflicting
   mappings. Report unreferenced historical IDs separately; they may remain in
   the source ledger but cannot justify an absent ID in either public artifact.
   Unbound native offers remain valid and are explicitly counted, not mislabeled
   as verified canonical IDs. Do not clear existing IDs automatically to make
   the audit pass or infer provider withdrawal from OpenRouter absence.
5. Only after a passing audit, run structural validation, deterministic build,
   and stale-dist checks, then commit/propose the complete source/artifact pair.
   Bind the audit to a digest of the complete candidate inputs and the final
   artifact snapshot. Any later source change, including agentic edits or rebase
   changes, invalidates the result and requires another audit. A later publication
   run fetches fresh evidence rather than treating an old PR report as approval.

The machine-readable audit records `outcome` (`passed`, `invalid_ids`, or
`unavailable`), source URL, fetch time, response SHA-256, run observation ID,
candidate digest, final artifact snapshot when built, counts of checked
references/distinct IDs/unbound offers, and every mismatch with source field
path, provider/native identity where relevant, and diagnostic code. Persist
the public response as a workflow audit artifact for reproduction; never store
request credentials. Reports distinguish `canonical_id_not_found`,
`canonical_binding_conflict`, and `canonical_source_unavailable`.

Any invalid referenced ID or unavailable/incomplete identity snapshot fails the
global publication gate and the final workflow status. Preserve the last valid
published pair, source bindings, and provider inventory; retain the failure
report and staged proposals for review without updating the publishable sync PR.
A missing ID requires reviewed correction, a verified migration to a current ID,
or removal of the public binding while retaining the native offer and historical
evidence. No automatic deletion, guessed remapping, or historical-ID exemption
is allowed. An ordinary provider-feed failure may still permit unrelated valid
updates; failure of this global audit permits no candidate publication.

Offline contributor checks and deterministic rebuilds may use a pinned synthetic
or recorded snapshot. They MUST identify that evidence and cannot claim fresh
OpenRouter validation or satisfy the trusted publication gate. Runtime readers
continue to use admitted registry snapshots without live OpenRouter requests.

### 10.2 Trusted workflow and publication

Scheduled sync runs on trusted repository code, using GitHub repository secrets
explicitly bound to feed environment names. For example:

```yaml
- name: Sync deterministic registry feeds
  env:
    EXAMPLE_REGISTRY_API_KEY: ${{ secrets.EXAMPLE_REGISTRY_API_KEY }}
    OPENROUTER_REGISTRY_API_KEY: ${{ secrets.OPENROUTER_REGISTRY_API_KEY }}
  run: cargo run -p dist-helper -- registry sync --write --report target/registry-sync.md
```

This is a configuration example, not a request to create a secret. Adding a
credentialed feed includes its explicit workflow env binding and documents the
required repository secret name. Do not export the entire secrets context or
interpolate secret values into shell commands. GitHub supplies an empty string
for an unset secret, and fork PRs normally receive no repository secrets;
see [GitHub's secret handling contract](https://docs.github.com/en/actions/how-tos/write-workflows/choose-what-workflows-do/use-secrets).

PR validation MUST be credential-free and use synthetic fixtures. It MUST NOT
run network feeds from untrusted PR YAML with privileged secrets. Scheduled or
manual trusted sync uses the checked-in feed URL/auth configuration. An agentic
enrichment step receives only its own credentials, not all provider feed keys,
and cannot change feed URLs, auth, ownership, or CI policy as part of sync.

Feed failures are collected, not swallowed. Subject to the global identity audit,
the scheduled workflow validates and proposes successful independent provider
updates, includes the failure summary, and records an unsuccessful final run status when required feeds fail
or lack secrets. A failure does not publish an invalid pair or masquerade as
“already up to date.” Implementation must arrange the report/build/PR steps
and final failure step explicitly so an early nonzero exit does not discard
successful independent results.

Before publication, pass the canonical identity audit, source validation,
deterministic build, and stale-dist check; commit both source and generated
artifacts together. Readers switch a whole validated pair atomically.
Private cache layout/versioning must prevent an
old unversioned cache from being mistaken for v1. Publish v1 through an explicit
v1 artifact location/configuration; do not replace the legacy consumer's default
feed in place during beta. A historical legacy snapshot may remain pinned, but
v1 code does not maintain a dual-format reader or promise ongoing v0 updates.

## 11. Conformance and stable freeze

Implementations are evaluated against the [conformance requirements](conformance.md)
for the roles and features they support. Schemas express structural rules; they
do not replace identity, ownership, lifecycle, billing, or routing semantics.
A claim of conformance must identify the supported protocol revision and roles.

Stable v1 freeze requires complete structural schemas with open extension points,
consistent worked examples, and verified old-reader/new-data compatibility.
The producer/consumer obligations and compatibility commitments in this contract
remain authoritative independently of implementation layout and release cadence.

## 12. References

External references explain why the feed contract is deliberately explicit:
[OpenAI's listing](https://developers.openai.com/api/reference/resources/models/methods/list)
returns IDs and basic metadata, not a complete price/capability contract.
[Claude's listing](https://platform.claude.com/docs/en/api/models/list) has cursor
pagination and lifecycle semantics. These are distinct wire responses even
though both use a `/v1/models` URL. Refer to provider documentation when enabling
each actual feed; the fictional examples above are not provider configurations.
