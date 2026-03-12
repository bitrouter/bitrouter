# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
