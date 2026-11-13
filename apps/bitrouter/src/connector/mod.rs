//! Observation and explicitly enabled bounded local execution for an
//! operator-declared fixed-model inference pool. No commercial capacity contract.

pub mod capabilities;
pub mod config;
pub mod controller;
pub mod executor;
pub mod journal;
mod metrics;
pub mod observer;
pub mod policy;
pub mod report;
pub mod request;
pub mod server;

#[cfg(test)]
mod tests;

#[cfg(all(test, unix))]
mod execution_tests;
