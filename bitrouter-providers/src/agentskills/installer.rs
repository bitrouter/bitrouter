//! Remote skill fetcher — HTTP GET from a registry or URL.

/// Fetch a `SKILL.md` file from a remote URL.
///
/// Returns the raw file content on success. The caller is responsible for
/// writing it to disk and parsing the frontmatter.
pub(crate) async fn fetch_skill(client: &reqwest::Client, url: &str) -> Result<String, String> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("failed to fetch skill from {url}: {e}"))?;

    let status = response.status();
    if !status.is_success() {
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "<unreadable body>".to_string());
        return Err(format!("HTTP {status} fetching {url}: {body}"));
    }

    response
        .text()
        .await
        .map_err(|e| format!("failed to read response body from {url}: {e}"))
}
