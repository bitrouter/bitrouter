# BitRouter Development Guidelines

## Project Overview

bitrouter is a modular, trait-based LLM routing system written in Rust. It can be used as:

- **A lightweight local LLM aggregator and proxy** — connect to upstream providers (OpenAI, Anthropic, Google) and expose provider-specific API types, running on your local machine.
- **A high-performance, out-of-the-box web server on the cloud** — deploy the binary for production LLM request proxying with config-driven routing, daemon management, and observability.
- **An SDK to build your own service** — import trait-based core components and API routes as library crates. Write your own implementation at any layer, or re-use service components to plug-and-play.

---

## Guidelines

1. **NEVER** use `#[allow(xxx)]` to bypass checks.
2. **NEVER** re-export components in a public mod. If you already have a public mod: `pub mod a;`, you never re-export components inside it: `pub use a::A; // Don't do this`.
3. **NEVER** use `.unwrap`, `.expect` or `panic!` to make the Rust program panic.
4. **NEVER** over-design types, functions and methods that is never used in the feature or fix you are working on. We don't allow dead code.

---

## Contributing

1. **ALWAYS** use the **conventional** git commit message format. Keep the title under 60 characters. The message body and footer can be any length.
2. **ALWAYS** use the format of **conventional** git commit message's header part for your PR title. We validate this.

---

## Run Checks Before Submitting Code

Run these checks before submitting to users if you modified source code:

1. `cargo test --workspace`: Ensure all unit tests, integration tests and doc tests pass.
2. `cargo clippy`: Ensure you are following Rust's best practise.
3. `cargo fmt -- --check`: Ensure the source code is correctly formatted.
