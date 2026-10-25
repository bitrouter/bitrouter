use super::*;
use anyhow::{Result, ensure};

fn headers() -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    for (name, value) in [
        ("bitrouter-tenancy-version", "1"),
        ("bitrouter-installation-id", "installation-a"),
        ("bitrouter-instance-id", "instance-a"),
        ("bitrouter-capability-revision", "cap-a"),
        ("bitrouter-execution-revision", "rev-a"),
        ("bitrouter-correlation-id", "correlation-a"),
        ("bitrouter-receipt-mode", "strict"),
    ] {
        headers.insert(http::HeaderName::from_static(name), value.parse()?);
    }
    Ok(headers)
}

#[test]
fn dispatch_headers_require_one_exact_complete_binding() -> Result<()> {
    let baseline = headers()?;
    let binding = dispatch_binding(&baseline).map_err(|f| anyhow::anyhow!("{:?}", f.0.code))?;
    ensure!(binding.execution_revision.as_str() == "rev-a");
    for name in baseline.keys() {
        let mut missing = baseline.clone();
        missing.remove(name);
        ensure!(dispatch_binding(&missing).is_err());
        let mut duplicate = baseline.clone();
        duplicate.append(name.clone(), baseline[name].clone());
        ensure!(dispatch_binding(&duplicate).is_err());
        let mut ambiguous = baseline.clone();
        ambiguous.insert(name.clone(), "value,another".parse()?);
        ensure!(dispatch_binding(&ambiguous).is_err());
    }
    for (name, value) in [
        ("bitrouter-tenancy-version", "01"),
        ("bitrouter-receipt-mode", "ordinary"),
        ("bitrouter-execution-revision", " rev-a"),
        ("bitrouter-correlation-id", ""),
    ] {
        let mut changed = baseline.clone();
        changed.insert(name, value.parse()?);
        ensure!(dispatch_binding(&changed).is_err());
    }
    Ok(())
}
