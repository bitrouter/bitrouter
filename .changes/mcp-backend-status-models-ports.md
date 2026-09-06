---
type: changed
breaking: true
title: "`bitrouter_mcp::backend` swaps its status/models types for query ports"
pr: 869
---

`bitrouter_mcp::backend::{ModelInfo, StatusInfo, ProviderStatus}` and
`Backend::{list_models, status}` are gone. `Backend` gains `status_port` /
`models_port` (`Option<Arc<dyn StatusQuery>>` / `Option<Arc<dyn ModelsQuery>>`),
`Builder::completion_local` is replaced by `completion(Arc<LocalBackend>)` +
`models(...)`, and `ServeOptions` gains `status` and `models`.

`bitrouter_sdk::language_model::routing::ModelInfo` is the element type of the
shared report and now derives `JsonSchema`, `PartialEq` and `Eq`.
