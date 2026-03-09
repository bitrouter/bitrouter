# bitrouter-openai

GitHub repository: [bitrouter/bitrouter](https://github.com/bitrouter/bitrouter)

OpenAI adapter crate for BitRouter.

This crate contains the OpenAI-facing request, response, and provider logic
used to serve Chat Completions and Responses-compatible APIs. It translates
between BitRouter core model types and OpenAI's HTTP surface.

## Includes

- Chat Completions support in `chat`
- Responses API support in `responses`
- Provider configuration and model implementations built on `reqwest`
