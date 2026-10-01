//! Managed orchestration: core owns scheduling; the authenticated harness owns
//! workspace execution and the durable checkpoint authority. Neither local
//! embedding nor an HTTP adapter may bypass the checkpoint acknowledgement.

pub mod checkpoint;
pub mod protocol;
pub mod session;
