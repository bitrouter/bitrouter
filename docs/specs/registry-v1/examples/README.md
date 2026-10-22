# Registry v1 worked examples

These are synthetic design examples for
[`1.0.0-beta.1`](../../REGISTRY_V1_SPEC.md), not generated production data or
claims about real model support, prices, or credentials. All URLs and providers
are fictional. The artifact pair has a computed shared snapshot digest.

- [`models.json`](models.json) curates only `example/chat-2` and
  `example/judge-1`.
- [`providers.json`](providers.json) also retains an older active chat model,
  a native model with no canonical binding or known price, and a future image
  interface with a gated non-token tariff. An older reader can keep the image
  record without invoking its unknown protocol or treating the tariff as free.
- The mixed chat/Decisions offer has interface-specific capabilities and prices;
  the chat price must not be applied to a Decisions request.
- `example:~native/Vendor%2FExperimental%3ABeta%251` addresses the exact native
  ID `Vendor/Experimental:Beta%1`. It is not an alias for another provider's
  model with the same spelling.
- [`feed.json`](feed.json) is one source-only feed object. Its two responses are
  [`page-1.json`](page-1.json) and [`page-2.json`](page-2.json). It is a separate
  import example, not the complete source for the artifact pair.

For that feed, the importer sends a first request without `after`; page 1
requires a second request with query value `after=cursor/2` (URL-encoded).
Both pages must succeed before reconciliation. The two observed native IDs are
`chat-2` and `chat-1`; the explicit prefix transform binds them to
`example/chat-2` and `example/chat-1` without consulting curated membership.
Input rates `"0.00000125"` and `"0.000001"` normalize to 1.25 and 1 USD per
million tokens. No output rate is invented. Both models remain in provider
inventory even though only the newer one is curated.

`EXAMPLE_REGISTRY_API_KEY` names a hypothetical environment variable. With no
value, this feed reports `missing_secret` and makes no request. It does not
inherit the runtime credential `EXAMPLE_API_KEY` in the provider artifact.

The extension fields at envelope/model/interface levels are intentional
round-trip data. Later implementation tests should build both positive and
negative scenarios from small synthetic fixtures rather than freeze commercial
catalog contents. The full conformance list is in the
[delivery plan](../../REGISTRY_V1_IMPLEMENTATION.md#3-conformance-matrix).
