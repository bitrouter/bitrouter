//! Terminal rendering of harness-owned permission decisions.

use crate::harness::permission::Prompt;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

impl Prompt {
    /// The prompt as it appears in the live area.
    ///
    /// One line: the terminal rows here are borrowed from the user's shell,
    /// and a permission question does not need a box drawn around it.
    pub fn render(&self) -> Line<'static> {
        let mut spans = vec![
            Span::styled(
                "allow? ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(self.title().to_owned()),
            Span::raw("  "),
        ];
        for (index, option) in self.options().iter().enumerate() {
            spans.push(Span::styled(
                format!("[{}]", index + 1),
                Style::default().add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(format!("{} ", option.name)));
        }
        spans.push(Span::styled(
            "[esc] deny",
            Style::default().fg(Color::DarkGray),
        ));
        Line::from(spans)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol_schema::v1::{
        PermissionOption, PermissionOptionId, PermissionOptionKind, ToolKind,
    };
    fn request(options: Vec<PermissionOption>) -> Prompt {
        Prompt::new(
            "r1",
            Some("Write src/main.rs".to_string()),
            "t1",
            Some(ToolKind::Edit),
            options,
        )
    }

    fn option(id: &str, name: &str, kind: PermissionOptionKind) -> PermissionOption {
        PermissionOption::new(PermissionOptionId::new(id), name, kind)
    }

    fn offered() -> Vec<PermissionOption> {
        vec![
            option("allow", "allow once", PermissionOptionKind::AllowOnce),
            option("always", "always allow", PermissionOptionKind::AllowAlways),
            option("no", "reject", PermissionOptionKind::RejectOnce),
        ]
    }

    fn text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
    }

    /// The prompt must say what is being permitted and how to answer.
    #[test]
    fn a_prompt_names_the_action_and_its_options() {
        let prompt = request(offered());
        let rendered = text(&prompt.render());
        assert!(
            rendered.contains("Write src/main.rs"),
            "the user must know what they are allowing: {rendered:?}"
        );
        for (index, option) in offered().iter().enumerate() {
            assert!(
                rendered.contains(&format!("[{}]", index + 1)),
                "{rendered:?}"
            );
            assert!(rendered.contains(&option.name), "{rendered:?}");
        }
    }
}
