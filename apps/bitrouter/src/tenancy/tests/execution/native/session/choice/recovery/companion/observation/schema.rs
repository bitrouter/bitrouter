//! Exact SDK keys for the two readable receipt checkpoints in this campaign.
use anyhow::{Context, Result, ensure};
use std::collections::{BTreeMap, BTreeSet};

pub(super) fn check(points: &BTreeMap<String, u64>) -> Result<()> {
    let mut expected = BTreeSet::new();
    for checkpoint in ["initial", "current"] {
        for field in [
            "completion_reserved",
            "physical_write_reserved",
            "accounted",
            "history_bytes",
            "oauth_reserved",
            "ingress_reserved",
            "potential_unknown",
            "live_unknown",
            "history_rows",
            "active",
            "queued",
            "ingress",
            "closed",
            "fenced",
            "available",
            "volume_available",
        ] {
            expected.insert(format!("{checkpoint}:{field}"));
        }
        ensure!(points.get(&format!("{checkpoint}:available")) == Some(&1));
        match points.get(&format!("{checkpoint}:volume_available")) {
            Some(1) => {
                expected.insert(format!("{checkpoint}:observed_free"));
            }
            Some(0) => {}
            _ => anyhow::bail!("invalid volume availability"),
        }
    }
    for (available, fields) in [
        (
            "execution_cache_available",
            &["execution_cache_entries", "execution_cache_bytes"][..],
        ),
        (
            "credential_cache_available",
            &["credential_cache_entries", "credential_cache_bytes"][..],
        ),
        (
            "routing_available",
            &[
                "routing_target_entries",
                "routing_provider_entries",
                "routing_active",
                "routing_bytes",
            ][..],
        ),
    ] {
        let key = format!("current:{available}");
        let value = points
            .get(&key)
            .context("missing working-set availability")?;
        expected.insert(key);
        match value {
            1 => {
                expected.extend(fields.iter().map(|field| format!("current:{field}")));
            }
            0 => {}
            _ => anyhow::bail!("invalid working-set availability"),
        }
    }
    // capacity_points has already rejected duplicate keys. Exact equality also
    // forbids missing fields, invented zeroes and immutable working-set values.
    ensure!(points.keys().cloned().collect::<BTreeSet<_>>() == expected);
    Ok(())
}
