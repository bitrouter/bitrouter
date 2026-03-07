use crate::models::shared::provider::ProviderMetadata;

/// Represents a file used in an image model call.
#[derive(Debug, Clone)]
pub enum ImageModelFile {
    /// A file represented by its media type and data.
    File {
        media_type: String,
        data: ImageModelFileData,
        provider_metadata: Option<ProviderMetadata>,
    },
    /// A file represented by its URL.
    Url {
        url: String,
        provider_metadata: Option<ProviderMetadata>,
    },
}

/// Represents the data of a file, which can be either a data URL or raw bytes.
#[derive(Debug, Clone)]
pub enum ImageModelFileData {
    /// A file represented as a data URL.
    DataUrl(String),
    /// A file represented as raw bytes.
    Bytes(Vec<u8>),
}
