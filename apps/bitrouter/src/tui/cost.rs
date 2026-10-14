//! Terminal display of shared cost values and their provenance.
use crate::presentation::cost::{Cost, Provenance};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

impl Cost {
    /// The cost as it appears in the live area.
    pub fn render(&self) -> Line<'static> {
        let figure = format!("{} {:.4}", self.currency(), self.amount());
        match self.provenance() {
            Provenance::Router => Line::from(Span::styled(
                format!("{figure} (router)"),
                Style::default().add_modifier(Modifier::DIM),
            )),
            // The qualifier is part of the figure, not a footnote: whatever
            // truncates this line must lose the number before the caveat.
            Provenance::Harness => Line::from(vec![
                Span::styled(
                    "agent-reported ",
                    Style::default().add_modifier(Modifier::DIM),
                ),
                Span::styled(figure, Style::default().add_modifier(Modifier::DIM)),
            ]),
        }
    }
}

/// What to draw when no cost was reported at all.
///
/// A separate function rather than a [`Provenance`] variant, so there is no
/// way to render an absent figure as a number. Most ACP agents are not
/// routers and will never send `cost`; that is not a zero.
pub fn unreported() -> Line<'static> {
    Line::from(Span::styled(
        "cost unreported",
        Style::default().add_modifier(Modifier::DIM),
    ))
}

#[cfg(test)]
mod tests {
    use agent_client_protocol_schema::v1::Cost as WireCost;

    use super::*;
    use crate::presentation::cost::{COST_PROVENANCE_META_KEY, COST_PROVENANCE_ROUTER, from_usage};
    use agent_client_protocol_schema::v1::UsageUpdate;

    fn text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
    }

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
        let ours = text(&Cost::new(0.42, "USD", Provenance::Router).render());
        assert!(ours.contains("0.42"), "{ours:?}");
        assert!(!ours.contains("agent"), "{ours:?}");

        let theirs = text(&Cost::new(1.32, "USD", Provenance::Harness).render());
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
        let rendered = text(&Cost::new(1.32, "USD", Provenance::Harness).render());
        let caveat = rendered.find("agent").expect("caveat present");
        let figure = rendered.find("1.32").expect("figure present");
        assert!(caveat < figure, "{rendered:?}");
    }

    /// No cost reported is not a cost of zero.
    #[test]
    fn an_absent_cost_is_unreported_not_zero() {
        let rendered = text(&unreported());
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
        ))
        .expect("a metered cost");
        assert_eq!(read, Cost::new(0.42, "USD", Provenance::Router));
    }

    /// No marker means the harness wrote the number itself. It is shown, and
    /// shown as the agent's — never as though BitRouter had measured it.
    #[test]
    fn an_unmarked_cost_is_the_harness_own() {
        let read = from_usage(&usage_with(Some(WireCost::new(9.99, "EUR")), None))
            .expect("the harness's figure");
        assert_eq!(read, Cost::new(9.99, "EUR", Provenance::Harness));
        assert!(text(&read.render()).contains("agent"));
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
        crate::tui::plain::text(
            &crate::presentation::cost::from_usage(usage)
                .map_or_else(crate::tui::cost::unreported, |c| c.render()),
        )
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
