use std::sync::Arc;

use crate::{
    errors::Result,
    models::language::language_model::DynLanguageModel,
    routers::{router::LanguageModelRouter, routing_table::RoutingTarget},
};

use super::{GenerationHook, HookedModel};

/// A [`LanguageModelRouter`] wrapper that attaches [`GenerationHook`]s to
/// every model returned by the inner router.
///
/// When the hooks slice is empty the wrapper is a zero-cost pass-through —
/// it returns the inner model unchanged.
pub struct HookedRouter<R> {
    inner: R,
    hooks: Arc<[Arc<dyn GenerationHook>]>,
}

impl<R> HookedRouter<R> {
    /// Wrap an existing router with generation hooks.
    ///
    /// If `hooks` is empty, models are returned unwrapped.
    pub fn new(inner: R, hooks: Arc<[Arc<dyn GenerationHook>]>) -> Self {
        Self { inner, hooks }
    }
}

impl<R> LanguageModelRouter for HookedRouter<R>
where
    R: std::ops::Deref + Send + Sync,
    R::Target: LanguageModelRouter + Send + Sync,
{
    async fn route_model(&self, target: RoutingTarget) -> Result<Box<DynLanguageModel<'static>>> {
        let model = self.inner.route_model(target).await?;

        if self.hooks.is_empty() {
            return Ok(model);
        }

        Ok(DynLanguageModel::new_box(HookedModel::new(
            model,
            self.hooks.clone(),
        )))
    }
}
