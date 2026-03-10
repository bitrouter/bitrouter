//! Account and session management for bitrouter.
//!
//! This crate provides:
//!
//! - **Entity types** — [`Account`](entity::account), [`ApiKey`](entity::api_key),
//!   [`Session`](entity::session), [`Message`](entity::message) backed by sea-orm.
//! - **Migrations** — Schema management via [`Migrator`](migration::Migrator).
//! - **Services** — [`AccountService`](service::AccountService) and
//!   [`SessionService`](service::SessionService) for data operations.
//! - **Warp filter builders** — [`filters`] module exposes route constructors
//!   parameterized by a caller-supplied auth filter.
//!
//! # Auth model
//!
//! This crate does **not** implement authentication. Instead, route builders
//! accept a warp [`Filter`](warp::Filter) that extracts an [`Identity`] from
//! the incoming request. The caller (typically `bitrouter-runtime` or a custom
//! server) provides the concrete auth implementation — API key lookup, JWT
//! validation, admin key, etc.
//!
//! See the [`identity`] module for the [`Identity`] and [`Scope`] types that
//! form the contract between auth and the account/session layer.

pub mod entity;
pub mod filters;
pub mod identity;
pub mod migration;
pub mod service;
