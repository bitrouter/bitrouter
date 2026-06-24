//! `bitrouter update` — in-place self-updater built on cargo-dist's
//! `axoupdater`. Decision logic (install-method detection, release channel,
//! nudge-cache TTL) lives in small pure functions so it can be unit-tested
//! without touching the network or replacing the running binary.

#[cfg(test)]
mod tests {}
