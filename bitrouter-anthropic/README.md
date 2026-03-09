# bitrouter-anthropic

GitHub repository: [bitrouter/bitrouter](https://github.com/bitrouter/bitrouter)

Anthropic Messages adapter crate for BitRouter.

This crate provides the Anthropic-specific request, response, and provider
implementation used by BitRouter's routing layer. It maps BitRouter's shared
model contracts onto the Anthropic Messages API.

## Includes

- Messages API support in `messages`
- Anthropic provider configuration and request execution
- Translation between BitRouter types and Anthropic payloads
