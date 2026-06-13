#[cfg(feature = "arc")]
pub mod chainlink;
pub mod receipt;

pub use receipt::AttestationReceipt;

#[cfg(feature = "arc")]
pub use chainlink::{AttestError, ChainlinkAttester, Resource};
