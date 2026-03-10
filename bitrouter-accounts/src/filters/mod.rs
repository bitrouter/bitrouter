//! Warp filter builders parameterized by authentication.
//!
//! Each public function returns a [`warp::Filter`] for a group of routes.
//! The caller supplies an `auth` filter that extracts an [`Identity`] — the
//! crate never decides *how* authentication works.
//!
//! # Example
//!
//! ```ignore
//! use bitrouter_accounts::filters;
//!
//! let auth = my_custom_auth_filter();           // impl Filter<Extract = (Identity,)>
//! let db: sea_orm::DatabaseConnection = /* … */;
//!
//! let routes = filters::account_routes(db.clone(), auth.clone())
//!     .or(filters::session_routes(db, auth));
//! ```

pub mod accounts;
pub mod sessions;

pub use accounts::account_routes;
pub use sessions::session_routes;

use sea_orm::DatabaseConnection;
use warp::Filter;

/// Inject [`DatabaseConnection`] into the filter chain.
pub(crate) fn with_db(
    db: DatabaseConnection,
) -> impl Filter<Extract = (DatabaseConnection,), Error = std::convert::Infallible> + Clone {
    warp::any().map(move || db.clone())
}
