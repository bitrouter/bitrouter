# TypeSafe evaluation provider extension

`provider/` contains the reviewed, native Rust TypeSafe integration. It
registers the `typesafe` provider's executable Jev model and owns its System
One JSON request/response mapping. The common BitRouter host owns account
selection, bearer authentication, HTTP transport, retries, cancellation, and
metering. Default `bro` links and registers this extension; it does not create
a route unless a TypeSafe account is active.

This is a compile-time Rust extension, not a dynamically installed plug-in or
security boundary. The public endpoint is BitRouter's `/v1/evaluate`; upstream
TypeSafe calls use `/v1/systemone`.
