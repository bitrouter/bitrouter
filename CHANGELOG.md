# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.14.0](https://github.com/bitrouter/bitrouter/compare/v0.13.0...v0.14.0)


### ⛰️ Features

- *(solana-mpp)* Make session challenge asset configurable ([#173](https://github.com/bitrouter/bitrouter/pull/173)) - ([a9426ce](https://github.com/bitrouter/bitrouter/commit/a9426ceb10f09c4cf5a645a071e8a033e2a97286))

### 🐛 Bug Fixes

- *(api)* Fixed solana mpp api types ([#170](https://github.com/bitrouter/bitrouter/pull/170)) - ([aba8e6a](https://github.com/bitrouter/bitrouter/commit/aba8e6ac0f4ff7ece1558f7ad21b584315db5b16))
- *(models)* Return configured routing models instead of built-in provider catalogs ([#177](https://github.com/bitrouter/bitrouter/pull/177)) - ([2b415ea](https://github.com/bitrouter/bitrouter/commit/2b415ea22c4ce9f27a440d839f8d614b9a1619ed))
- *(solana-mpp)* Pass through open action to process request ([#175](https://github.com/bitrouter/bitrouter/pull/175)) - ([1fef327](https://github.com/bitrouter/bitrouter/commit/1fef3275d3259376479e2ee9e7697254f1d0e379))

### 🚜 Refactor

- *(deps)* Replace serde-yaml with serde-saphyr ([#167](https://github.com/bitrouter/bitrouter/pull/167)) - ([d62b7bd](https://github.com/bitrouter/bitrouter/commit/d62b7bd079c2107f7a19a99298c35da1a61bfeb7))


## [0.12.0](https://github.com/bitrouter/bitrouter/compare/v0.11.0...v0.12.0)


### ⛰️ Features

- *(api)* Support MPP `session` intend on Solana ([#144](https://github.com/bitrouter/bitrouter/pull/144)) - ([b398a4f](https://github.com/bitrouter/bitrouter/commit/b398a4f835ba3d30151f809bd30c6a7231bec175))
- Bitrouter default provider onboarding ([#135](https://github.com/bitrouter/bitrouter/pull/135)) - ([7269264](https://github.com/bitrouter/bitrouter/commit/7269264afdfda54bd77f58b608c3608392e886a9))


## [0.11.0](https://github.com/bitrouter/bitrouter/compare/v0.10.0...v0.11.0)


### ⛰️ Features

- *(cli)* Unify `bitrouter init` with cloud and BYOK onboarding ([#130](https://github.com/bitrouter/bitrouter/pull/130)) - ([2ecdbfb](https://github.com/bitrouter/bitrouter/commit/2ecdbfb1032dd1580fe53fb7f832fd79c9a8e51a))
- Sanitize account system ([#133](https://github.com/bitrouter/bitrouter/pull/133)) - ([a20e4f8](https://github.com/bitrouter/bitrouter/commit/a20e4f87aecc4eb7c65beea63a1cbaee7275b1d6))

### 🐛 Bug Fixes

- *(api)* Handle tool call in api protocols ([#141](https://github.com/bitrouter/bitrouter/pull/141)) - ([09d19ad](https://github.com/bitrouter/bitrouter/commit/09d19ad66e65b63d3a53a0621b4f658f04d6ef85))
- *(code-quality)* Remove #[allow(clippy)] and eliminate potential panics ([#134](https://github.com/bitrouter/bitrouter/pull/134)) - ([f64d937](https://github.com/bitrouter/bitrouter/commit/f64d9373b6801b7d68e5c7205de3d09bbd58fd67))


## [0.10.0](https://github.com/bitrouter/bitrouter/compare/v0.9.0...v0.10.0)


### ⛰️ Features

- *(cli)* Default provider onboarding ([#124](https://github.com/bitrouter/bitrouter/pull/124)) - ([65b48cb](https://github.com/bitrouter/bitrouter/commit/65b48cb358c23b0b25969c100456ffd9d142bf26))


## [0.9.0](https://github.com/bitrouter/bitrouter/compare/v0.8.1...v0.9.0)


### ⛰️ Features

- *(hooks)* Propagate model context to GenerationHook callbacks ([#121](https://github.com/bitrouter/bitrouter/pull/121)) - ([3cdc613](https://github.com/bitrouter/bitrouter/commit/3cdc613ffcde58522575460e556598f7a87cc58f))
- *(observe)* Add `bitrouter-observe` crate for spend tracking and metrics ([#119](https://github.com/bitrouter/bitrouter/pull/119)) - ([240f547](https://github.com/bitrouter/bitrouter/commit/240f547780962c9c2308a9e65b5c726990ba7a0c))


## [0.8.0](https://github.com/bitrouter/bitrouter/compare/v0.7.1...v0.8.0)


### ⛰️ Features

- *(config)* Add `file` variant to `Modality` enum ([#110](https://github.com/bitrouter/bitrouter/pull/110)) - ([de04e02](https://github.com/bitrouter/bitrouter/commit/de04e02c0b00d60448e2c4c68efde2d849856998))


## [0.7.1](https://github.com/bitrouter/bitrouter/compare/v0.7.0...v0.7.1)


### ⛰️ Features

- *(api)* Add GET /v1/models endpoint with query filters ([#107](https://github.com/bitrouter/bitrouter/pull/107)) - ([a5d1b95](https://github.com/bitrouter/bitrouter/commit/a5d1b9546447ffc5b596e5725e7346c0421e4202))


## [0.7.0](https://github.com/bitrouter/bitrouter/compare/v0.6.1...v0.7.0)


### 🚜 Refactor

- *(core)* Refactor auth module ([#93](https://github.com/bitrouter/bitrouter/pull/93)) - ([c6042fa](https://github.com/bitrouter/bitrouter/commit/c6042fa5333258015cbc097acf8fcf27647a3e74))

### ⚙️ Miscellaneous Tasks

- *(config)* Built-in provider update ([#91](https://github.com/bitrouter/bitrouter/pull/91)) - ([5fc2147](https://github.com/bitrouter/bitrouter/commit/5fc214733bfaddc732e518284bdd64b8d09b482f))


## [0.6.1](https://github.com/bitrouter/bitrouter/compare/v0.6.0...v0.6.1)


### ⛰️ Features

- *(api)* Runtime admin API for dynamic route management ([#87](https://github.com/bitrouter/bitrouter/pull/87)) - ([0c9c502](https://github.com/bitrouter/bitrouter/commit/0c9c50277a4f2494f6e946354e3faa336f3163a3))
- *(api)* Add hooked router filters ([#90](https://github.com/bitrouter/bitrouter/pull/90)) - ([6ca74fc](https://github.com/bitrouter/bitrouter/commit/6ca74fcb838517145dd34407337ad091f1b914a3))

### ⚙️ Miscellaneous Tasks

- *(cli)* Deprecated `bitrouter --headless` ([#88](https://github.com/bitrouter/bitrouter/pull/88)) - ([d4d7b49](https://github.com/bitrouter/bitrouter/commit/d4d7b49b1e2bc2a68f3f7d2b0db05eacc8074a32))


## [0.6.0](https://github.com/bitrouter/bitrouter/compare/v0.5.0...v0.6.0)


### ⛰️ Features

- *(api)* Add per-route metrics endpoint ([#78](https://github.com/bitrouter/bitrouter/pull/78)) - ([4efc359](https://github.com/bitrouter/bitrouter/commit/4efc359f86fa48c54da8bcef640a766a4b9f14f5))
- *(cli)* Automatic latest version check from GitHub releases ([#76](https://github.com/bitrouter/bitrouter/pull/76)) - ([5f43914](https://github.com/bitrouter/bitrouter/commit/5f43914c66bf011778ea9b732a36e71844b951c2))
- *(core)* Implement hooked model ([#84](https://github.com/bitrouter/bitrouter/pull/84)) - ([712cbfd](https://github.com/bitrouter/bitrouter/commit/712cbfd8ab35a73362653f2bef8349c0df2a2540))
- *(guardrail)* Implement guardrail and basic rules ([#77](https://github.com/bitrouter/bitrouter/pull/77)) - ([333a340](https://github.com/bitrouter/bitrouter/commit/333a340bcdbb8bdcff1962ea1e8238cc50508e92))


## [0.5.0](https://github.com/bitrouter/bitrouter/compare/v0.4.0...v0.5.0)


### ⛰️ Features

- *(auth)* Web3 multi-chain wallet auth with CAIP-10 identity ([#72](https://github.com/bitrouter/bitrouter/pull/72)) - ([faeb05d](https://github.com/bitrouter/bitrouter/commit/faeb05da30c91ff8cdfcfc2bf566a90815940d4a))


## [0.4.0](https://github.com/bitrouter/bitrouter/compare/v0.3.0...v0.4.0)

### ⛰️ Features

- _(config)_ Add per-model metadata and token pricing to provider config ([#66](https://github.com/bitrouter/bitrouter/pull/66)) - ([7ba1ee1](https://github.com/bitrouter/bitrouter/commit/7ba1ee1e29b927b49e9089885e0ecd57565a854a))
- Replace API key auth with self-signed EdDSA JWTs ([#62](https://github.com/bitrouter/bitrouter/pull/62)) - ([87f1879](https://github.com/bitrouter/bitrouter/commit/87f187942eff11e9e576c96157f5ccc96685e762))

## [0.3.0](https://github.com/bitrouter/bitrouter/compare/v0.2.5...v0.3.0)

### ⛰️ Features

- _(bitrouter)_ Implement database configuration and connection ([#57](https://github.com/bitrouter/bitrouter/pull/57)) - ([ea272b9](https://github.com/bitrouter/bitrouter/commit/ea272b998045fd215e7377bbca6f0d2c6ed9d691))

## [0.2.5](https://github.com/bitrouter/bitrouter/compare/v0.2.4...v0.2.5) - 2026-03-11

### Added

- _(bitrouter)_ embed runtime and tui into `bitrouter` bin ([#51](https://github.com/bitrouter/bitrouter/pull/51))

## [0.2.4](https://github.com/bitrouter/bitrouter/compare/v0.2.3...v0.2.4) - 2026-03-11

### Added

- _(api)_ add GET /v1/routes endpoint ([#43](https://github.com/bitrouter/bitrouter/pull/43))

## [0.2.3](https://github.com/bitrouter/bitrouter/compare/v0.2.2...v0.2.3) - 2026-03-11

### Added

- add init wizard, provider auto-detection, and Google API route ([#41](https://github.com/bitrouter/bitrouter/pull/41))

## [0.2.2](https://github.com/bitrouter/bitrouter/compare/v0.2.0...v0.2.2) - 2026-03-10

### Added

- _(runtime)_ add basic api key auth ([#39](https://github.com/bitrouter/bitrouter/pull/39))

### Other

- _(repo)_ use unified workspace package
