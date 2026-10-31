//! The public persistent facade is the producer; SQL below only observes its facts.
use super::*;
use crate::core::optimization::cohort::CohortGateVerdict;
use crate::tenancy::store::StoredResponse;
use bitrouter_ai::{
    core::protocol::ModelMode,
    eval::{EvalSnapshot, ExperimentArm},
    tenancy::configuration::LearningMode,
};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};
mod authority;
mod capacity;
mod consumer;
mod fixture;
mod human;
mod learner_input;
mod refusal;
mod tools;
use fixture::*;

#[cfg(unix)]
mod learner;

#[cfg(unix)]
mod paid;
