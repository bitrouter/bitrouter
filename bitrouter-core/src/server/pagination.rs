/// A cursor-based page of results.
#[derive(Debug, Clone)]
pub struct CursorPage<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
    pub has_more: bool,
}

/// Parameters for a cursor-based page request.
#[derive(Debug, Clone)]
pub struct PageRequest {
    pub cursor: Option<String>,
    pub limit: u32,
}

impl Default for PageRequest {
    fn default() -> Self {
        Self {
            cursor: None,
            limit: 20,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_page_request() {
        let pr = PageRequest::default();
        assert_eq!(pr.limit, 20);
        assert!(pr.cursor.is_none());
    }
}
