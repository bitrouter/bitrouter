---
type: changed
breaking: true
title: "`bitrouter_mcp::capabilities::routing` moves to a typed `actions::route` port"
pr: 869
---

`bitrouter_mcp::capabilities::routing` is gone. The routing port moved to
`bitrouter_mcp::actions::route` and is now typed:
`RoutingQuery::preview(RoutePreviewArgs) -> serde_json::Value` becomes
`RouteQuery::route(RouteInput) -> RouteReport`. `ServeOptions::routing` takes the
new trait object.
