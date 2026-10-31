# Usage evidence conformance

1. Strict decoding rejects missing/duplicate/unknown fields, invalid integers,
   source/protocol mismatches, foreign attempt IDs and oversized documents.
2. Repeated native frames retain order and snapshot semantics. Explicit zero,
   absent counters, invalid counters, estimates and unsupported fields remain
   distinguishable. Arbitrary field names and payload strings never leak.
3. JSON and SSE capture reported cache/search/media dimensions before conversion.
   HTTP failures and any authorized credential-refresh attempt preserve distinct
   attempt attribution. Malformed or mislabeled SSE cannot forge eligible usage.
4. Frame and byte exhaustion retain a bounded prefix and sticky truncation;
   subsequent frames cannot silently reset the document's completeness state.
5. Completed, rejected, partial and disconnected executions retain their actual
   observations without claiming unseen work was free or successful. No evidence
   read, response loss or stream failure causes inference replay.
6. An estimator is recorded only when used, with the exact algorithm revision,
   numerical inputs and coverage. It never overwrites native evidence.
7. Receipt and evidence commit atomically. Storage faults, duplicate terminal
   delivery, shutdown and restart preserve one bound outcome. Missing, orphaned,
   tampered and oversized evidence fail integrity checks; historical absence is
   explicit and cannot mask a deleted required document.
   Recovery of a versioned capture seed without observations produces durable
   `unavailable` evidence; a historical seed alone produces `not_captured`.
8. Reads verify current receipt authority and exact installation/tenant/execution
   binding. SDK HTTP/1.1 and HTTP/2 decode the same bounded contract and reject
   foreign identities without retry.
9. Advertised owned child linkage comes from independent admissions. Parent and
   child retain separate attempt/usage ledgers with no double counting. Generic
   server tool capture remains unavailable unless separately implemented.
10. Existing v1 receipt/discovery schemas are unchanged. A consumer that does
    not implement this additional contract can continue reading the old receipt.
