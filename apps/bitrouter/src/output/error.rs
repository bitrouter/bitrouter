//! The uniform error envelope as a [`CliReport`], plus normalizers from the
//! CLI's error sources into it.
//!
//! Every failed command renders [`ErrorEnvelope`] through the same [`Output`](super::Output)
//! driver as a success: JSON `{"error": {"kind", "message", …}}` on stdout by
//! default, or the human `error:` / `while:` / `hint:` block under `--human`.

use bitrouter_sdk::error::{BitrouterError, ErrorBody, ErrorEnvelope, ErrorKind};

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::{Format, Output};

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
