# Policy lock v4 conformance cases

1. Admit a valid v4 lock with artifact lineage and a nonempty policy, and reject
   versions 1–3, an unknown field in a strict structure and scalar tier targets.
2. An explicit route without its required certificate is rejected; a route's
   certificate must not be silently reused for a different classifier cohort.
3. Verify exact route, unknown-family fallback and default in that order.
4. Candidate compilation leaves the active lock unchanged; publication against
   a changed parent fails without overwriting the concurrent edit.
5. A policy action is still subject to provider, context and execution admission.
