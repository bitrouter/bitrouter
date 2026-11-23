#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]

pub mod attempt;
pub mod control;
pub mod error;
pub mod gateway;
pub mod operator;
pub mod stream;
pub mod tenancy;

mod transport;
