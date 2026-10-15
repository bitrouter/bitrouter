//! Evidence-grounded evaluation and policy-block learning from stored ACP data.

pub mod bandit;
mod catalog;
pub mod control;
pub mod costs;
pub mod evidence;
pub mod jobs;
pub mod judge;
pub mod learning;
pub mod operator;
pub mod rubric;
pub mod runtime;
pub mod scheduler;
pub mod scoring;
pub mod service;

#[cfg(test)]
mod tests;
