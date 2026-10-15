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

    /// A display label that retains the figure's provenance.
    pub(crate) fn label(&self) -> String {
        let figure = format!("{} {:.4}", self.currency, self.amount);
        match self.provenance {
            Provenance::Router => format!("{figure} (router)"),
            // Keep the caveat first when a consumer truncates the label.
            Provenance::Harness => format!("agent-reported {figure}"),
        }
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

/// Absence is not evidence that the turn was free.
pub(crate) const UNREPORTED: &str = "cost unreported";

#[cfg(test)]
mod tests {
    use agent_client_protocol_schema::v1::Cost as WireCost;

    use super::*;
    use crate::presentation::cost::{COST_PROVENANCE_META_KEY, COST_PROVENANCE_ROUTER, from_usage};
    use agent_client_protocol_schema::v1::UsageUpdate;

    fn usage_with(cost: Option<WireCost>, marker: Option<serde_json::Value>) -> UsageUpdate {
        let mut usage = UsageUpdate::new(1_500, 200_000);
        usage.cost = cost;
        if let Some(marker) = marker {
            let mut meta = serde_json::Map::new();
            meta.insert(COST_PROVENANCE_META_KEY.to_string(), marker);
            usage.meta = Some(meta);
        }
        usage
    }

    /// The two provenances must be tellable apart at a glance: ours is the
    /// bare figure, the harness's is labelled as the agent's.
    #[test]
    fn the_two_provenances_render_differently() {
        let ours = Cost::new(0.42, "USD", Provenance::Router).label();
        assert!(ours.contains("0.42"), "{ours:?}");
        assert!(!ours.contains("agent"), "{ours:?}");

        let theirs = Cost::new(1.32, "USD", Provenance::Harness).label();
        assert!(theirs.contains("1.32"), "{theirs:?}");
        assert!(
            theirs.contains("agent"),
            "the harness's own figure must be marked as not ours: {theirs:?}"
        );
        assert_ne!(ours, theirs);
    }

    /// The caveat precedes the number, so truncation loses the figure rather
    /// than the warning about it.
    #[test]
    fn the_harness_caveat_precedes_the_figure() {
        let rendered = Cost::new(1.32, "USD", Provenance::Harness).label();
        assert!(
            rendered.starts_with("agent-reported USD 1.3200"),
            "{rendered:?}"
        );
    }

    /// No cost reported is not a cost of zero.
    #[test]
    fn an_absent_cost_is_unreported_not_zero() {
        let rendered = UNREPORTED;
        assert!(rendered.contains("unreported"), "{rendered:?}");
        assert!(
            !rendered.contains('0'),
            "an unobserved price must not read as a free turn: {rendered:?}"
        );
        assert!(from_usage(&usage_with(None, None)).is_none());
        assert!(
            from_usage(&usage_with(None, Some(serde_json::json!("router")))).is_none(),
            "a marker with no figure is still no figure"
        );
    }

    /// The marker the controller writes is the one that makes a figure ours.
    #[test]
    fn the_router_marker_makes_the_figure_ours() {
        let read = from_usage(&usage_with(
            Some(WireCost::new(0.42, "USD")),
            Some(serde_json::json!(COST_PROVENANCE_ROUTER)),
        ));
        assert_eq!(read, Some(Cost::new(0.42, "USD", Provenance::Router)));
    }

    /// No marker means the harness wrote the number itself. It is shown, and
    /// shown as the agent's — never as though BitRouter had measured it.
    #[test]
    fn an_unmarked_cost_is_the_harness_own() {
        let read = from_usage(&usage_with(Some(WireCost::new(9.99, "EUR")), None));
        assert_eq!(read, Some(Cost::new(9.99, "EUR", Provenance::Harness)));
        assert_eq!(
            read.map(|cost| cost.label()),
            Some("agent-reported EUR 9.9900".into())
        );
    }

    /// A marker this renderer does not know is a figure nobody here can
    /// vouch for. It is not drawn — not as ours, and not as the agent's.
    #[test]
    fn an_unknown_provenance_is_never_drawn() {
        for marker in [serde_json::json!("something-new"), serde_json::json!(1)] {
            assert!(
                from_usage(&usage_with(Some(WireCost::new(0.42, "USD")), Some(marker))).is_none(),
                "an unrecognised marker must not reach the screen"
            );
        }
    }
}

#[cfg(test)]
mod wire_tests {
    use agent_client_protocol_schema::v1::UsageUpdate;

    fn usage(cost: bool, marker: Option<&str>) -> UsageUpdate {
        let mut usage = UsageUpdate::new(1_500, 200_000);
        if cost {
            usage.cost = Some(agent_client_protocol_schema::v1::Cost::new(0.42, "USD"));
        }
        if let Some(marker) = marker {
            let mut meta = serde_json::Map::new();
            meta.insert(
                crate::presentation::cost::COST_PROVENANCE_META_KEY.to_string(),
                serde_json::Value::String(marker.to_string()),
            );
            usage.meta = Some(meta);
        }
        usage
    }

    fn rendered(usage: &UsageUpdate) -> String {
        crate::presentation::cost::from_usage(usage)
            .map_or_else(|| super::UNREPORTED.to_string(), |cost| cost.label())
    }

    /// No figure at all renders *unreported*, never a zero. This is the case
    /// every harness that does not report cost lands in — and every
    /// unmetered BitRouter session, which is forwarded with no figure.
    #[test]
    fn an_absent_cost_renders_unreported() {
        let text = rendered(&usage(false, None));
        assert!(text.contains("unreported"), "{text:?}");
        assert!(!text.contains('0'), "{text:?}");
    }

    /// The marker on the wire is what reaches the line — the view never
    /// decides whose number it is.
    #[test]
    fn the_wire_provenance_is_the_one_rendered() {
        let ours = rendered(&usage(
            true,
            Some(crate::presentation::cost::COST_PROVENANCE_ROUTER),
        ));
        assert!(ours.contains("0.42"), "{ours:?}");
        assert!(!ours.contains("agent"), "{ours:?}");

        let theirs = rendered(&usage(true, None));
        assert!(theirs.contains("agent"), "{theirs:?}");
        assert!(theirs.contains("0.42"), "{theirs:?}");

        let unknown = rendered(&usage(true, Some("something-new")));
        assert!(unknown.contains("unreported"), "{unknown:?}");
    }
}
