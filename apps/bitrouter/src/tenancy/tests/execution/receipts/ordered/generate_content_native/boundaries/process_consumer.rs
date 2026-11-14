//! Feature-unified AP SDK in a separate process against the normal Core owner.
//! The Core retained-cache Number layout fence is intentionally unchanged.
use super::*;
use std::{path::PathBuf, process::Stdio};
use tokio::io::AsyncWriteExt;

#[tokio::test]
#[ignore = "requires BITROUTER_NATIVE_AP_CLIENT pointing to the Cargo-built AP SDK consumer fixture"]
async fn native_generate_content_actual_ap_sdk_consumer_keeps_original_tokens_json_sse_h1_h2()
-> Result<()> {
    ensure!(
        !std::mem::needs_drop::<serde_json::Number>(),
        "the Core owner must use its qualified inline Number layout"
    );
    let binary = PathBuf::from(
        std::env::var_os("BITROUTER_NATIVE_AP_CLIENT").context("AP consumer executable")?,
    );
    ensure!(
        binary.is_absolute() && binary.is_file(),
        "absolute AP consumer executable required"
    );
    for h2 in [
        false,
        #[cfg(feature = "http2")]
        true,
    ] {
        for stream in [false, true] {
            let harness = Harness::new(0, "responses", h2).await?;
            let opaque = json!([{"$serde_json::private::Number":"12.5"},
                {"$serde_json::private::Number":"opaque","extra":[1.25,null]},
                {"$serde_json::private::RawValue":"[1]"}]);
            let mut original = json!({"contents":[{"parts":[{"text":"signed","thoughtSignature":"original"},
                {"unknownPart":{"retained":true}}]}],"cachedContent":null});
            original["opaque"] = opaque.clone();
            let request: OrderedGenerateContentRequestV1 =
                serde_json::from_str(&envelope(&original.to_string(), stream))?;
            let layer = defaults(json!({"opaque_default":opaque,
                "__google_top_level__":{"cachedContent":"cachedContents/suppressed-by-source-null"}}))?;
            let binding = harness
                .ordered
                .binding(vec![1, 0], GenerationProtocol::GenerateContent)
                .await?
                .with_generation_defaults_envelope_v2(&layer)?;
            let control = harness.ordered.control(true, HttpVersion::Http1Only)?;
            let capabilities = control.ordered_generate_content_capabilities().await?;
            let content = json!({"role":"model","parts":[{"inlineData":{"mimeType":"image/png","data":"AA=="}}]});
            let (media, data) = if stream {
                let first = json!({"candidates":[{"index":0,"content":content}],"opaque":opaque});
                let terminal = json!({"candidates":[{"index":0,"finishReason":"STOP"},{"index":9,"finishReason":"STOP"}],"usageMetadata":usage()});
                (
                    "text/event-stream",
                    format!("data: {first}\n\ndata: {terminal}\n\n"),
                )
            } else {
                (
                    "application/json",
                    json!({"candidates":[{"index":0,"content":content,"finishReason":"STOP"},
                    {"index":9,"finishReason":"STOP"}],"usageMetadata":usage(),"opaque":opaque})
                    .to_string(),
                )
            };
            *harness
                .peer
                .reply_data
                .lock()
                .map_err(|_| anyhow::anyhow!("reply"))? = Some((media.into(), data));
            let input = serde_json::to_vec(&json!({"endpoint":harness.ordered.endpoint,
                "token":harness.ordered.token()?,"binding":binding,"capabilities":capabilities,
                "request":request,"defaults":layer,"h2":h2}))?;
            let mut child = tokio::process::Command::new(&binary)
                .env_clear()
                .env("NO_PROXY", "127.0.0.1,localhost,::1")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()?;
            let output = tokio::time::timeout(Duration::from_secs(30), async move {
                let mut stdin = child.stdin.take().context("child stdin")?;
                stdin.write_all(&input).await?;
                drop(stdin);
                Ok::<_, anyhow::Error>(child.wait_with_output().await?)
            })
            .await??;
            ensure!(
                output.status.success(),
                "AP consumer failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let result = crate::gateway::model::protocol::generate_content::native::value(
                std::str::from_utf8(&output.stdout)?,
            )?;
            ensure!(result["ap_enabled"] == true && result["stream_terminal"] == stream);
            ensure!(result["frames"][0]["opaque"] == opaque);
            if !stream {
                ensure!(
                    result["frames"][0]["candidates"]
                        .as_array()
                        .context("full candidates")?
                        .len()
                        == 2
                );
            }
            let id: ExecutionId = result["execution_id"]
                .as_str()
                .context("id")?
                .to_owned()
                .try_into()?;
            ensure!(harness.ordered.observed()? == ["model-0"]);
            let receipt = harness.ordered.receipt(&id).await?;
            ensure!(
                receipt.outcome == Outcome::Completed
                    && receipt.attempts.len() == 1
                    && receipt.attempts[0].candidate_index == 0
            );
            {
                let records = harness
                    .peer
                    .records
                    .lock()
                    .map_err(|_| anyhow::anyhow!("records"))?;
                ensure!(
                    records.len() == 1 && records[0].0.rank == 1 && records[0].0.execution_id == id
                );
                ensure!(records[0].1["request"] == original);
                ensure!(records[0].1[ENVELOPE_BODY_MEMBER] == serde_json::to_value(&layer)?);
                ensure!(
                    records[0].2
                        == if h2 {
                            Version::HTTP_2
                        } else {
                            Version::HTTP_11
                        }
                );
            }
            harness.ordered.fixture.service.shutdown().await?;
        }
    }
    Ok(())
}
