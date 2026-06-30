//! Compile-time dark palette loaded from `assets/theme.json`.

use serde::Deserialize;

/// Catppuccin-Mocha-inspired dark palette embedded at compile time.
///
/// Each field is a hex colour string such as `"#1e1e2e"`. The embedded JSON
/// is validated at test time; all fields are mandatory.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Theme {
    /// Window / root background.
    pub bg: String,
    /// Slightly elevated surface (panels, cards).
    pub surface: String,
    /// Dividers and strokes.
    pub border: String,
    /// Primary text.
    pub text: String,
    /// Secondary / de-emphasised text.
    pub dim: String,
    /// Interactive accent (links, focus ring, active item).
    pub accent: String,
    /// Success colour.
    pub ok: String,
    /// Warning colour.
    pub warn: String,
    /// Error colour.
    pub err: String,
}

impl Theme {
    /// Parse the palette from the JSON embedded at compile time.
    ///
    /// Returns an error if the JSON is malformed or any required key is absent.
    pub fn load() -> anyhow::Result<Self> {
        let raw = include_str!("../../../assets/theme.json");
        let theme: Self = serde_json::from_str(raw)?;
        Ok(theme)
    }
}

#[cfg(test)]
mod tests {
    use super::Theme;

    #[test]
    fn load_parses_known_field() -> anyhow::Result<()> {
        let theme = Theme::load()?;
        assert_eq!(theme.bg, "#1e1e2e");
        assert_eq!(theme.accent, "#89b4fa");
        Ok(())
    }
}
