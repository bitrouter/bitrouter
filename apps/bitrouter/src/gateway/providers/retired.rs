//! Bounded migration guidance for removed built-in model integrations.

/// Identify retired built-in providers before credentials or network activity.
pub fn provider_message(name: &str) -> Option<&'static str> {
    match name {
        "google-ai" => Some(
            "google-ai subscription inference is retired; configure a supported provider explicitly. Antigravity SDK clients can use the OpenAI-compatible gateway; saved credentials are preserved",
        ),
        "vertex" => Some(
            "the Vertex Express integration is retired; configure a verified supported provider explicitly. No automatic credential or billing migration is performed",
        ),
        _ => None,
    }
}

/// Retired names remain readable as historical provenance, never callable wires.
pub fn protocol_message(name: &str) -> Option<&'static str> {
    match name {
        "generate_content" | "google" | "antigravity" => Some(
            "native Gemini Generate Content and Antigravity protocols are retired; use Chat Completions with a verified OpenAI-compatible endpoint",
        ),
        _ => None,
    }
}
