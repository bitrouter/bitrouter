#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PaginationRequest {
    pub limit: Option<u32>,
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
}
