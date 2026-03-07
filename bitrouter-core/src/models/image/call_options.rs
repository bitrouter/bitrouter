use http::HeaderMap;
use tokio_util::sync::CancellationToken;

use crate::models::{image::file::ImageModelFile, shared::provider::ProviderOptions};

/// The options for calling an image generation model.
#[derive(Debug, Clone)]
pub struct ImageModelCallOptions {
    /// An optional prompt to guide the image generation, if supported by the model.
    pub prompt: Option<String>,
    /// The number of images to generate.
    pub n: u32,
    /// The desired size of the generated images.
    pub size: Option<ImageModelCallOptionsSize>,
    /// The desired aspect ratio of the generated images, if supported by the model.
    pub aspect_ratio: Option<ImageModelCallOptionsAspectRatio>,
    /// An optional seed for the random number generator, if supported by the model.
    pub seed: Option<u64>,
    /// Optional files to use as input for the image generation, if supported by the model.
    pub files: Option<Vec<ImageModelFile>>,
    /// An optional file to use as a mask for inpainting, if supported by the model.
    pub mask: Option<ImageModelFile>,
    /// Optional provider-specific options for the image generation model.
    pub provider_options: Option<ProviderOptions>,
    /// An optional signal to abort the image generation request.
    pub abort_signal: Option<CancellationToken>,
    /// Optional HTTP headers to include in the image generation request.
    pub headers: Option<HeaderMap>,
}

/// The size of the generated image, specified as width and height in pixels.
#[derive(Debug, Clone)]
pub struct ImageModelCallOptionsSize {
    pub width: u32,
    pub height: u32,
}

/// The aspect ratio of the generated image, specified as width and height ratios.
#[derive(Debug, Clone)]
pub struct ImageModelCallOptionsAspectRatio {
    pub width: u32,
    pub height: u32,
}
