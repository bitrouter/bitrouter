use dynosaur::dynosaur;

use crate::models::image::{
    call_options::ImageModelCallOptions, generation_result::ImageModelGenerationResult,
};

#[dynosaur(pub DynImageModel = dyn(box) ImageModel)]
pub trait ImageModel {
    fn provider_name(&self) -> &str;
    fn model_id(&self) -> &str;
    fn max_images_per_call(&self) -> impl Future<Output = Option<u32>> + Send;
    fn generate(
        &self,
        options: ImageModelCallOptions,
    ) -> impl Future<Output = ImageModelGenerationResult> + Send;
}

// ── Send-safe boxed wrapper ─────────────────────────────────────────────────

use std::pin::Pin;

/// Object-safe helper trait with Send + Sync bounds for dynamic dispatch.
trait ErasedImageModel: Send + Sync {
    fn provider_name(&self) -> &str;
    fn model_id(&self) -> &str;
    fn max_images_per_call_boxed(&self) -> Pin<Box<dyn Future<Output = Option<u32>> + Send + '_>>;
    fn generate_boxed(
        &self,
        options: ImageModelCallOptions,
    ) -> Pin<Box<dyn Future<Output = ImageModelGenerationResult> + Send + '_>>;
}

impl<T: ImageModel + Send + Sync> ErasedImageModel for T {
    fn provider_name(&self) -> &str {
        ImageModel::provider_name(self)
    }
    fn model_id(&self) -> &str {
        ImageModel::model_id(self)
    }
    fn max_images_per_call_boxed(&self) -> Pin<Box<dyn Future<Output = Option<u32>> + Send + '_>> {
        Box::pin(self.max_images_per_call())
    }
    fn generate_boxed(
        &self,
        options: ImageModelCallOptions,
    ) -> Pin<Box<dyn Future<Output = ImageModelGenerationResult> + Send + '_>> {
        Box::pin(self.generate(options))
    }
}

/// A boxed, Send + Sync wrapper around any [`ImageModel`] implementation.
pub struct BoxImageModel {
    inner: Box<dyn ErasedImageModel>,
}

// SAFETY: BoxImageModel wraps a `dyn ErasedImageModel` which requires Send + Sync.
unsafe impl Send for BoxImageModel {}
unsafe impl Sync for BoxImageModel {}

impl BoxImageModel {
    /// Creates a new `BoxImageModel` from any concrete model that implements
    /// [`ImageModel`] + `Send` + `Sync`.
    pub fn new<T: ImageModel + Send + Sync + 'static>(model: T) -> Self {
        Self {
            inner: Box::new(model),
        }
    }
}

impl ImageModel for BoxImageModel {
    fn provider_name(&self) -> &str {
        self.inner.provider_name()
    }

    fn model_id(&self) -> &str {
        self.inner.model_id()
    }

    fn max_images_per_call(&self) -> impl Future<Output = Option<u32>> + Send {
        self.inner.max_images_per_call_boxed()
    }

    fn generate(
        &self,
        options: ImageModelCallOptions,
    ) -> impl Future<Output = ImageModelGenerationResult> + Send {
        self.inner.generate_boxed(options)
    }
}
