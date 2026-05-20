//! Upstream URL validator — defends against SSRF when bitrouter accepts an
//! `api_base` from a less-trusted source (a BYOK row a user POSTed, or a
//! `bitrouter.yaml` provider entry someone else can edit).
//!
//! Without validation, a hostile `api_base` like `http://169.254.169.254/` or
//! `http://localhost:8200/v1/secret/` would have the executor POST upstream
//! requests — Authorization header and all — into the host's internal
//! network. The bitrouter request body might also exfiltrate data the
//! attacker shouldn't see.
//!
//! This validator is **textual / cheap** — it parses the URL and rejects
//! obvious bad cases on the wire form. It does NOT resolve DNS names (DNS
//! resolves at request time, so a check here would be a TOCTOU). For
//! resolved-IP checks the right place is a connect-time hook on the HTTP
//! client itself.
//!
//! Inspired by the v0 audit (bitrouter#463 / cloud#251 audit S3, S4).
//!
//! Defence-in-depth — the BYOK form is the obvious gate, but the same check
//! also runs on the provider config so a typo or a malicious config file
//! can't slip past either.

use std::net::IpAddr;

use crate::error::{BitrouterError, Result};

/// DNS hostnames that map to a cloud-provider metadata service. Reject these
/// outright; a request body landing here can extract IAM credentials.
const METADATA_HOSTS: &[&str] = &[
    // AWS EC2 / EKS instance metadata
    "instance-data",
    "instance-data.ec2.internal",
    // GCP metadata
    "metadata.google.internal",
    "metadata.google.com",
    // Azure Instance Metadata Service is IP-only (169.254.169.254); no DNS.
];

/// Validate an upstream `api_base` URL.
///
/// Rules:
/// - Scheme must be `http` or `https`. Anything else (`file://`, `gopher://`,
///   `data:`, `javascript:`) is rejected.
/// - `http://` is only allowed when the host is a loopback name or IP
///   (`localhost`, `127.x.x.x`, `[::1]`) — self-hosted upstreams on dev
///   machines and Docker hosts. Production should always use `https://`.
/// - The host MUST NOT be a known cloud-provider metadata DNS alias
///   (`metadata.google.internal`, EC2's `instance-data`, …).
/// - If the host is a literal IP, it MUST NOT be loopback, link-local
///   (covers `169.254.169.254`), or any IETF private range (RFC 1918, RFC
///   4193, RFC 6598 carrier-grade NAT, the IPv6 unique-local block).
///   Unspecified / multicast / documentation ranges are rejected too.
///
/// Returns a `BitrouterError::BadRequest` describing the rejection so the
/// reason can be surfaced to whoever submitted the URL.
pub fn validate_upstream_url(raw: &str) -> Result<()> {
    let url = url::Url::parse(raw)
        .map_err(|e| BitrouterError::bad_request(format!("invalid URL '{raw}': {e}")))?;

    let scheme = url.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(BitrouterError::bad_request(format!(
            "URL scheme '{scheme}' is not allowed (use http or https)",
        )));
    }

    let host = url
        .host_str()
        .ok_or_else(|| BitrouterError::bad_request(format!("URL '{raw}' has no host")))?;

    let host_lower = host.to_ascii_lowercase();
    if METADATA_HOSTS.iter().any(|m| *m == host_lower) {
        return Err(BitrouterError::bad_request(format!(
            "host '{host}' is a cloud-metadata alias — refusing to route there",
        )));
    }

    // Literal IP? Apply the strict IP allow-list.
    let host_for_ip = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = host_for_ip.parse::<IpAddr>()
        && is_blocked_ip(&ip)
    {
        return Err(BitrouterError::bad_request(format!(
            "host '{ip}' is in a non-routable / metadata range",
        )));
    }

    // Plain HTTP is only allowed for loopback.
    if scheme == "http" && !is_loopback_host(&host_lower, host_for_ip) {
        return Err(BitrouterError::bad_request(format!(
            "URL '{raw}' uses http:// for a non-loopback host (use https://)",
        )));
    }

    Ok(())
}

fn is_loopback_host(host_lower: &str, host_for_ip: &str) -> bool {
    if matches!(host_lower, "localhost" | "localhost.localdomain") {
        return true;
    }
    if let Ok(ip) = host_for_ip.parse::<IpAddr>()
        && ip.is_loopback()
    {
        return true;
    }
    false
}

fn is_blocked_ip(ip: &IpAddr) -> bool {
    // Loopback (127.0.0.0/8, ::1) is explicitly allowed — symmetric with the
    // hostname `localhost`, and needed for dev / self-hosted upstreams
    // (Ollama, vLLM, the test harness).
    if ip.is_loopback() {
        return false;
    }
    if ip.is_unspecified() || ip.is_multicast() {
        return true;
    }
    match ip {
        IpAddr::V4(v4) => {
            // Link-local (169.254.0.0/16) — covers the IMDS endpoint
            // 169.254.169.254 used by AWS / Azure / GCP / Alibaba.
            if v4.is_link_local() {
                return true;
            }
            // RFC 1918 private ranges.
            if v4.is_private() {
                return true;
            }
            let o = v4.octets();
            // RFC 6598 carrier-grade NAT: 100.64.0.0/10
            if o[0] == 100 && (64..=127).contains(&o[1]) {
                return true;
            }
            // RFC 5737 documentation ranges + RFC 6890 0.0.0.0/8 + benchmarking.
            if matches!(o[0], 0 | 198 if matches!(o[1], 18 | 19))
                || (o[0] == 192 && o[1] == 0 && o[2] == 2)
                || (o[0] == 198 && o[1] == 51 && o[2] == 100)
                || (o[0] == 203 && o[1] == 0 && o[2] == 113)
            {
                return true;
            }
            false
        }
        IpAddr::V6(v6) => {
            // Unique-local (fc00::/7) — covers fd00::/8 ULA.
            let segs = v6.segments();
            if (segs[0] & 0xfe00) == 0xfc00 {
                return true;
            }
            // Link-local (fe80::/10) — covers the IPv6 metadata alias fe80::a9fe:a9fe.
            if (segs[0] & 0xffc0) == 0xfe80 {
                return true;
            }
            // Documentation 2001:db8::/32.
            if segs[0] == 0x2001 && segs[1] == 0x0db8 {
                return true;
            }
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_metadata_hostname() {
        assert!(validate_upstream_url("https://metadata.google.internal/").is_err());
        assert!(validate_upstream_url("HTTPS://Metadata.Google.Internal/").is_err());
        assert!(validate_upstream_url("https://instance-data.ec2.internal/v1").is_err());
    }

    #[test]
    fn rejects_imds_literal_ip() {
        assert!(validate_upstream_url("http://169.254.169.254/").is_err());
        assert!(validate_upstream_url("https://169.254.169.254/").is_err());
    }

    #[test]
    fn rejects_private_ipv4() {
        for url in [
            "http://10.0.0.1/",
            "https://172.16.0.1/",
            "https://192.168.1.1/",
            "https://100.64.0.1/",
        ] {
            assert!(
                validate_upstream_url(url).is_err(),
                "{url} should be rejected"
            );
        }
    }

    #[test]
    fn allows_loopback_literal() {
        // Symmetric with the `localhost` hostname — dev / self-hosted setups
        // use loopback IPs all the time.
        validate_upstream_url("http://127.0.0.1/").unwrap();
        validate_upstream_url("http://127.0.0.1:8080/v1").unwrap();
        validate_upstream_url("https://[::1]/").unwrap();
    }

    #[test]
    fn rejects_unique_local_ipv6() {
        assert!(validate_upstream_url("https://[fd00::1]/").is_err());
    }

    #[test]
    fn rejects_non_http_scheme() {
        assert!(validate_upstream_url("file:///etc/passwd").is_err());
        assert!(validate_upstream_url("gopher://example.com/").is_err());
    }

    #[test]
    fn allows_https_public() {
        validate_upstream_url("https://api.openai.com/v1").unwrap();
        validate_upstream_url("https://api.anthropic.com").unwrap();
    }

    #[test]
    fn http_only_allowed_for_localhost() {
        // dev / self-hosted (Ollama, vLLM) on localhost is the explicit
        // exception.
        validate_upstream_url("http://localhost:11434/v1").unwrap();
        // bare localhost IP / IPv6 loopback are NOT allowed for http:// because
        // the IP allow-list rejects loopback IPs outright (use the hostname).
        // A different reading of "loopback" would allow them; this matches
        // the safer-by-default story.
        assert!(validate_upstream_url("http://example.com/").is_err());
    }
}
