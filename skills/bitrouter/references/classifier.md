# Classifier APIs

BitRouter accepts typed classifier calls through `POST /v1/decisions` and
`POST /v1/systemone` on `http://127.0.0.1:4356`. Predicate, choice and score
questions share evidence and return typed judgments. These calls have no
generation stream, tool loop or conversation continuation.

## Provider setup

Use `OPENAI_API_KEY` for native OpenAI Decisions and `TYPESAFE_API_KEY` for
TypeSafe System One. These are upstream credentials. When `server.skip_auth`
is false, the local HTTP caller separately supplies its BitRouter virtual key.
ChatGPT subscription credentials do not establish Decisions support.

TypeSafe's upstream base is `https://api.typesafe.ai/v1`; the transport appends
`/systemone`. The registry declares `typesafe/jev-1.13.0`, `typesafe/jev-latest`
and `typesafe/jev-preview`. Versioned IDs keep threshold evaluations stable;
aliases can change. Provider activation depends on the corresponding key and
an installed catalog containing the protocol/model declarations.

```bash
curl http://127.0.0.1:4356/v1/systemone \
  -H "Authorization: Bearer $BITROUTER_VIRTUAL_KEY" \
  -H 'Content-Type: application/json' \
  -d '{
    "model": "typesafe/jev-1.13.0",
    "state": {"build": "All required checks passed."},
    "questions": {
      "passed": {"type": "noul", "instructions": "Did all required checks pass?"}
    }
  }'
```

System One preserves structured state, instructions and criteria. Choices have
string keys. Scores use ordered levels. Map question keys remain independent
of OpenAI's optional question names. The response reports the actual upstream
model/version and provider usage.

## Conversion and admission

Both APIs use Classifier internally. A System One caller can select a Decisions
target for plain-text predicates, string choices and simple score rubrics.
The response retains the caller's map keys. Choice/score confidence is derived
for System One using its documented statistic; the original upstream confidence
remains evidence, and thresholds are not assumed to transfer between models.

Structured text, separate Noul criteria, split score descriptions, boolean
choices, images and scoped safety options require their native supported
protocol until a corresponding conversion rule exists. Incompatible targets
are excluded before model I/O. Provider pins cannot bypass admission.

Decisions callers cannot currently route to System One because Jev's reported
usage lacks the required OpenAI cache/reasoning breakdown. Missing counters
remain unknown rather than becoming zero. This gate also applies to otherwise
compatible predicates. Native System One works independently of this gate.

OpenAI refusals cannot be represented as System One answers. Such a completed
call returns a gateway output error, retains its usage and settles once. It
never generates a replacement probability or retries paid model work. Completed
malformed output follows the same accounting rule; pre-completion HTTP failures
follow the configured fallback policy.

## Pricing and evidence

`pricing_by_protocol` is independent for `decisions` and `systemone`; it does
not inherit generation pricing. Tariffs are frozen for the actual selected
upstream protocol and endpoint. The caller's API format does not choose the rate.

System One's input-only tariff uses the reported input total. Reported output
tokens remain visible even when their rate is zero. Charge evidence records
`billable_input_tokens` independently of unknown normalized breakdowns and
retains explicit usage availability. `bro requests` preserves the native
usage and frozen tariff evidence; a charge estimate is not an invoice receipt.

Decisions cache billing remains unverified: nonzero cache counters retain usage
but make cost unavailable. `server.require_known_pricing` still rejects routes
whose actual tariff lacks guaranteed coverage. See [Decisions details](decisions.md)
for regional endpoint/tariff applicability. A catalog update or local fixture
does not establish hosted Cloud support.
