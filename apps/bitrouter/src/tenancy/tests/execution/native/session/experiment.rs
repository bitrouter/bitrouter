//! The public persistent facade is the producer; SQL below only observes its facts.
use super::*;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};
mod fixture;
use fixture::*;

#[cfg(unix)]
mod paid;
