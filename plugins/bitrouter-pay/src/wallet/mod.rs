#[cfg(feature = "mpp")]
pub mod local_signer;
#[cfg(feature = "arc")]
pub mod ows_signer;

#[cfg(feature = "mpp")]
pub use local_signer::ArcLocalSigner;
#[cfg(feature = "arc")]
pub use ows_signer::ArcSigner;
