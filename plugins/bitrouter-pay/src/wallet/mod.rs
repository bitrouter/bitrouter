#[cfg(feature = "arc")]
pub mod ows_signer;

#[cfg(feature = "arc")]
pub use ows_signer::ArcSigner;
