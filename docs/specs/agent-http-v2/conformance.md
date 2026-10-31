# Native Thread HTTP v2 conformance cases

1. The dedicated bearer is required; capabilities identify the serving instance
   and do not advertise disabled native execution operations.
2. A request targeting an unauthorized workspace or a stale server instance is
   rejected. A client rediscovers after restart before a new mutation.
3. Matching mutation/key replay does not execute twice; changed content using
   the same key conflicts. Missing, non-ASCII or oversized keys fail.
4. Enqueued turn admission is not completion. Disconnecting observation does
   not cancel accepted execution; history remains accessible with authority.
5. An SSE error and a truncated stream do not become a successful terminal.
