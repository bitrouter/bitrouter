# Registry v1 worked examples

These are informative synthetic examples for
[`1.0.0-beta.6`](../README.md), not production records or claims about commercial
model support, prices, or credentials. The provider URLs and model identities
are fictional. The identity-source request uses the real documented API shape;
its response fixture is synthetic and establishes no live OpenRouter validity.

- [`registry.json`](registry.json) is the sole public document. `data.models`
  recommends `example/chat-2` and `example/judge-1`; `data.providers` additionally
  retains an older active chat model, an unbound native offer with unknown price,
  and a future image interface. Canonical joins are derived from provider rows;
  no reverse index, companion document, or embedded paired digest is needed.
- [`identities.json`](identities.json) illustrates the source-only identity ledger,
  including non-curated identities and exact bindings for `example` and the
  separate `feed-example` importer. Actual bindings require actual evidence.
- [`identity-source.json`](identity-source.json) is the source root's
  `identity_source` configuration. It explicitly requests
  `output_modalities=all`; omitting that query would default to text-only output.
- [`openrouter-models.json`](openrouter-models.json) is the minimal full identity
  response fixture, including text, decision, image output, and an excluded batch
  variant. The full audit retains that variant, but no registry offer or new
  ledger binding imports it. Explicit endpoint evidence enables the chat rows.
  Every public
  canonical ID appears in `data[].id`. One `canonical_slug` differs intentionally;
  substituting it, narrowing by output modality, or relying on ledger history
  fails the audit. `total_count` agrees with the complete list.
- [`feed.json`](feed.json) is one source-only required feed. It uses JSON Pointer
  for every mapping/response path and an explicit header credential env binding.
  [`page-1.json`](page-1.json) and [`page-2.json`](page-2.json) are its responses.
  It is not the entire source of the public registry example.
- [`reconciliation.json`](reconciliation.json) illustrates absence observations
  A(10), B(11), stale replay A(10), return C(12), and explicit status D(13).
  With two missing runs and a 24-hour minimum, B withdraws the offer; replay A
  changes nothing; C restores feed-owned active status through the declared
  status default. Explicit mapped status wins; manual status stays authoritative.
- [`charges.json`](charges.json) gives exact proportional decision/request and
  disjoint-token arithmetic. Three decisions at USD 0.20 per 1000 cost USD 0.0006.
  No per-question/request rounding or started-block charging is implied.
- [`consumption.json`](consumption.json) traces first-party startup from a stale
  source-bound cache, a failed refresh, a successful unchanged download, and
  disabling automatic refresh. Its settings are the proposed v1 contract, not
  a claim of current CLI support; its URL is fictional and never fetched.

The mixed language/decision offer retains one native and canonical identity, with
separate interfaces and rates. The judge illustrates `decisions/default` and two
`systemone` profiles. Its metered rates and counter names are hypothetical; an
implementation lacking their codec/counter rules retains the records but cannot
invoke or price them. Matching operation names do not authorize lossy conversion.

`example/chat-1` is absent from recommendations and has no quality score, yet an
explicit user-defined router can select it. The native selector
`example:~native/Vendor%2FExperimental%3ABeta%251` resolves exactly to
`Vendor/Experimental:Beta%1`. It grants no cross-provider alias or extra capability.

For the feed, page 1 requires a second request with `after=cursor/2`, URL-encoded.
Both pages must succeed before reconciliation. Prefixing the native IDs `chat-2`
and `chat-1` proposes `example/chat-2` and `example/chat-1`; the ledger's exact
`feed-example` bindings verify them independently of curation. Without evidence,
keep native offers unbound and report unresolved identity. Input strings
`"0.00000125"` and `"0.000001"` normalize to 1.25 and 1 USD per million tokens;
missing output prices remain unknown.

`EXAMPLE_REGISTRY_API_KEY` and `OPENROUTER_REGISTRY_API_KEY` name hypothetical
environment variables, never secret values. Missing values make no authenticated
request and fail the required stage. Inference credentials are independent.

Unknown optional metadata round-trips through the entire document. Unsupported
profiles disable their interface; unknown meters make prices unavailable.
Malformed/incompatible refreshes preserve the last valid document and leave the
host alive. Any missing referenced canonical ID, failed complete identity fetch,
or post-audit candidate edit prevents publication without deleting native offers.
See the [conformance requirements](../conformance.md#conformance-cases) for the
corresponding observable behavior; these fixtures are not completed runtime tests.

The consumption trace uses `registry.json` as its accepted complete document.
At startup it serves the stale cache immediately; an incompatible response
preserves both the document and old `checked_at`, then schedules a retry one hour
later. An identical valid response advances only the local check time and
schedules the next check 24 hours later. Disabling automatic refresh cancels
that timer without removing the admitted document. Document/evidence timestamps
are never renewed by these events.

An implicit source in a beta-only release resolves to that release's exact
protocol-revision stream, which may receive compatible data updates. An explicit
commit URL instead freezes a snapshot. A stable-supporting upgrade changes the implicit default
to the stable v1 stream and uses that source's local data; an explicit URL
remains fixed. A custom URL never borrows the official bundle on failure. See
the [first-party consumption profile](../README.md#11-bitrouter-first-party-consumption-profile)
for the default URL and complete selection rules.
