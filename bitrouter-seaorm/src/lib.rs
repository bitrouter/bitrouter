//! BitRouter SeaORM — SeaORM-backed implementations of the BitRouter Core
//! server service traits.
//!
//! This crate will provide persistence-backed implementations of:
//! - [`bitrouter_core::server::accounts::AccountService`]
//! - [`bitrouter_core::server::accounts::ApiKeyService`]
//! - [`bitrouter_core::server::sessions::SessionQueryService`]
//! - [`bitrouter_core::server::sessions::SessionWriteService`]
//! - [`bitrouter_core::server::blobs::BlobStore`]
//! - [`bitrouter_core::server::blobs::ObjectCatalog`]
//! - [`bitrouter_core::server::usage::UsageMeter`]
//!
//! Backend features (SQLite, Postgres, MySQL) will be gated behind
//! Cargo feature flags.
