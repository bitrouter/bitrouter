# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.22.0](https://github.com/bitrouter/bitrouter/compare/v0.21.0...v0.22.0)


### ⛰️ Features

- Per-caller policy engine for tool access control ([#296](https://github.com/bitrouter/bitrouter/pull/296)) - ([0e11786](https://github.com/bitrouter/bitrouter/commit/0e11786a0bfb7dcec5afc8c3a51abe338bdc225e))
- Spend-limit policy engine and policy CLI ([#292](https://github.com/bitrouter/bitrouter/pull/292)) - ([133f3ae](https://github.com/bitrouter/bitrouter/commit/133f3ae19427c61d533891eba42124f662dceea1))

### 🚜 Refactor

- *(config)* Place built-in signals under `src` ([#288](https://github.com/bitrouter/bitrouter/pull/288)) - ([32cfbe4](https://github.com/bitrouter/bitrouter/commit/32cfbe4e2050c3edd050c41d3d8dc1a03ce37b99))

### ⚙️ Miscellaneous Tasks

- *(providers)* Update models from models.dev ([#297](https://github.com/bitrouter/bitrouter/pull/297)) - ([ccd6cbb](https://github.com/bitrouter/bitrouter/commit/ccd6cbb82ebb5e2c3095b280f02a16b2526cb098))


## [0.21.0](https://github.com/bitrouter/bitrouter/compare/v0.20.0...v0.21.0)


### 🐛 Bug Fixes

- Add wallet variant to AuthConfig enum ([#275](https://github.com/bitrouter/bitrouter/pull/275)) - ([755239a](https://github.com/bitrouter/bitrouter/commit/755239abe88964b5e7243c1225e2d0c02aeb8840))
- Strip ANSI escape codes from model names in routing ([#284](https://github.com/bitrouter/bitrouter/pull/284)) - ([b008f58](https://github.com/bitrouter/bitrouter/commit/b008f58775184bb3305e7bd29363b51662147827))

### 📚 Documentation

- Fix stale a2a refs and broken code example ([#272](https://github.com/bitrouter/bitrouter/pull/272)) - ([2dbde0f](https://github.com/bitrouter/bitrouter/commit/2dbde0f664bab106c444c8a029cfe42c16cbd24b))

### ⚙️ Miscellaneous Tasks

- *(providers)* Update models from models.dev ([#287](https://github.com/bitrouter/bitrouter/pull/287)) - ([4a8e9e1](https://github.com/bitrouter/bitrouter/commit/4a8e9e15b0b610fb04cd222ee97150f2480dff9a))
- *(providers)* Update models from models.dev ([#283](https://github.com/bitrouter/bitrouter/pull/283)) - ([53c87d1](https://github.com/bitrouter/bitrouter/commit/53c87d15cc32c765d346b5fadc2540bc0d6325c2))


## [0.20.0](https://github.com/bitrouter/bitrouter/compare/v0.19.1...v0.20.0)


### ⛰️ Features

- Content-based auto-routing with keyword signal detection ([#269](https://github.com/bitrouter/bitrouter/pull/269)) - ([decbbd1](https://github.com/bitrouter/bitrouter/commit/decbbd1cc9541c74f4f1c75baa532dad37765046))


## [0.19.1](https://github.com/bitrouter/bitrouter/compare/v0.19.0...v0.19.1)


### ⚙️ Miscellaneous Tasks

- *(maintenance)* Auto-fix outdated deps and stale doc path ([#264](https://github.com/bitrouter/bitrouter/pull/264)) - ([7cb7a55](https://github.com/bitrouter/bitrouter/commit/7cb7a55b18a34401d996ab61833204f61ddf1b36))


## [0.19.0](https://github.com/bitrouter/bitrouter/compare/v0.18.3...v0.19.0)


### ⛰️ Features

- *(accounts)* Database-backed key revocation table ([#259](https://github.com/bitrouter/bitrouter/pull/259)) - ([a682c13](https://github.com/bitrouter/bitrouter/commit/a682c13f71988673814cdb0bb6c944e167faf06a))
- *(auth)* Add API key identity (`id` claim) for per-key tracking and revocation ([#256](https://github.com/bitrouter/bitrouter/pull/256)) - ([e7f0258](https://github.com/bitrouter/bitrouter/commit/e7f0258d4d2d338f715ab5e67baa039742be1a84))
- *(core)* Add AgentProvider trait ([#250](https://github.com/bitrouter/bitrouter/pull/250)) - ([d8e8d9b](https://github.com/bitrouter/bitrouter/commit/d8e8d9bf036992b74a4d8dc970d51f81ccc636af))


## [0.18.2](https://github.com/bitrouter/bitrouter/compare/v0.18.1...v0.18.2)


### 🐛 Bug Fixes

- *(bitrouter)* Guard skill registry path handling ([#245](https://github.com/bitrouter/bitrouter/pull/245)) - ([4b273f3](https://github.com/bitrouter/bitrouter/commit/4b273f3bec6523215254a70d27e982bb0d992e8b))
- *(bitrouter)* Config template file resolution ([#246](https://github.com/bitrouter/bitrouter/pull/246)) - ([c0ce9d3](https://github.com/bitrouter/bitrouter/commit/c0ce9d371be98618f5b7aaa276ee1e0a8aa221e2))


## [0.18.1](https://github.com/bitrouter/bitrouter/compare/v0.18.0...v0.18.1)


### ⛰️ Features

- *(config)* Add config templates with tests ([#242](https://github.com/bitrouter/bitrouter/pull/242)) - ([cb59a2b](https://github.com/bitrouter/bitrouter/commit/cb59a2b623f6de7179d46c87cf0c961a898c781a))

### ⚙️ Miscellaneous Tasks

- Add cargo-dist with npm, Homebrew, and shell installers ([#241](https://github.com/bitrouter/bitrouter/pull/241)) - ([d76c8f5](https://github.com/bitrouter/bitrouter/commit/d76c8f5561a631c0e09b02044ddecb7c4fa8ecf3))


## [0.18.0](https://github.com/bitrouter/bitrouter/compare/v0.17.0...v0.18.0)


### ⛰️ Features

- *(auth)* Unified JWT auth with operator-signed delegation model ([#226](https://github.com/bitrouter/bitrouter/pull/226)) - ([a50f610](https://github.com/bitrouter/bitrouter/commit/a50f6100dbff5f020b9908e98eef0f2d8a923a61))
- *(cli)* Migrate legacy commands to OWS wallet auth ([#224](https://github.com/bitrouter/bitrouter/pull/224)) - ([582a932](https://github.com/bitrouter/bitrouter/commit/582a9326f93e0dd5c038016ba0f6000319afd2f2))
- *(cli)* Add wallet and key commands with onboarding ([#222](https://github.com/bitrouter/bitrouter/pull/222)) - ([5ab7fb2](https://github.com/bitrouter/bitrouter/commit/5ab7fb24d67bdcde2da272f2604ee1bea3edea71))
- *(ows-signer)* OWS-backed signer for MPP close signing ([#221](https://github.com/bitrouter/bitrouter/pull/221)) - ([4885afc](https://github.com/bitrouter/bitrouter/commit/4885afca6779c82727b97b3a565a40b9d6e591e7))
- *(payment)* Add client-side payment middleware for upstream 402 handling ([#234](https://github.com/bitrouter/bitrouter/pull/234)) - ([2a0c0a1](https://github.com/bitrouter/bitrouter/commit/2a0c0a11351bfd9c05771c8792fd6fadf93040df))

### 🐛 Bug Fixes

- *(anthropic)* Accept system field as both string and array of content blocks ([#228](https://github.com/bitrouter/bitrouter/pull/228)) - ([a761d6a](https://github.com/bitrouter/bitrouter/commit/a761d6a0ae155da5b9c9838ffb124cda5674deb5))
- *(bitrouter)* Restore templates to crate directory ([#237](https://github.com/bitrouter/bitrouter/pull/237)) - ([4d98f61](https://github.com/bitrouter/bitrouter/commit/4d98f61f5d9bd51bfaa10cecdccd70f926d04aa1))
- *(bitrouter)* Gate mcp/rest/mpp code behind feature flags ([#236](https://github.com/bitrouter/bitrouter/pull/236)) - ([e658232](https://github.com/bitrouter/bitrouter/commit/e6582325c419c243d7754f3dd7a5f295cbeed1f4))

### 🚜 Refactor

- *(core)* Remove A2A protocol, add REST providers, unify tool routing ([#225](https://github.com/bitrouter/bitrouter/pull/225)) - ([9a8d64c](https://github.com/bitrouter/bitrouter/commit/9a8d64cc5574a2aa5ad081c703219510942eb371))
- *(mpp)* Upgrade mpp-br and remove legacy signer ([#215](https://github.com/bitrouter/bitrouter/pull/215)) - ([600693e](https://github.com/bitrouter/bitrouter/commit/600693efa737b2898b9306ebe90051464a2debc1))


## [0.17.0](https://github.com/bitrouter/bitrouter/compare/v0.16.0...v0.17.0)


### ⛰️ Features

- *(api)* Add PaymentGate trait for pluggable payment verification ([#213](https://github.com/bitrouter/bitrouter/pull/213)) - ([5699c2b](https://github.com/bitrouter/bitrouter/commit/5699c2b6dd3964531f40eeab2bc0c367868da663))


## [0.16.0](https://github.com/bitrouter/bitrouter/compare/v0.15.0...v0.16.0)


### 🚜 Refactor

- *(api)* Accept trait-based signer on Tempo ([#210](https://github.com/bitrouter/bitrouter/pull/210)) - ([7c6cb35](https://github.com/bitrouter/bitrouter/commit/7c6cb35c7520f55df3c9e83e5a750c86002e246e))


## [0.15.0](https://github.com/bitrouter/bitrouter/compare/v0.14.0...v0.15.0)


### ⛰️ Features

- *(cli)* Node tempo mpp onboarding ([#193](https://github.com/bitrouter/bitrouter/pull/193)) - ([f204f02](https://github.com/bitrouter/bitrouter/commit/f204f0248305a3366d94e5ce7dbec2dcd34e0ccb))
- *(config)* Add deepseek, minimax, zai, moonshot, qwen, openrouter providers ([#185](https://github.com/bitrouter/bitrouter/pull/185)) - ([2ddd7cf](https://github.com/bitrouter/bitrouter/commit/2ddd7cfe40358351659ef9c08977759c6b9aab2a))
- *(mpp)* Add server-side close support for Tempo ([#187](https://github.com/bitrouter/bitrouter/pull/187)) - ([4e69203](https://github.com/bitrouter/bitrouter/commit/4e6920370498d12739436922c40fade03920bf62))
- Configuration hot reload ([#179](https://github.com/bitrouter/bitrouter/pull/179)) - ([d116de2](https://github.com/bitrouter/bitrouter/commit/d116de233437f9476e0bbb9971efc8825a6ad7e8))

### 🐛 Bug Fixes

- *(cli)* Increase warp recursion limit ([#199](https://github.com/bitrouter/bitrouter/pull/199)) - ([ffb4e88](https://github.com/bitrouter/bitrouter/commit/ffb4e88e677445ff35dd525428e9ddda20b116ac))
- *(config)* Default provider config ([#194](https://github.com/bitrouter/bitrouter/pull/194)) - ([154eecf](https://github.com/bitrouter/bitrouter/commit/154eecf11448c73d9ed59261a484f264a384882e))
- *(mpp)* Tempo payment flow ([#202](https://github.com/bitrouter/bitrouter/pull/202)) - ([9e08658](https://github.com/bitrouter/bitrouter/commit/9e086585b037775ec0666ae7548c6262e74479b8))
- *(mpp)* Add suggested_deposit to Solana session challenges ([#184](https://github.com/bitrouter/bitrouter/pull/184)) - ([82e02ab](https://github.com/bitrouter/bitrouter/commit/82e02ab9ed321f0eaa50002f85da3b1c84b022ba))
- *(mpp)* Use Mutex to lock close guard ([#192](https://github.com/bitrouter/bitrouter/pull/192)) - ([3040ce9](https://github.com/bitrouter/bitrouter/commit/3040ce9f6f28a86312b458f2952d07206faafaf2))
- *(mpp)* Use `channel.token` for gas fee ([#190](https://github.com/bitrouter/bitrouter/pull/190)) - ([3ed5f09](https://github.com/bitrouter/bitrouter/commit/3ed5f09216e09ca4feb54c428517df8695a9c6c7))
- *(mpp)* Resolve Tempo backend lookup by payment method name ([#186](https://github.com/bitrouter/bitrouter/pull/186)) - ([fb2c81f](https://github.com/bitrouter/bitrouter/commit/fb2c81fb6372eeaf1d6d976c6e3451df40c99197))
- *(rejection)* Handle BitrouterRejection in recover handler ([#200](https://github.com/bitrouter/bitrouter/pull/200)) - ([1064c14](https://github.com/bitrouter/bitrouter/commit/1064c140ac8129230084b93f55d3334c55f5f170))

### 🚜 Refactor

- *(api)* Gate A2A and MCP handlers behind feature flags ([#204](https://github.com/bitrouter/bitrouter/pull/204)) - ([b193538](https://github.com/bitrouter/bitrouter/commit/b1935386b9e582b5b97b5ed0f794e5bbba2409d6))


## [0.14.0](https://github.com/bitrouter/bitrouter/compare/v0.13.0...v0.14.0)


### ⛰️ Features

- *(solana-mpp)* Make session challenge asset configurable ([#173](https://github.com/bitrouter/bitrouter/pull/173)) - ([a9426ce](https://github.com/bitrouter/bitrouter/commit/a9426ceb10f09c4cf5a645a071e8a033e2a97286))

### 🐛 Bug Fixes

- *(api)* Fixed solana mpp api types ([#170](https://github.com/bitrouter/bitrouter/pull/170)) - ([aba8e6a](https://github.com/bitrouter/bitrouter/commit/aba8e6ac0f4ff7ece1558f7ad21b584315db5b16))
- *(models)* Return configured routing models instead of built-in provider catalogs ([#177](https://github.com/bitrouter/bitrouter/pull/177)) - ([2b415ea](https://github.com/bitrouter/bitrouter/commit/2b415ea22c4ce9f27a440d839f8d614b9a1619ed))
- *(pricing)* Make pricing fields Optional instead of defaulting to zero ([#180](https://github.com/bitrouter/bitrouter/pull/180)) - ([6b02882](https://github.com/bitrouter/bitrouter/commit/6b02882b67c74601a0094b6e5e9112da99f32470))
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
