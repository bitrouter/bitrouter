# Managed Core v1 conformance cases

1. Require a valid service key for capabilities even when ordinary inference
   skips auth. Reject an unsupported beta value before channel upgrade.
2. Require bind/restore within the initial deadline; unsupported operations
   fail rather than being inferred from a successful handshake.
3. Persist a checkpoint before sending its explicit ACK; observing HTTP/SSE
   output alone does not acknowledge a checkpoint.
4. Dropping the channel or observation does not establish cancellation or
   rollback. Treat `commit_status: unknown` as an unresolved outcome.
5. Reject malformed strict message input and unsupported required semantics.
