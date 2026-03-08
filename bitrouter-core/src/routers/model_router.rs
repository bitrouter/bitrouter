use crate::{
    errors::Result,
    models::{image::image_model::BoxImageModel, language::language_model::BoxLanguageModel},
    routers::routing_table::RoutingTarget,
};

/// A router that routes to the appropriate language model implementation based on the routing target.
pub trait LanguageModelRouter {
    /// Routes to the appropriate language model implementation based on the routing target.
    ///
    /// Returns a [`BoxLanguageModel`] which is Send + Sync safe and can hold any
    /// concrete model type, enabling multi-provider routing from a single method.
    fn route_model(
        &self,
        target: RoutingTarget,
    ) -> impl Future<Output = Result<BoxLanguageModel>> + Send;
}

/// A router that routes to the appropriate image model implementation based on the routing target.
pub trait ImageModelRouter {
    /// Routes to the appropriate image model implementation based on the routing target.
    fn route_model(
        &self,
        target: RoutingTarget,
    ) -> impl Future<Output = Result<BoxImageModel>> + Send;
}
