//! Service behavior suites share fixtures without importing other test suites.

mod close;
mod harness;
mod instructions;
mod lifecycle;
#[cfg(feature = "acp")]
mod native_acp;
mod observation;
mod ownership;
#[cfg(unix)]
mod process_recovery;
mod recovery;
mod startup;
mod steering;
mod support;
mod thread;
mod unification;
