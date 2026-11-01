//! Bounded service response fields; no raw values escape validation.

use reqwest::header::HeaderMap;

use super::ErrorSource;

fn single<'a>(headers: &'a HeaderMap, name: &str) -> Result<Option<&'a str>, ()> {
    let mut values = headers.get_all(name).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(());
    }
    value.to_str().map(Some).map_err(|_| ())
}

pub(super) fn retry_after_seconds(headers: &HeaderMap) -> Result<Option<u64>, ()> {
    let Some(value) = single(headers, "retry-after")? else {
        return Ok(None);
    };
    // RFC 9110 permits delay-seconds or HTTP-date. Core's error formatter emits
    // only delay-seconds, including normalized provider dates. Reject unsupported
    // representations explicitly rather than silently discarding the hint.
    // https://www.rfc-editor.org/rfc/rfc9110.html#section-10.2.3
    if !(1..=20).contains(&value.len()) || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(());
    }
    value.parse().map(Some).map_err(|_| ())
}

pub(super) fn error_source(headers: &HeaderMap) -> Result<Option<ErrorSource>, ()> {
    match single(headers, "x-bitrouter-error-source")? {
        None => Ok(None),
        Some("upstream") => Ok(Some(ErrorSource::Upstream)),
        Some(_) => Err(()),
    }
}
