//! Task-specific views over immutable evidence. A view changes a prompt, never
//! the source conversation. Decision output is advisory to this compiler.

pub mod evidence;
pub mod models;
pub mod planner;
pub(crate) mod tasks;
pub mod tools;
