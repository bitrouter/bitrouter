use crate::models::image::{
    call_options::ImageModelCallOptions, generation_result::ImageModelGenerationResult,
};

pub trait ImageModel {
    fn provider_name(&self) -> &str;
    fn max_images_per_call(&self, model_id: &str) -> impl Future<Output = Option<u32>>;
    fn generate(
        &self,
        model_id: &str,
        options: ImageModelCallOptions,
    ) -> impl Future<Output = ImageModelGenerationResult>;
}
