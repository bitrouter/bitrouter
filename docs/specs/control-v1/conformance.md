# Control v1 conformance cases

These are observable cases for the beta contract, not a report of a particular
build. Enable the relevant host features and use independent read-only and
read-plus-reload credentials.

1. A missing or invalid bearer credential cannot read capabilities. Read-only
   discovery omits reload, and a direct reload request is denied.
2. Discovery identifies only enabled and authorized actions and supplies their
   request and response schemas. A client handles a missing optional descriptor.
3. Submit a valid reload using the sampled instance/generation. Disconnect and
   look up the same request ID on the same boot; the operation continues.
4. Replay the identical body under the same credential and verify no second
   reload is admitted. Reuse the ID with changed input: 409 conflict.
5. Stale generation, another boot and concurrent reload do not start a second
   operation. Unknown/expired results are not represented as a confirmed rollback.
6. A partially applied operation is a terminal report with participant outcomes,
   not a successful atomic transaction. Unknown response fields/codes do not
   make a client report success.
