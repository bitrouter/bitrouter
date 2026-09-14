//! ACP metadata translated into plain, temporary Code pickers.

use agent_client_protocol::schema::v1::{
    SessionConfigKind, SessionConfigOption, SessionConfigOptionCategory, SessionConfigOptionValue,
    SessionConfigSelectOptions, SessionModeState,
};
use bitrouter_tui::code::{Selector, SelectorRow};

/// Revalidate a choice against the latest reported setting before mutation.
pub(crate) fn setting_value(
    option: &SessionConfigOption,
    id: &str,
) -> anyhow::Result<SessionConfigOptionValue> {
    match &option.kind {
        SessionConfigKind::Boolean(_) => {
            Ok(SessionConfigOptionValue::Boolean { value: id.parse()? })
        }
        SessionConfigKind::Select(select) => {
            let available = match &select.options {
                SessionConfigSelectOptions::Ungrouped(options) => {
                    options.iter().any(|option| option.value.to_string() == id)
                }
                SessionConfigSelectOptions::Grouped(groups) => groups.iter().any(|group| {
                    group
                        .options
                        .iter()
                        .any(|option| option.value.to_string() == id)
                }),
                _ => false,
            };
            anyhow::ensure!(available, "This setting choice is no longer advertised");
            Ok(SessionConfigOptionValue::ValueId {
                value: id.to_string().into(),
            })
        }
        _ => anyhow::bail!("This setting type is not supported"),
    }
}

pub(crate) fn row(
    id: impl Into<String>,
    label: impl Into<String>,
    detail: impl Into<String>,
) -> SelectorRow {
    SelectorRow {
        id: id.into(),
        label: label.into(),
        detail: detail.into(),
        unavailable: None,
    }
}

pub(crate) fn picker(
    id: impl Into<String>,
    title: impl Into<String>,
    rows: Vec<SelectorRow>,
) -> Selector {
    Selector {
        id: id.into(),
        title: title.into(),
        detail: String::new(),
        rows,
        allow_custom: false,
        custom_label: String::new(),
    }
}

pub(crate) fn settings(
    config: &[SessionConfigOption],
    modes: Option<&SessionModeState>,
) -> Vec<Selector> {
    let mut children = Vec::new();
    let mut root = Vec::new();
    for option in config {
        let (current, rows) = match &option.kind {
            SessionConfigKind::Select(select) => {
                let rows = match &select.options {
                    SessionConfigSelectOptions::Ungrouped(options) => options
                        .iter()
                        .map(|value| {
                            row(
                                value.value.to_string(),
                                &value.name,
                                value.description.clone().unwrap_or_default(),
                            )
                        })
                        .collect(),
                    SessionConfigSelectOptions::Grouped(groups) => groups
                        .iter()
                        .flat_map(|group| {
                            group.options.iter().map(|value| {
                                row(
                                    value.value.to_string(),
                                    &value.name,
                                    format!(
                                        "{} · {}",
                                        group.name,
                                        value.description.as_deref().unwrap_or("")
                                    ),
                                )
                            })
                        })
                        .collect(),
                    _ => Vec::new(),
                };
                (select.current_value.to_string(), rows)
            }
            SessionConfigKind::Boolean(value) => (
                value.current_value.to_string(),
                vec![row("true", "On", ""), row("false", "Off", "")],
            ),
            _ => {
                let mut unavailable = row(
                    option.id.to_string(),
                    &option.name,
                    "Unsupported setting type",
                );
                unavailable.unavailable =
                    Some("This setting type is not supported by this client".to_string());
                root.push(unavailable);
                continue;
            }
        };
        let id = format!("config:{}", option.id);
        let category = match option.category.as_ref() {
            Some(SessionConfigOptionCategory::Model) => "Model · ",
            Some(SessionConfigOptionCategory::ModelConfig) => "Model settings · ",
            Some(SessionConfigOptionCategory::ThoughtLevel) => "Reasoning · ",
            Some(SessionConfigOptionCategory::Mode) => "Mode · ",
            _ => "",
        };
        // Categories help users find related controls; the agent's labels,
        // identities, and ordering remain the source of every actual choice.
        root.push(row(
            &id,
            &option.name,
            format!("{category}Current: {current}"),
        ));
        let mut child = picker(id, format!("Agent setting · {}", option.name), rows);
        child.detail = option.description.clone().unwrap_or_default();
        children.push(child);
    }
    if let Some(modes) = modes {
        let rows = modes
            .available_modes
            .iter()
            .map(|mode| {
                row(
                    mode.id.to_string(),
                    &mode.name,
                    mode.description.clone().unwrap_or_default(),
                )
            })
            .collect();
        children.push(picker("mode", "Agent mode", rows));
        root.push(row(
            "mode",
            "Mode",
            format!("Current: {}", modes.current_mode_id),
        ));
    }
    let mut main = picker("settings", "Agent settings", root);
    main.detail = if main.rows.is_empty() {
        "This agent has not reported editable settings".into()
    } else {
        "These settings belong to the agent. Session routing is a separate control.".into()
    };
    children.push(main);
    children
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{
        SessionConfigBoolean, SessionConfigSelect, SessionConfigSelectOption,
    };

    #[test]
    fn setting_choices_preserve_agent_ids_and_support_boolean_options() {
        let config = vec![
            SessionConfigOption::new(
                "unusual:id",
                "Agent's model",
                SessionConfigKind::Select(SessionConfigSelect::new(
                    "model/value",
                    vec![SessionConfigSelectOption::new("model/value", "A model")],
                )),
            ),
            SessionConfigOption::new(
                "enabled",
                "Feature",
                SessionConfigKind::Boolean(SessionConfigBoolean::new(true)),
            ),
        ];
        let selectors = settings(&config, None);
        assert_eq!(selectors[0].id, "config:unusual:id");
        assert_eq!(selectors[0].rows[0].id, "model/value");
        assert_eq!(
            selectors[1]
                .rows
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            vec!["true", "false"]
        );
        assert_eq!(selectors[2].rows.len(), 2);
    }

    #[test]
    fn settings_removed_during_selection_are_not_sent() {
        let option = SessionConfigOption::new(
            "model",
            "Model",
            SessionConfigKind::Select(SessionConfigSelect::new(
                "current",
                vec![SessionConfigSelectOption::new("current", "Current model")],
            )),
        );
        assert!(setting_value(&option, "removed").is_err());
        assert!(setting_value(&option, "current").is_ok());
        let toggle = SessionConfigOption::new(
            "feature",
            "Feature",
            SessionConfigKind::Boolean(SessionConfigBoolean::new(false)),
        );
        assert!(setting_value(&toggle, "yes").is_err());
    }
}
