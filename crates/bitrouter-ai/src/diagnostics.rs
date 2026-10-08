//! Credential and caller-supplied identifier filtering for trusted diagnostics.

use crate::error::ModelError;

/// Filters exact sensitive values from model/transport diagnostics.
///
/// This is diagnostic filtering, not continuation or replay authorization.
#[derive(Default)]
pub struct DiagnosticRedactor {
    replacements: Vec<(String, String)>,
}

fn is_sensitive_credential_name(name: &str) -> bool {
    let normalized = name.to_ascii_lowercase().replace('-', "_");
    matches!(
        normalized.as_str(),
        "authorization" | "proxy_authorization" | "cookie" | "set_cookie"
    ) || normalized.split('_').any(|segment| {
        matches!(
            segment,
            "auth" | "key" | "token" | "credential" | "secret" | "signature" | "sig"
        )
    })
}

impl DiagnosticRedactor {
    /// Capture opaque history continuity before a provider can echo it in errors.
    pub fn capture_prompt_continuity(&mut self, prompt: &crate::types::Prompt) {
        for message in &prompt.messages {
            self.capture_content_continuity(&message.content);
        }
    }

    /// Capture opaque output continuity before diagnostic/telemetry serialization.
    pub fn capture_content_continuity(&mut self, content: &[crate::types::Content]) {
        for content in content {
            let metadata = match content {
                crate::types::Content::ToolCall {
                    provider_metadata, ..
                }
                | crate::types::Content::Reasoning {
                    provider_metadata, ..
                } => provider_metadata,
                _ => continue,
            };
            for namespace in metadata.values().filter_map(serde_json::Value::as_object) {
                for (key, value) in namespace {
                    if matches!(
                        key.as_str(),
                        "thoughtSignature" | "replayProof" | "signature"
                    ) && let Some(value) = value.as_str()
                    {
                        self.add_replacement(value.to_owned(), "[redacted continuity]".into());
                    }
                }
            }
        }
    }
    /// Capture credentials from the final request headers and URL.
    pub fn capture_request_credentials(
        &mut self,
        request: &reqwest::Request,
        effective_target_key: &str,
    ) {
        for name in request.headers().keys() {
            for value in request.headers().get_all(name) {
                let Ok(value) = value.to_str() else {
                    continue;
                };
                if !is_sensitive_credential_name(name.as_str())
                    && (effective_target_key.is_empty() || value != effective_target_key)
                {
                    continue;
                }
                self.add_replacement(value.to_owned(), "[redacted credential]".to_owned());
                if let Some((_, credential)) = value.split_once(' ')
                    && !credential.is_empty()
                {
                    self.add_replacement(credential.to_owned(), "[redacted credential]".to_owned());
                }
                if name.as_str().eq_ignore_ascii_case("cookie") {
                    for pair in value.split(';') {
                        if let Some((_, credential)) = pair.trim().split_once('=')
                            && !credential.is_empty()
                        {
                            self.add_replacement(
                                credential.to_owned(),
                                "[redacted credential]".to_owned(),
                            );
                        }
                    }
                }
            }
        }
        for (name, value) in request.url().query_pairs() {
            if is_sensitive_credential_name(&name)
                || (!effective_target_key.is_empty() && value == effective_target_key)
            {
                self.add_replacement(value.into_owned(), "[redacted credential]".to_owned());
            }
        }
        if let Some(raw_query) = request.url().query() {
            for raw_pair in raw_query.split('&') {
                let (_, raw_value) = raw_pair.split_once('=').unwrap_or((raw_pair, ""));
                let Some((decoded_name, decoded_value)) =
                    url::form_urlencoded::parse(raw_pair.as_bytes()).next()
                else {
                    continue;
                };
                if is_sensitive_credential_name(&decoded_name)
                    || (!effective_target_key.is_empty()
                        && decoded_value.as_ref() == effective_target_key)
                {
                    self.add_replacement(raw_value.to_owned(), "[redacted credential]".to_owned());
                }
            }
        }
    }

    /// Add one exact sensitive-value replacement, preferring longest matches.
    pub fn add_replacement(&mut self, sensitive: String, replacement: String) {
        if sensitive.is_empty()
            || self
                .replacements
                .iter()
                .any(|(existing, _)| existing == &sensitive)
        {
            return;
        }
        self.replacements.push((sensitive, replacement));
        self.replacements
            .sort_by_key(|(sensitive, _)| std::cmp::Reverse(sensitive.len()));
    }

    /// Filter exact values from a text diagnostic.
    pub fn scrub_text(&self, text: &str) -> String {
        self.replacements
            .iter()
            .fold(text.to_owned(), |scrubbed, (sensitive, replacement)| {
                scrubbed.replace(sensitive, replacement)
            })
    }

    /// Filter string values and object keys in structured diagnostics.
    pub fn scrub_value(&self, value: &mut serde_json::Value) {
        match value {
            serde_json::Value::String(text) => *text = self.scrub_text(text),
            serde_json::Value::Array(values) => {
                for value in values {
                    self.scrub_value(value);
                }
            }
            serde_json::Value::Object(object) => {
                let entries = std::mem::take(object);
                for (key, mut value) in entries {
                    if matches!(
                        key.as_str(),
                        "thought_signature"
                            | "thoughtSignature"
                            | "google_replay_proof"
                            | "replayProof"
                            | "signature"
                    ) {
                        value = serde_json::Value::String("[redacted continuity]".into());
                    } else {
                        self.scrub_value(&mut value);
                    }
                    object.insert(self.scrub_text(&key), value);
                }
            }
            serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
            }
        }
    }

    /// Filter a provider body while preserving JSON structure when possible.
    pub fn scrub_body(&self, body: &str) -> String {
        let Ok(mut value) = serde_json::from_str::<serde_json::Value>(body) else {
            return self.scrub_text(body);
        };
        self.scrub_value(&mut value);
        value.to_string()
    }

    /// Filter diagnostics without changing the failure class or provider status.
    pub fn scrub_error(&self, error: ModelError) -> ModelError {
        match error {
            ModelError::InvalidRequest { message } => {
                ModelError::invalid_request(self.scrub_text(&message))
            }
            ModelError::InvalidResponse { message, mut usage } => ModelError::InvalidResponse {
                message: self.scrub_text(&message),
                usage: {
                    if let Some(raw) = usage.as_mut().and_then(|usage| usage.raw.as_mut()) {
                        self.scrub_value(raw);
                    }
                    usage
                },
            },
            ModelError::ClassifierResponse { mut failure } => {
                failure.message = self.scrub_text(&failure.message);
                if let Some(usage) = &mut failure.usage
                    && let Some(raw) = &mut usage.raw
                {
                    self.scrub_value(raw);
                }
                ModelError::ClassifierResponse { failure }
            }
            ModelError::Provider { status, message } => ModelError::Provider {
                status,
                message: self.scrub_text(&message),
            },
            ModelError::PolicyViolation { message } => ModelError::PolicyViolation {
                message: self.scrub_text(&message),
            },
            ModelError::InvalidCredential { message } => {
                ModelError::invalid_credential(self.scrub_text(&message))
            }
            ModelError::Configuration { message } => ModelError::Configuration {
                message: self.scrub_text(&message),
            },
            ModelError::Transport { message } => ModelError::Transport {
                message: self.scrub_text(&message),
            },
            ModelError::Decode { message } => ModelError::Decode {
                message: self.scrub_text(&message),
            },
            ModelError::HttpResponse {
                status,
                body,
                retry_after,
            } => ModelError::HttpResponse {
                status,
                body: self.scrub_body(&body),
                retry_after,
            },
            error @ (ModelError::Incompatible { .. }
            | ModelError::Timeout
            | ModelError::Cancelled
            | ModelError::CredentialStorage { .. }) => error,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_sensitive_credential_name;
    #[test]
    fn credential_name_detection_covers_custom_auth_without_redacting_ordinary_fields() {
        for name in [
            "Authorization",
            "api-key",
            "X-Custom-Token",
            "X-Provider-Auth",
            "x-refresh-secret",
            "X-Amz-Signature",
            "cookie",
        ] {
            assert!(is_sensitive_credential_name(name), "missed {name}");
        }
        for name in ["content-type", "api-version", "model", "x-request-id"] {
            assert!(
                !is_sensitive_credential_name(name),
                "ordinary field was classified as a credential: {name}"
            );
        }
    }
}
