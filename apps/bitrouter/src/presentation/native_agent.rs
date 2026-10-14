//! Native conversation entries supplied to presentation consumers.

pub enum NativeEntryKind {
    User,
    Assistant,
    Detail,
    Tool { status: ToolStatus, detail: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolStatus {
    Running,
    Succeeded,
    Failed,
    Denied,
    Unknown,
}

pub struct NativeLive {
    pub item_id: Option<String>,
    pub text: String,
    pub truncated: bool,
}

pub struct NativeEntry {
    pub kind: NativeEntryKind,
    pub id: String,
    pub text: String,
}
