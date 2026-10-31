# Native ACP conformance cases

1. Initialize v1 and v2 separately and verify version-specific capability
   advertisement. Unsupported versions and methods fail rather than downgrade.
2. For each enabled version, create a session and prompt. In v2, acceptance is
   not reported as completion before the terminal update.
3. Disconnect after admission; reconnect with current authority and observe the
   same session without canceling the accepted turn.
4. Cancel active work and verify accepted queued work stays paused. Close an
   enabled session and verify active and queued work are canceled but history
   remains available to an authorized client.
5. Reject an invalid array member instead of silently dropping it. A session ID
   or client-provided working directory does not bypass authorization.
6. With core disabled, do not advertise native execution/queue controls that
   depend on core. Approval reattachment must not create a second approval.
7. Retry the same durable operation with its namespaced key and verify the
   original receipt; a different payload with the same key conflicts.
