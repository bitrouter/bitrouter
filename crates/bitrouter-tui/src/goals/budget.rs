//! Explicit numeric limits. The app maps these to the authoritative contract.
use super::{Effect, State};
use crate::editor::Editor;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Limits {
    pub elapsed_ms: Option<u64>,
    pub managed_token_estimates_micro_usd: Option<u64>,
}

pub(super) struct Dialog {
    pub goal: Option<(String, Limits)>,
    pub fields: [Editor; 2],
    pub selected: usize,
}

impl Dialog {
    pub(super) fn new(goal: Option<String>, limits: Limits) -> Self {
        let mut fields = [Editor::default(), Editor::default()];
        fields[0].set_text(decimal(limits.elapsed_ms, 3));
        fields[1].set_text(decimal(limits.managed_token_estimates_micro_usd, 6));
        Self {
            goal: goal.map(|id| (id, limits)),
            fields,
            selected: 0,
        }
    }
}

pub(super) fn decimal(value: Option<u64>, precision: u32) -> String {
    let Some(value) = value else {
        return "none".into();
    };
    let scale = 10_u64.pow(precision);
    format!(
        "{}.{:0width$}",
        value / scale,
        value % scale,
        width = precision as usize
    )
}

fn parse(text: &str, precision: u32) -> Option<Option<u64>> {
    let text = text.trim();
    if text.eq_ignore_ascii_case("none") {
        return Some(None);
    }
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    if whole.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || fraction.len() > precision as usize
        || !fraction.bytes().all(|b| b.is_ascii_digit())
        || text.ends_with('.')
    {
        return None;
    }
    let amount = whole
        .parse::<u64>()
        .ok()?
        .checked_mul(10_u64.pow(precision))?;
    let fraction = if fraction.is_empty() {
        0
    } else {
        fraction
            .parse::<u64>()
            .ok()?
            .checked_mul(10_u64.pow(precision - fraction.len() as u32))?
    };
    Some(Some(amount.checked_add(fraction)?))
}

impl State {
    pub(super) fn budget_event(&mut self, key: KeyEvent) -> Vec<Effect> {
        if key.code == KeyCode::Esc {
            self.budget_dialog = None;
            return Vec::new();
        }
        let Some(dialog) = &mut self.budget_dialog else {
            return Vec::new();
        };
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            dialog.selected = 1 - dialog.selected;
        } else if key.code == KeyCode::Enter && key.modifiers.is_empty() {
            let Some(elapsed_ms) = parse(dialog.fields[0].text(), 3) else {
                self.notice = Some("Elapsed limit requires nonnegative seconds with at most 3 decimal places, or none".into());
                return Vec::new();
            };
            let Some(managed_token_estimates_micro_usd) = parse(dialog.fields[1].text(), 6) else {
                self.notice = Some("Token estimate limit requires nonnegative USD with at most 6 decimal places, or none".into());
                return Vec::new();
            };
            let limits = Limits {
                elapsed_ms,
                managed_token_estimates_micro_usd,
            };
            let effect = dialog
                .goal
                .as_ref()
                .map(|(id, original)| Effect::SetBudget {
                    goal_id: id.clone(),
                    original: original.clone(),
                    limits: limits.clone(),
                });
            if effect.is_none() {
                self.new_budget = limits;
            }
            self.budget_dialog = None;
            self.notice = Some(
                if effect.is_some() {
                    "Budget update requested; cumulative use retained. Resume is a separate action."
                } else {
                    "Initial limits saved in this draft; they will be committed with Goal creation."
                }
                .into(),
            );
            return effect.into_iter().collect();
        } else if !key.modifiers.contains(KeyModifiers::CONTROL) {
            // This single-line numeric form never forwards Enter to a composer.
            dialog.fields[dialog.selected].apply(key);
        }
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn amounts_are_exact_and_ambiguous_or_overflowing_numbers_are_rejected() {
        assert_eq!(parse("1.000001", 6), Some(Some(1_000_001)));
        assert_eq!(parse("0.001", 3), Some(Some(1)));
        assert_eq!(parse("NONE", 6), Some(None));
        for text in [
            "",
            "-1",
            "+1",
            "1e3",
            ".5",
            "1.",
            "1.0000001",
            "18446744073710",
        ] {
            assert_eq!(parse(text, 6), None, "{text}");
        }
        let value = 18_446_744_073_709_551_615;
        assert_eq!(parse(&decimal(Some(value), 6), 6), Some(Some(value)));
    }
}
