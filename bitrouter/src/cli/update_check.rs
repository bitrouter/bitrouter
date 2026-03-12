use std::time::Duration;

const GITHUB_RELEASES_URL: &str =
    "https://api.github.com/repos/bitrouter/bitrouter/releases/latest";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Fetch the latest release tag from GitHub and return an update notice
/// if a newer version is available. Returns `None` on any error or if
/// the local version is already up-to-date.
pub async fn check_for_update() -> Option<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .ok()?;

    let resp = client
        .get(GITHUB_RELEASES_URL)
        .header("User-Agent", format!("bitrouter/{CURRENT_VERSION}"))
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .ok()?;

    if !resp.status().is_success() {
        return None;
    }

    let body = resp.text().await.ok()?;

    #[derive(serde::Deserialize)]
    struct Release {
        tag_name: String,
    }

    let release: Release = serde_json::from_str(&body).ok()?;
    let latest = release
        .tag_name
        .strip_prefix('v')
        .unwrap_or(&release.tag_name);

    if is_newer(latest, CURRENT_VERSION) {
        Some(format!(
            "\n  A new version of bitrouter is available: v{latest} (current: v{CURRENT_VERSION})\n  \
             https://github.com/bitrouter/bitrouter/releases/tag/{}\n",
            release.tag_name,
        ))
    } else {
        None
    }
}

/// Returns `true` if `latest` is strictly newer than `current` (semver comparison).
fn is_newer(latest: &str, current: &str) -> bool {
    let parse = |v: &str| -> Option<(u64, u64, u64)> {
        let mut parts = v.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next()?.parse().ok()?;
        Some((major, minor, patch))
    };

    match (parse(latest), parse(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_version_detected() {
        assert!(is_newer("1.0.0", "0.9.0"));
        assert!(is_newer("0.6.0", "0.5.0"));
        assert!(is_newer("0.5.1", "0.5.0"));
    }

    #[test]
    fn same_version_not_newer() {
        assert!(!is_newer("0.5.0", "0.5.0"));
    }

    #[test]
    fn older_version_not_newer() {
        assert!(!is_newer("0.4.0", "0.5.0"));
        assert!(!is_newer("0.5.0", "0.5.1"));
    }

    #[test]
    fn invalid_version_not_newer() {
        assert!(!is_newer("abc", "0.5.0"));
        assert!(!is_newer("0.5.0", "abc"));
        assert!(!is_newer("", ""));
    }
}
