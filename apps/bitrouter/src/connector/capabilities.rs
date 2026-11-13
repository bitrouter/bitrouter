//! Honest adapter capabilities; observation confers no execution authority.

use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct Capabilities {
    pub adapter: &'static str,
    pub metrics: [&'static str; 3],
    pub inference_dispatch: bool,
    pub capacity_estimation: bool,
    pub admission_control: bool,
    pub budget_actuation: bool,
    pub durable_execution: bool,
    pub pool_discovery: bool,
    pub router_binding_verification: bool,
}

pub fn capabilities() -> Capabilities {
    Capabilities {
        adapter: "vllm_prometheus_v1",
        metrics: [
            "vllm:num_requests_running",
            "vllm:num_requests_waiting",
            "vllm:kv_cache_usage_perc",
        ],
        inference_dispatch: false,
        capacity_estimation: false,
        admission_control: false,
        budget_actuation: false,
        durable_execution: false,
        pool_discovery: false,
        router_binding_verification: false,
    }
}
