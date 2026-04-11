//! Account and session management for bitrouter.
//!
//! This crate provides:
//!
//! - **Entity types** — [`Account`](entity::account),
//!   [`RotatedPubkey`](entity::rotated_pubkey),
//!   [`Session`](entity::session), [`Message`](entity::message) backed by sea-orm.
//! - **Migrations** — Individual migration steps exported via
//!   [`migration::migrations()`](migration::migrations).
//! - **Services** — [`AccountService`](service::AccountService),
//!   [`SessionService`](service::SessionService), and
//!   [`DbRevocationSet`](service::DbRevocationSet) for data operations.
//! - **Warp filter builders** — [`filters`] module exposes route constructors
//!   parameterized by a caller-supplied auth filter.
//!
//! # Auth model
//!
//! This crate does **not** implement authentication. Instead, route builders
//! accept a warp [`Filter`](warp::Filter) that extracts an [`Identity`] from
//! the incoming request. The caller (typically the `bitrouter` binary or a
//! custom server) provides the concrete auth implementation — EdDSA JWT
//! validation, etc.
//!
//! See the [`identity`] module for the [`Identity`] and [`Scope`] types that
//! form the contract between auth and the account/session layer.

pub mod entity;
pub mod filters;
pub mod identity;
pub mod migration;
pub mod policy;
pub mod service;
