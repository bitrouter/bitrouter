//! Terminal-independent choices shared by commands and selection views.

/// A selectable label and its searchable context (for example a provider id).
pub struct Item {
    pub label: String,
    pub detail: String,
}

impl Item {
    pub fn new(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            detail: detail.into(),
        }
    }
}
