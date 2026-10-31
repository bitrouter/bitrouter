# Resource provisioning conformance

A producer and consumer must demonstrate these cases against their exact
artifacts. Passing schema validation alone does not establish conformance.

1. Missing nullable CAS fields, unknown fields, duplicate fields, wrong kinds,
   oversized secrets/documents and duplicate credential bindings are rejected.
2. Concurrent creates/rotations have one CAS winner. Identical operation replay
   returns the original revision; changed content with that ID conflicts.
3. Secret plaintext is absent from reads, operation records and diagnostics;
   retained content fails authentication under the wrong wrapping key.
4. Same-name private resources are isolated by owner. A tenant cannot mutate
   pool resources or read/compile foreign private dependencies, even through a
   route or policy. Permission reductions take effect on subsequent checks.
5. Tenant routes cannot select an unapproved origin or override its authority
   with headers. Valid operator-approved endpoints remain usable.
6. Rotation preserves old immutable bytes and configurations. Revoking an old
   or current revision fences cached dispatch and new compilation/activation;
   restart retains the same revocation and managed head.
7. Lost replies, handler cancellation, shutdown and transaction faults preserve
   a single atomic mutation/outcome or an explicitly unknown result. Replaying
   the original operation cannot create another resource revision.
8. HTTP clients validate installation, operation and resource identities in
   replies. Bounded decoding rejects mismatched or oversized responses.
9. Actual service processes accept SDK-created resources and execute authorized
   JSON/SSE over HTTP/1.1 and HTTP/2 without manifest edits or recompilation.
10. Existing strict tenancy discovery and receipt clients retain their original
    wire shape. Single-instance results make no distributed-ownership claim.
