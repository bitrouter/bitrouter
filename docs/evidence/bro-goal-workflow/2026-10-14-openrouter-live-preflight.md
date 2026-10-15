# OpenRouter live connectivity preflight

The user supplied a test OpenRouter credential and explicitly authorized its
use for the active Goal. Its value is stored only in a 0600 file under a
0700 temporary directory outside the repository. No credential/header file
is included in evidence. The user's normal configuration was not changed.

Stripe Directory was checked first under the installed skill. The installed
CLI lacks Directory; its plugin install endpoint returned not found. That
failure did not create an account, subscription or payment. Existing SDK
source and official OpenRouter documentation were used for the supplied key.

Official [Jev documentation](https://openrouter.ai/docs/guides/community/jev)
confirms the same OpenRouter key supports actual TypeSafe System One inference.
The [SDK guide](https://openrouter.ai/docs/guides/community/typesafe-sdk)
documents base URL `https://openrouter.ai/api` and `/v1/systemone`.
The existing Rust executor already appends that path and validates the
TypeSafe response shape. The earlier suggestion that another TypeSafe
credential was required was incorrect and was corrected before execution.

Two actual HTTP preflight requests returned 200, with no retry:

- System One requested `typesafe/jev-1.13`; the provider reported
  `typesafe/jev-1.13-20260917`, TypeSafe, 953 input/207 output tokens,
  `usage.cost = 0.000040026` USD. The benign hypothetical pause probe produced
  query=present and modify/permission_response=absent. Control's highest
  probability was pause (0.47), confidence 0.34; target was unknown (0.7).
  These original conflicting/uncertain outputs are retained unchanged.
- Reasoning requested and reported `openai/gpt-6.1-sol`, OpenAI, 76 input/35
  output tokens, `usage.cost = 0.000502` USD. Its small connectivity prompt
  returned a Chinese explanation with control=null. It was not the complete
  production Goal interpreter prompt.

Total reported cost for these two calls is 0.000542026 USD. No latency or
cost-benefit benchmark is inferred. The requests contain no expected labels,
forbidden-effect annotations, secret or repository content. Sanitized evidence
retains only model/provider/request identity, actual answers and usage.

These calls establish credential and endpoint availability. They do **not**
prove Rust executor invocation, durable Goal input effects, real harness
adoption, calibration or the 25-case/8-sequence evaluation gate. The typed
probe uses a small synthetic state, not the authoritative production fixture;
the reasoning probe is not a comparable baseline. No semantic accuracy rate
is reported. In particular, its uncertain pause answer must never directly
authorize stopping a Goal. The current production path still upgrades all
typed proposals to reasoning and then performs runtime validation.

The credential unlocks the next actual Goal-runtime input runs. Those must
record native proposals, runtime refusals, committed effects and actual
execution evidence separately, use the same persisted fixture per arm and
preserve original user text and identities. Runtime token estimates are not
presented as the exact provider-reported bills above.
