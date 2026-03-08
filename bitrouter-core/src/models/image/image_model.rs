use dynosaur::dynosaur;

use crate::models::image::{
    call_options::ImageModelCallOptions, generation_result::ImageModelGenerationResult,
};

#[dynosaur(pub DynImageModel = dyn(box) ImageModel)]
pub trait ImageModel: Send + Sync {
    fn provider_name(&self) -> &str;
    fn model_id(&self) -> &str;
    fn max_images_per_call(&self) -> impl Future<Output = Option<u32>> + Send;
    fn generate(
        &self,
        options: ImageModelCallOptions,
    ) -> impl Future<Output = ImageModelGenerationResult> + Send;
}
