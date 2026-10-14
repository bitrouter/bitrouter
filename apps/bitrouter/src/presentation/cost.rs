//! The cost line: what a session spent, and **whose** number that is.
//!
//! # Provenance, not scope
//!
//! ACP's `UsageUpdate.cost` is specified as this session's cumulative cost,
//! so a figure that arrives there is never wider than the session — the
//! daemon's total is answered by `bro status --requests`, not here. What
//! the specification cannot say is *who wrote the number*: two parties can. A
//! harness may report its own provider relationship, and BitRouter may report
//! its meter. They are different numbers, and a subscription harness's figure
//! is not BitRouter's spend.
//!
//! So the controller marks provenance in `_meta`, under
//! [`COST_PROVENANCE_META_KEY`]: present as [`COST_PROVENANCE_ROUTER`] when
//! BitRouter metered and attributed the session's traffic, absent when the
//! figure is the harness's own, forwarded untouched. [`from_usage`] reads
//! that marker back and [`Provenance`] carries it to the line.
//!
//! # What is never drawn
//!
//! A figure whose provenance is **unknown** — the marker is present but not
//! a value this renderer knows — is not drawn at all, because we do not know
//! whose it is, and the one thing this line must never do is show someone
//! else's number as ours. And no figure at all is `unreported`, never
//! `$0.00`: a client that cannot see a price has not observed a free turn.

use agent_client_protocol_schema::v1::UsageUpdate;

/// `_meta` key on a `UsageUpdate` naming whose figure `cost` is.
///
/// The same spelling the controller writes; the app pins the two constants
/// against each other so they cannot drift apart across the crate boundary.
pub const COST_PROVENANCE_META_KEY: &str = "bitrouter.dev/cost";

/// The [`COST_PROVENANCE_META_KEY`] value for a figure BitRouter metered.
pub const COST_PROVENANCE_ROUTER: &str = "router";

/// Who wrote a cost figure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    /// BitRouter metered this session's traffic and attributed it.
    Router,
    /// The harness's own figure — its provider relationship, not our meter.
    Harness,
}

/// A cost figure and who it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct Cost {
    amount: f64,
    currency: String,
    provenance: Provenance,
}

impl Cost {
    /// A figure that knows whose it is.
    ///
    /// There is deliberately no constructor taking a bare `UsageUpdate`: the
    /// provenance is a `_meta` marker, and a `From<UsageUpdate>` would have to
    /// guess at it. Guessing "ours" is the precise error this module exists
    /// to prevent.
    pub fn new(amount: f64, currency: impl Into<String>, provenance: Provenance) -> Self {
        Self {
            amount,
            currency: currency.into(),
            provenance,
        }
    }

    pub(crate) fn amount(&self) -> f64 {
        self.amount
    }
    pub(crate) fn currency(&self) -> &str {
        &self.currency
    }
    pub(crate) fn provenance(&self) -> Provenance {
        self.provenance
    }
}

/// Read a cost off a `UsageUpdate`, when it carries one whose provenance is
/// known.
///
/// `None` when the agent reported no cost, and when the marker names a
/// provenance this renderer does not know. The footer draws `unreported`
/// in its place — a number nobody can vouch for is worse than a blank.
pub fn from_usage(usage: &UsageUpdate) -> Option<Cost> {
    let cost = usage.cost.as_ref()?;
    let marker = usage
        .meta
        .as_ref()
        .and_then(|meta| meta.get(COST_PROVENANCE_META_KEY));
    let provenance = match marker {
        None => Provenance::Harness,
        Some(value) if value.as_str() == Some(COST_PROVENANCE_ROUTER) => Provenance::Router,
        Some(_) => return None,
    };
    Some(Cost::new(cost.amount, cost.currency.clone(), provenance))
}
