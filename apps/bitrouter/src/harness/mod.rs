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
pub mod resources;
pub mod service;
pub mod storage;
pub mod thread;
pub(crate) mod tools;
pub mod turn;

pub mod permission;
