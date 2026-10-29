//! The uniform error envelope as a [`CliReport`], plus normalizers from the
//! CLI's error sources into it.
//!
//! Every failed command renders [`ErrorEnvelope`] through the same [`Output`](super::Output)
//! driver as a success: JSON `{"error": {"kind", "message", …}}` on stdout by
//! default, or the human `error:` / `while:` / `hint:` block under `--human`.

use crate::gateway::error::BitrouterError;
use crate::gateway::error::ErrorBody;
use crate::gateway::error::ErrorEnvelope;
use crate::gateway::error::ErrorKind;

use crate::error_report::{hint_for, strip_status_prefix};
use crate::output::CliReport;
use crate::output::human::Human;

impl CliReport for ErrorEnvelope {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        h.error_head(&self.error.message)?;
        for layer in &self.error.context {
            h.while_line(layer)?;
        }
        if let Some(hint) = &self.error.hint {
            h.blank()?;
            for line in hint.lines() {
                h.hint_line(line)?;
            }
        }
        Ok(())
    }

    fn exit_code(&self) -> i32 {
        1
    }
}

/// Normalize an `anyhow` error chain into the canonical envelope: root cause →
/// `message` (HTTP status-prefix stripped), the [`BitrouterError`] kind when one
/// is present in the chain (else [`ErrorKind::Internal`]), the outer chain
/// layers → `context` (outermost first), and a recognised remediation `hint`.
pub fn envelope_from_anyhow(err: &anyhow::Error) -> ErrorEnvelope {
    // Operator uncertainty is the result classification, not an incidental
    // transport cause. Do not flatten it into the deepest arbitrary source.
    if let Some(envelope) = err.chain().find_map(|error| {
        error
            .downcast_ref::<bitrouter_sdk::error::Error>()
            .and_then(operator_envelope)
    }) {
        return envelope;
    }
    let chain: Vec<String> = err.chain().map(|e| e.to_string()).collect();
    let root_raw = chain
        .last()
        .map(String::as_str)
        .unwrap_or("(unknown error)");
    let root = strip_status_prefix(root_raw);

    let kind = err
        .chain()
        .find_map(|e| e.downcast_ref::<BitrouterError>())
        .map(BitrouterError::kind)
        .unwrap_or(ErrorKind::Internal);

    let context = if chain.len() > 1 {
        chain[..chain.len() - 1]
            .iter()
            .map(|layer| strip_status_prefix(layer).to_string())
            .collect()
    } else {
        Vec::new()
    };

    ErrorEnvelope {
        error: ErrorBody {
            kind,
            message: root.to_string(),
            context,
            hint: hint_for(root),
        },
    }
}

fn operator_envelope(error: &bitrouter_sdk::error::Error) -> Option<ErrorEnvelope> {
    use bitrouter_ai::gateway::operator::ErrorCode;
    use bitrouter_sdk::error::Error;

    let (kind, message, context, hint) = match error {
        Error::OperatorUnknown { operation_id, .. } => (
            ErrorKind::Upstream,
            "operator mutation outcome is unknown".into(),
            vec![format!("operation_id: {operation_id}")],
            Some(format!(
                "Inspect `bro shared operation --endpoint <same endpoint> --token-env <same variable> --operation-id={operation_id}`. Keep the original mutation; do not retry with a new operation ID."
            )),
        ),
        Error::OperatorRejected(code) => {
            let kind = match code {
                ErrorCode::InvalidInput | ErrorCode::Conflict => ErrorKind::BadRequest,
                ErrorCode::Unauthorized => ErrorKind::Unauthorized,
                ErrorCode::Forbidden => ErrorKind::Forbidden,
                ErrorCode::NotFound => ErrorKind::NotFound,
                ErrorCode::Unavailable | ErrorCode::Unknown => ErrorKind::UpstreamUnavailable,
            };
            (
                kind,
                format!("operator request rejected ({code:?})"),
                Vec::new(),
                None,
            )
        }
        _ => return None,
    };
    Some(ErrorEnvelope {
        error: ErrorBody {
            kind,
            message,
            context,
            hint,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::{Format, Output};

    #[test]
    fn operator_uncertainty_remains_prominent_without_its_source_chain() -> anyhow::Result<()> {
        let error = anyhow::Error::new(bitrouter_sdk::error::Error::OperatorUnknown {
            operation_id: "..".into(),
            source: Box::new(bitrouter_sdk::error::Error::Response(
                "PRIVATE_SOURCE_SENTINEL",
            )),
        })
        .context("PRIVATE_OUTER_SENTINEL");
        let envelope = envelope_from_anyhow(&error);
        assert_eq!(
            envelope.error.message,
            "operator mutation outcome is unknown"
        );
        assert_eq!(envelope.error.context, ["operation_id: .."]);
        for format in [Format::Json, Format::Human] {
            let text = String::from_utf8(Output::new(format).render_to_vec(&envelope))?;
            assert!(text.contains("shared operation") && text.contains("--operation-id=.."));
            assert!(text.contains("do not retry with a new operation ID"));
            assert!(!text.contains("PRIVATE_"));
        }
        let rejection = envelope_from_anyhow(&anyhow::Error::new(
            bitrouter_sdk::error::Error::OperatorRejected(
                bitrouter_ai::gateway::operator::ErrorCode::Forbidden,
            ),
        ));
        assert!(matches!(rejection.error.kind, ErrorKind::Forbidden));
        assert!(!rejection.error.message.contains("mutation outcome"));
        assert!(rejection.error.hint.is_none());
        Ok(())
    }

    #[test]
    fn anyhow_chain_becomes_message_context_hint() {
        let err = anyhow::anyhow!("config references undefined environment variable 'FOO'")
            .context("loading /tmp/bitrouter.yaml");
        let env = envelope_from_anyhow(&err);
        assert_eq!(
            env.error.message,
            "config references undefined environment variable 'FOO'"
        );
        assert_eq!(
            env.error.context,
            vec!["loading /tmp/bitrouter.yaml".to_string()]
        );
        assert!(env.error.hint.as_deref().unwrap().contains("export FOO"));
        assert_eq!(env.exit_code(), 1);
    }

    #[test]
    fn error_envelope_json_shape() {
        let env = envelope_from_anyhow(&anyhow::anyhow!("boom"));
        let buf = Output::new(Format::Json).render_to_vec(&env);
        let v: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(v["error"]["kind"], "internal");
        assert_eq!(v["error"]["message"], "boom");
    }

    #[test]
    fn error_envelope_human_block() {
        let env = envelope_from_anyhow(&anyhow::anyhow!("boom").context("doing x"));
        let buf = Output::new(Format::Human).render_to_vec(&env);
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            "error: boom\n  while: doing x\n"
        );
    }
}
