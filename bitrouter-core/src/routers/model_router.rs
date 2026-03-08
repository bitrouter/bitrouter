use crate::{
    errors::Result,
    models::{image::image_model::ImageModel, language::language_model::LanguageModel},
    routers::routing_table::RoutingTarget,
};

/// A router that routes to the appropriate language model implementation based on the routing target.
pub trait LanguageModelRouter {
    /// Routes to the appropriate language model implementation based on the routing target.
    fn route_model(
        &self,
        target: RoutingTarget,
    ) -> impl Future<Output = Result<impl LanguageModel>>;
}

/// A router that routes to the appropriate image model implementation based on the routing target.
pub trait ImageModelRouter {
    /// Routes to the appropriate image model implementation based on the routing target.
    fn route_model(&self, target: RoutingTarget) -> impl Future<Output = Result<impl ImageModel>>;
}
