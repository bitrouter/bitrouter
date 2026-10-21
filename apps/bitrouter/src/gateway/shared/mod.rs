//! Shared gateway authority. Local operation never initializes this backend.

pub(crate) mod admitted;
mod authentication;
mod authority;
pub(crate) mod bootstrap;
pub(crate) mod capture;
pub(crate) mod continuation;
pub(crate) mod credentials;
mod crypto;
pub(crate) mod dispatched;
mod engine;
mod generations;
mod http;
mod operator;
mod operator_service;
mod ownership;
mod publication;
pub(crate) mod runtime;
mod schema;
mod store;
pub(crate) mod telemetry;
mod transport;

#[cfg(test)]
mod test_database;

#[cfg(test)]
mod tests;
