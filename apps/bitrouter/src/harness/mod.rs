//! Harness execution, workspace resources and durable session authority.
//!
//! Core scheduling requests execution through the harness boundary; session
//! commits and workspace effects remain owned by these modules.

pub mod acp;
pub mod agent;
pub mod catalog;
#[cfg(test)]
mod context;
pub(crate) mod control;
pub mod item;
pub(crate) mod local;
pub mod resources;
pub mod result_contract;
pub mod service;
pub mod storage;
#[cfg(feature = "harness")]
pub(crate) mod supervisor;
pub mod thread;
pub(crate) mod tools;
#[cfg(feature = "harness")]
pub mod trajectory;
pub mod turn;

pub mod permission;
