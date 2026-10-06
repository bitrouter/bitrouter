//! Application-side discovery of the installed Antigravity (`agy`) OAuth client.
//! Request authentication, project binding, protocol adaptation and confidential
//! refresh live in `bitrouter_ai::providers::antigravity`. The application explicitly
//! supplies this module's permitted local secret source to that refresher.

pub mod agy_client;
