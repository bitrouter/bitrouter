//! Standard JSON-RPC 2.0 error codes used by the MCP server protocol.

/// Invalid JSON was received.
pub const PARSE_ERROR: i64 = -32700;

/// The JSON sent is not a valid JSON-RPC request.
pub const INVALID_REQUEST: i64 = -32600;

/// The method does not exist or is not available.
pub const METHOD_NOT_FOUND: i64 = -32601;

/// Invalid method parameter(s).
pub const INVALID_PARAMS: i64 = -32602;

/// Internal JSON-RPC error.
pub const INTERNAL_ERROR: i64 = -32603;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_negative() {
        assert!(PARSE_ERROR < 0);
        assert!(INVALID_REQUEST < 0);
        assert!(METHOD_NOT_FOUND < 0);
        assert!(INVALID_PARAMS < 0);
        assert!(INTERNAL_ERROR < 0);
    }

    #[test]
    fn codes_match_spec() {
        assert_eq!(PARSE_ERROR, -32700);
        assert_eq!(INVALID_REQUEST, -32600);
        assert_eq!(METHOD_NOT_FOUND, -32601);
        assert_eq!(INVALID_PARAMS, -32602);
        assert_eq!(INTERNAL_ERROR, -32603);
    }
}
