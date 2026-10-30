use super::*;
use anyhow::{Context, Result, ensure};

fn request() -> Result<reqwest::Request> {
    let mut request = reqwest::Request::new(
        reqwest::Method::POST,
        "https://example.invalid/v1/chat/completions".parse()?,
    );
    request
        .headers_mut()
        .insert("authorization", "Bearer owned-slot-secret".parse()?);
    request
        .headers_mut()
        .insert("content-type", "application/json".parse()?);
    *request.body_mut() = Some(r#"{"model":"retained-model","max_tokens":25}"#.into());
    Ok(request)
}

#[test]
fn authenticated_wire_binding_detects_changes_to_each_transmitted_component() -> Result<()> {
    let baseline = request()?;
    let expected = digest(&baseline).map_err(|code| anyhow::anyhow!("{code:?}"))?;
    for component in [
        "method",
        "origin",
        "path",
        "query",
        "auth",
        "account",
        "duplicate",
        "body",
    ] {
        let mut changed = baseline.try_clone().context("buffered request")?;
        match component {
            "method" => *changed.method_mut() = reqwest::Method::PUT,
            "origin" => changed.url_mut().set_host(Some("other.invalid"))?,
            "path" => changed.url_mut().set_path("/v1/other"),
            "query" => changed.url_mut().set_query(Some("account=other")),
            "auth" => {
                changed
                    .headers_mut()
                    .insert("authorization", "Bearer another-slot-secret".parse()?);
            }
            "account" => {
                changed
                    .headers_mut()
                    .insert("openai-organization", "another-org".parse()?);
            }
            "duplicate" => {
                changed
                    .headers_mut()
                    .append("authorization", "Bearer owned-slot-secret".parse()?);
            }
            "body" => {
                *changed.body_mut() = Some(r#"{"model":"another-model","max_tokens":25}"#.into())
            }
            _ => anyhow::bail!("unknown component"),
        }
        ensure!(
            digest(&changed).map_err(|code| anyhow::anyhow!("{code:?}"))? != expected,
            "{component}"
        );
    }
    // The final current-authority check can narrow a timeout without changing
    // the authenticated bytes or granting another credential/target authority.
    let mut bounded = baseline.try_clone().context("buffered request")?;
    *bounded.timeout_mut() = Some(std::time::Duration::from_secs(1));
    ensure!(digest(&bounded).map_err(|code| anyhow::anyhow!("{code:?}"))? == expected);
    Ok(())
}

#[test]
fn unbuffered_authenticated_bodies_cannot_receive_a_wire_binding() -> Result<()> {
    let mut request = request()?;
    *request.body_mut() = Some(reqwest::Body::wrap_stream(futures::stream::once(async {
        Ok::<_, std::io::Error>("opaque body")
    })));
    ensure!(matches!(digest(&request), Err(ErrorCode::InvalidRequest)));
    Ok(())
}
