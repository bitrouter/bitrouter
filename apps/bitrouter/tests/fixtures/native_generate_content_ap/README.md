# Native ordered AP SDK process fixture

This standalone Cargo package enables `serde_json/arbitrary_precision` and
`raw_value` for an SDK consumer in a separate process. The mounted Core test
process keeps its supported inline Number layout and its existing retained-cache
allocation guard. Both processes use the same repository AI and SDK source.
The fixture reads a typed request from standard input and sends only to the
loopback service supplied by the test; it does not discover, retry, or use
external services.

From the repository root, build the fixture separately from the normal Core
test feature graph:

```sh
cargo build --locked --manifest-path apps/bitrouter/tests/fixtures/native_generate_content_ap/Cargo.toml --target-dir target/native-ap-sdk-consumer
BITROUTER_NATIVE_AP_CLIENT="$PWD/target/native-ap-sdk-consumer/debug/native-ap-sdk-consumer" cargo test --locked -p bitrouter --lib --no-default-features --features http2,otel-http -- --ignored --exact tenancy::tests::execution::receipts::ordered::generate_content_native::boundaries::process_consumer::native_generate_content_actual_ap_sdk_consumer_keeps_original_tokens_json_sse_h1_h2
```

The test checks actual JSON and SSE over H1 and H2, raw private-number-shaped
objects, original request/default separation, exact paired body and identity,
full raw response arrays, usage receipts and clean-EOF terminal ownership. It
fails without an explicitly supplied Cargo-built consumer binary.
