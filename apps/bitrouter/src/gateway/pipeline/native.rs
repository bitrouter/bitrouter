//! Request-local response ingress bound.

/// Maximum decoded HTTP entity bytes consumed for one managed provider reply.
pub(crate) struct NativeResponseByteLimit(pub u64);
