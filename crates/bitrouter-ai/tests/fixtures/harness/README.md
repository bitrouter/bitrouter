# Harness wire contract fixtures

These constructed fixtures exercise the public HTTP v2 representations present
at `f569357`, including nested recovery/resources/context inspection, legacy
optional fields, historical instance IDs and continuing observation variants.
They are not captured database/session bytes and do not establish historical
persisted-format compatibility.

`harness_contracts.rs` checks serde fidelity and diagnostic redaction. The
application's `agent_api::core_disabled_tests` and `harness_sdk` integration
target separately exercise actual handler output through the SDK with and
without native execution. Runtime authority and private checkpoints remain
application-owned.
