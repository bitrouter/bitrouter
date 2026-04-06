# BitRouter Development Guidelines

## Documents

See `README.md` and `DEVELOPMENT.md` for full project introduction and architecture.

## Guidelines

1. **NEVER** use `#[allow(xxx)]` to bypass checks.
2. **NEVER** re-export components in a public mod. If you already have a public mod: `pub mod a;`, you never re-export components inside it: `pub use a::A; // Don't do this`.
3. **NEVER** use `.unwrap`, `.expect` or `panic!` to make the Rust program panic.
4. **NEVER** over-design types, functions and methods that is never used in the feature or fix you are working on. We don't allow dead code.

## Contributing

1. **ALWAYS** use the **conventional** git commit message format. Keep the title under 60 characters. The message body and footer can be any length.
2. **ALWAYS** use the format of **conventional** git commit message's header part for your PR title. We validate this.

## Run Checks Before Submitting Code

Run these checks before submitting to users if you modified source code:

1. `cargo nextest run --all-features` or `cargo test --all-features` if `cargo-nextest` is absent: Ensure all unit tests, integration tests and doc tests pass.
2. `cargo clippy --all-features`: Ensure you are following Rust's best practices. Direct auto-fix, if applicable: run `cargo clippy --all-features --fix` at a clean git workspace.
3. `cargo fmt -- --check`: Ensure the source code is correctly formatted. Direct auto-fix: run `cargo fmt`
