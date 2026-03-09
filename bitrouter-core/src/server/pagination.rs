#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaginationRequest {
    pub limit: usize,
    pub cursor: Option<String>,
}

impl PaginationRequest {
    pub const DEFAULT_LIMIT: usize = 50;
    pub const MAX_LIMIT: usize = 200;

    pub fn with_limit(limit: Option<usize>, cursor: Option<String>) -> Self {
        let limit = limit.unwrap_or(Self::DEFAULT_LIMIT).min(Self::MAX_LIMIT);
        Self { limit, cursor }
    }
}

impl Default for PaginationRequest {
    fn default() -> Self {
        Self::with_limit(None, None)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::PaginationRequest;

    #[test]
    fn pagination_request_uses_default_limit() {
        let request = PaginationRequest::default();
        assert_eq!(request.limit, PaginationRequest::DEFAULT_LIMIT);
        assert_eq!(request.cursor, None);
    }

    #[test]
    fn pagination_request_caps_large_limits() {
        let request = PaginationRequest::with_limit(Some(500), None);
        assert_eq!(request.limit, PaginationRequest::MAX_LIMIT);
    }
}
