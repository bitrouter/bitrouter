use super::*;
use anyhow::{Result, ensure};

fn signal(ordinal: usize) -> Signal {
    Signal {
        ordinal,
        outstanding_work: 0,
        capacity: 8,
        bookable: true,
        affinity_millionths: 0,
        latency: None,
    }
}

#[test]
fn frozen_policy_cold_warm_load_escape_and_unavailable_order() -> Result<()> {
    let mut signals = [signal(0), signal(1)];
    ensure!(rank(&signals, 4096, Policy::PrefixAffinity)? == [0, 1]);
    signals[1].affinity_millionths = 750_000;
    ensure!(rank(&signals, 4096, Policy::PrefixAffinity)? == [1, 0]);
    ensure!(rank(&signals, 4096, Policy::LoadAware)? == [0, 1]);
    signals[1].outstanding_work = 4096;
    ensure!(rank(&signals, 4096, Policy::PrefixAffinity)? == [0, 1]);
    signals[1].outstanding_work = 409;
    ensure!(rank(&signals, 4096, Policy::PrefixAffinity)? == [0, 1]); // discount cannot hide this much load
    signals[1].affinity_millionths = 1_000_000;
    ensure!(rank(&signals, 4096, Policy::PrefixAffinity)? == [1, 0]);
    signals[1].outstanding_work = 410;
    ensure!(rank(&signals, 4096, Policy::PrefixAffinity)? == [0, 1]); // beyond hard 110% band
    signals[0].bookable = false;
    ensure!(rank(&signals, 4096, Policy::PrefixAffinity)? == [1, 0]);
    Ok(())
}

#[test]
fn latency_only_breaks_exact_ties_with_comparable_fresh_samples() -> Result<()> {
    let mut signals = [signal(0), signal(1), signal(2)];
    signals[0].latency = Some(Latency {
        samples: 3,
        mean_millis: 50,
        age_millis: 0,
    });
    signals[1].latency = Some(Latency {
        samples: 3,
        mean_millis: 10,
        age_millis: 299_999,
    });
    ensure!(rank(&signals, 4096, Policy::LoadAware)?[0] == 0); // third unknown
    signals[2].latency = Some(Latency {
        samples: 3,
        mean_millis: 20,
        age_millis: 0,
    });
    ensure!(rank(&signals, 4096, Policy::LoadAware)? == [1, 2, 0]);
    signals[1]
        .latency
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("latency"))?
        .age_millis = 300_000;
    ensure!(rank(&signals, 4096, Policy::LoadAware)?[0] == 0);
    signals[1].latency = Some(Latency {
        samples: 2,
        mean_millis: 0,
        age_millis: 0,
    });
    ensure!(rank(&signals, 4096, Policy::LoadAware)?[0] == 0);
    signals[0].outstanding_work = 1;
    ensure!(rank(&signals, 4096, Policy::LoadAware)?[0] == 1); // no latency preference across unequal scores
    Ok(())
}

#[test]
fn ranking_validates_bounds_and_uses_checked_wide_products() -> Result<()> {
    let mut signals = [signal(4095), signal(4)];
    signals[0].outstanding_work = u64::MAX;
    signals[1].outstanding_work = u64::MAX;
    signals[0].capacity = u32::MAX;
    signals[1].capacity = u32::MAX;
    ensure!(rank(&signals, 131072, Policy::LoadAware)? == [4, 4095]);
    signals[1].ordinal = 4095;
    ensure!(rank(&signals, 4096, Policy::LoadAware).is_err());
    signals[1].ordinal = 4;
    signals[1].capacity = 0;
    ensure!(rank(&signals, 4096, Policy::LoadAware).is_err());
    signals[1].capacity = 1;
    signals[1].affinity_millionths = 1_000_001;
    ensure!(rank(&signals, 4096, Policy::PrefixAffinity).is_err());
    ensure!(rank(&[], 4096, Policy::LoadAware).is_err());
    ensure!(rank(&[signal(0)], 0, Policy::LoadAware).is_err());
    Ok(())
}
