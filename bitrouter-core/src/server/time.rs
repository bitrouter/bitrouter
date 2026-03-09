use std::time::{Duration, SystemTime};

pub type Timestamp = SystemTime;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetentionPolicy {
    Keep,
    ExpireAt(Timestamp),
    ExpireAfter(Duration),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleState {
    Active,
    Archived,
    Deleted,
}
