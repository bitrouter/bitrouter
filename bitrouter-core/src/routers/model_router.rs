use crate::{
    errors::Result,
    models::{image::image_model::DynImageModel, language::language_model::DynLanguageModel},
    routers::routing_table::RoutingTarget,
};

/// A router that routes to the appropriate language model implementation based on the routing target.
pub trait LanguageModelRouter {
    /// Routes to the appropriate language model implementation based on the routing target.
    ///
    /// Returns a dynosaur-generated [`DynLanguageModel`] that can hold any
    /// concrete model type while keeping the dynamic-dispatch boilerplate in one place.
    fn route_model(
        &self,
        target: RoutingTarget,
    ) -> impl Future<Output = Result<Box<DynLanguageModel<'static>>>> + Send;
}

/// A router that routes to the appropriate image model implementation based on the routing target.
pub trait ImageModelRouter {
    /// Routes to the appropriate image model implementation based on the routing target.
    fn route_model(
        &self,
        target: RoutingTarget,
    ) -> impl Future<Output = Result<Box<DynImageModel<'static>>>> + Send;
}
