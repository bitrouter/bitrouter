# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.0.0-alpha.34](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.33...v1.0.0-alpha.34)


### ⛰️ Features

- *(acp)* Add native BRO server and migrate SDK runtime ([#963](https://github.com/bitrouter/bitrouter/pull/963)) - ([9956a42](https://github.com/bitrouter/bitrouter/commit/9956a427bd5d4316b3bf2359f88e293b158781d5))
- *(acp)* Persist canonical session trajectories ([#902](https://github.com/bitrouter/bitrouter/pull/902)) - ([17116a2](https://github.com/bitrouter/bitrouter/commit/17116a25e58c15f5965e2072be80de9436e36748))
- *(acp)* Read harness authentication from the protocol ([#860](https://github.com/bitrouter/bitrouter/pull/860)) - ([205f5d1](https://github.com/bitrouter/bitrouter/commit/205f5d149bb01bd5b601aa20d7064209fe5b379f))
- *(acp)* Harden v1 controller routing ([#853](https://github.com/bitrouter/bitrouter/pull/853)) - ([d97924e](https://github.com/bitrouter/bitrouter/commit/d97924e78c7dab7c6fdf93c779014583e079b97e))
- *(acp)* Add controller identity and session routing ([#852](https://github.com/bitrouter/bitrouter/pull/852)) - ([d392008](https://github.com/bitrouter/bitrouter/commit/d3920084d0f8ddc8ac0d9ef1d1994e2b2d2b9908))
- *(acp)* Add multi-session controller ([#849](https://github.com/bitrouter/bitrouter/pull/849)) - ([d6e9327](https://github.com/bitrouter/bitrouter/commit/d6e93272a70a3ecc6b807848a4447188296f6513))
- *(acp)* Rebuild the ACP agent surface and add the chat TUI ([#816](https://github.com/bitrouter/bitrouter/pull/816)) - ([f3f86ab](https://github.com/bitrouter/bitrouter/commit/f3f86ab2d47bf560a85a0ca57055458958512914))
- *(acp)* Per-session ACP substrate (acp serve|prompt) ([#613](https://github.com/bitrouter/bitrouter/pull/613)) - ([683e5de](https://github.com/bitrouter/bitrouter/commit/683e5de63beffa3be9b852b4fee2fee7372720d7))
- *(acp)* Pure-routing ACP pipeline + `bitrouter agent-proxy` stdio bridge - ([d221941](https://github.com/bitrouter/bitrouter/commit/d2219411955b7d1b61e38b86b2399c9d187dc91a))
- *(actions)* [**breaking**] One table for status, list_models, route ([#869](https://github.com/bitrouter/bitrouter/pull/869)) - ([956f394](https://github.com/bitrouter/bitrouter/commit/956f39489456dec937393b204d44c039157fc68d))
- *(ai)* [**breaking**] Add native Decisions API support ([#965](https://github.com/bitrouter/bitrouter/pull/965)) - ([d6dfc24](https://github.com/bitrouter/bitrouter/commit/d6dfc245be38c7e6d3977b0528cbcdf694fac246))
- *(approval)* Tool-approval flow + execution-denied - ([4dda86e](https://github.com/bitrouter/bitrouter/commit/4dda86e95b06e7bf7cda6c2403175cc1d7c1c3df))
- *(attestation)* NEAR confidential-inference verification (L1 + L1.5) ([#563](https://github.com/bitrouter/bitrouter/pull/563)) - ([9dc0a1d](https://github.com/bitrouter/bitrouter/commit/9dc0a1d9cea18b74f7ee112c85e5968545957103))
- *(bedrock)* Integrate Amazon Bedrock as an outbound provider - ([7698f90](https://github.com/bitrouter/bitrouter/commit/7698f90e53fbe005465ded2c854ecee6ecd3b57b))
- *(benchmark)* Harden workflow policy attribution - ([a4e7e3f](https://github.com/bitrouter/bitrouter/commit/a4e7e3f504fc9a0a95deaafe4d52f52ffb257244))
- *(bitrouter)* Windows support for the daemon control surface ([#490](https://github.com/bitrouter/bitrouter/pull/490)) - ([9a960df](https://github.com/bitrouter/bitrouter/commit/9a960df6e8e65555e62db979fb1cfd19e5330609))
- *(bro)* Improve adaptive native task execution ([#976](https://github.com/bitrouter/bitrouter/pull/976)) - ([be71c46](https://github.com/bitrouter/bitrouter/commit/be71c46d808303aa2ab757a4206331bd3bf9ec62))
- *(bro)* Show routing status and live spinner ([#973](https://github.com/bitrouter/bitrouter/pull/973)) - ([f29be9d](https://github.com/bitrouter/bitrouter/commit/f29be9d45d6202f65f8d2aa7a50bfa2ee7a5611d))
- *(bro)* [**breaking**] Unify model and context routing ([#961](https://github.com/bitrouter/bitrouter/pull/961)) - ([4ab82de](https://github.com/bitrouter/bitrouter/commit/4ab82de94f7c6fab583fd9fea1bddb1ea42eea97))
- *(bro)* Connect native harness to routing core ([#960](https://github.com/bitrouter/bitrouter/pull/960)) - ([0a2325b](https://github.com/bitrouter/bitrouter/commit/0a2325b222a0cb71841ff5550d5f0e9d8d950fe3))
- *(bro)* Add durable conversations and native harness ([#945](https://github.com/bitrouter/bitrouter/pull/945)) - ([aa2ec34](https://github.com/bitrouter/bitrouter/commit/aa2ec3476f68b1a5a72faba2e9f97f2be767a120))
- *(capabilities)* Extend Capability vocabulary (tools, reasoning, web_search, logprobs) ([#536](https://github.com/bitrouter/bitrouter/pull/536)) - ([e369f2e](https://github.com/bitrouter/bitrouter/commit/e369f2e184832c877f42bbb7b0673b9f38f3ef06))
- *(capabilities)* Extend Capability vocabulary (tools, reasoning, web_search, logprobs) - ([e1a9238](https://github.com/bitrouter/bitrouter/commit/e1a92388428db927387fdb49f7778f0a6cb49b39))
- *(capabilities)* Capability-aware routing primitives + typed structured-output fields (B4) ([#528](https://github.com/bitrouter/bitrouter/pull/528)) - ([e4fee7c](https://github.com/bitrouter/bitrouter/commit/e4fee7c85b9a7e134152cce3cf412d958183e419))
- *(chat)* Resolve slash commands against the actions table ([#880](https://github.com/bitrouter/bitrouter/pull/880)) - ([61dd773](https://github.com/bitrouter/bitrouter/commit/61dd77333a91f7aab01647b0ae7d8625d3039ffe))
- *(claude-code)* Route Claude Code traffic to a dedicated subscription provider ([#593](https://github.com/bitrouter/bitrouter/pull/593)) - ([15fde63](https://github.com/bitrouter/bitrouter/commit/15fde6346c209e4f864d0b35e573006588765022))
- *(cli)* [**breaking**] Rename the invocation to `bro` - ([23cc164](https://github.com/bitrouter/bitrouter/commit/23cc164452fe844b04919aeb93e05a83286d775c))
- *(cli)* Unify ACP lifecycle surfaces ([#885](https://github.com/bitrouter/bitrouter/pull/885)) - ([63b05b8](https://github.com/bitrouter/bitrouter/commit/63b05b874db10ba29fb73d3c923e99a5c329757d))
- *(cli)* Operate trajectory history - ([87695fb](https://github.com/bitrouter/bitrouter/commit/87695fb5a60715cfbc657b58a4b960155c49065d))
- *(cli)* JSON output layer with a uniform error envelope ([#610](https://github.com/bitrouter/bitrouter/pull/610)) - ([df6e3cc](https://github.com/bitrouter/bitrouter/commit/df6e3cc487bba9e60461828c86a028656a619e21))
- *(config)* Add provider header policies ([#910](https://github.com/bitrouter/bitrouter/pull/910)) - ([9691d84](https://github.com/bitrouter/bitrouter/commit/9691d849f4bf9813d412491e20d6c97419e7d600))
- *(config)* JSON Schema + config validate for IaC ([#575](https://github.com/bitrouter/bitrouter/pull/575)) - ([8ab2382](https://github.com/bitrouter/bitrouter/commit/8ab2382240964dbbdb7804c808d4c22eead3b672))
- *(config)* Provider derives inheritance (acceptance F20) - ([62f30c2](https://github.com/bitrouter/bitrouter/commit/62f30c24db21e45210168a299e52dc063d80b8e9))
- *(control)* Add remote router administration ([#901](https://github.com/bitrouter/bitrouter/pull/901)) - ([a88ce8e](https://github.com/bitrouter/bitrouter/commit/a88ce8e2d6fba3e2c4359287d22e046224eb9616))
- *(core)* Add managed agent orchestration ([#956](https://github.com/bitrouter/bitrouter/pull/956)) - ([5679fec](https://github.com/bitrouter/bitrouter/commit/5679fec46a74820c76b94b57e857d22a3c72f715))
- *(daemon)* Hand off idle local daemon safely ([#932](https://github.com/bitrouter/bitrouter/pull/932)) - ([d8b66a9](https://github.com/bitrouter/bitrouter/commit/d8b66a9edf44b5f46afac66ce01e976c21fe6df4))
- *(eval)* Make policy locks the routing source ([#762](https://github.com/bitrouter/bitrouter/pull/762)) - ([ef3fb69](https://github.com/bitrouter/bitrouter/commit/ef3fb6993605351dabc635e5a7bab89ca4937907))
- *(evolution)* Add checkpoint TS routing ([#907](https://github.com/bitrouter/bitrouter/pull/907)) - ([94a3fb3](https://github.com/bitrouter/bitrouter/commit/94a3fb3d7103ad96cfdb2a43a2d8c68a9def68d7))
- *(extensions)* [**breaking**] Add native request-check host ([#923](https://github.com/bitrouter/bitrouter/pull/923)) - ([750d618](https://github.com/bitrouter/bitrouter/commit/750d618ad4235adefaaa33e4d864ef61169440fd))
- *(fusion)* Multi-model deliberation server tool + alias ([#574](https://github.com/bitrouter/bitrouter/pull/574)) - ([c7bfac9](https://github.com/bitrouter/bitrouter/commit/c7bfac9482d86287e8c44d789f532edc16516e62))
- *(guardrails)* [**breaking**] Plugin + per-account rule resolution ([#503](https://github.com/bitrouter/bitrouter/pull/503)) - ([ee06962](https://github.com/bitrouter/bitrouter/commit/ee069621f1b56eaecc45d54b9409b69ce3383d92))
- *(launch)* Route every catalog harness ([#781](https://github.com/bitrouter/bitrouter/pull/781)) - ([5cefa6d](https://github.com/bitrouter/bitrouter/commit/5cefa6da240cddbd5763f6eb04ec3b4750125798))
- *(mcp)* Add native skills gateway support ([#912](https://github.com/bitrouter/bitrouter/pull/912)) - ([b1cd8f3](https://github.com/bitrouter/bitrouter/commit/b1cd8f334b68999df7354071327afcc4c8188592))
- *(mcp)* Upgrade to stable rmcp 3.1 for MCP 2026-07-28 ([#760](https://github.com/bitrouter/bitrouter/pull/760)) - ([5536fe5](https://github.com/bitrouter/bitrouter/commit/5536fe525829ec51d373ac611ae89d1fed449dfc))
- *(mcp)* Answer the MCP initialize/ping handshake at the gateway ([#561](https://github.com/bitrouter/bitrouter/pull/561)) - ([43d1365](https://github.com/bitrouter/bitrouter/commit/43d1365fce758f4fb2b4ea528d01ef55fdd92d82))
- *(mcp)* Aggregation + caching + SSE + sampling-deny ([#484](https://github.com/bitrouter/bitrouter/pull/484)) - ([f1ab313](https://github.com/bitrouter/bitrouter/commit/f1ab313582bc92655c9eec9c27524a0622e3e063))
- *(mcp)* Wire upstream MCP routing via rmcp behind sdk `mcp` feature - ([ec24538](https://github.com/bitrouter/bitrouter/commit/ec24538600d6a95ab3f228436b7b5edac1ee3099))
- *(mcp+acp)* Wire pipelines into App + mount POST /mcp/{server} - ([23618d6](https://github.com/bitrouter/bitrouter/commit/23618d6165b9ab935ad21882dfd7dac85ddaf176))
- *(messages)* Preserve thinking-block signature through the streaming round-trip ([#592](https://github.com/bitrouter/bitrouter/pull/592)) - ([4d76676](https://github.com/bitrouter/bitrouter/commit/4d766767a29cc90149c919a3197e24d47bebad4a))
- *(messages)* Integrate Opus 4.8 Messages API parameters ([#509](https://github.com/bitrouter/bitrouter/pull/509)) - ([c768b50](https://github.com/bitrouter/bitrouter/commit/c768b508f6374e861566ab1ba94e546a0ed1a30d))
- *(metadata)* Per-part provider_metadata + cache_control - ([b9769a1](https://github.com/bitrouter/bitrouter/commit/b9769a122dfa558c05990a2b64f3e59bc67e035c))
- *(metering)* Context-tier ("staged") pricing ([#551](https://github.com/bitrouter/bitrouter/pull/551)) - ([0464703](https://github.com/bitrouter/bitrouter/commit/04647033f11f3e908178b354482bd1333ee289eb))
- *(multimodal)* Output file parts (stream + response) - ([9acf6ab](https://github.com/bitrouter/bitrouter/commit/9acf6ab0f76c26c1c05b41b0e6b66ba307c1ed51))
- *(multimodal)* Wire request output modalities - ([99480f1](https://github.com/bitrouter/bitrouter/commit/99480f1004a3be1b1a0a0555e645c94babcd4fe3))
- *(multimodal)* Parse provider media into file parts + tests - ([e55eaf6](https://github.com/bitrouter/bitrouter/commit/e55eaf67cfa0656d09d75635bf62277d9b2d1c35))
- *(multimodal)* File content IR + capability detection + render - ([6fc29c3](https://github.com/bitrouter/bitrouter/commit/6fc29c37a4320c58a40b9bc9c39eb426d6067bbf))
- *(observability)* [**breaking**] Live router view and hosted launch ([#803](https://github.com/bitrouter/bitrouter/pull/803)) - ([25f69b7](https://github.com/bitrouter/bitrouter/commit/25f69b71f5660253c7ee46874b460370e954060d))
- *(observability)* V0-style "request received" / "request finished" log lines - ([b854d90](https://github.com/bitrouter/bitrouter/commit/b854d902f3a07468b666d137d3bbb8c8b749c8f2))
- *(observe)* [**breaking**] Forward cloud span attrs + content capture ([#535](https://github.com/bitrouter/bitrouter/pull/535)) - ([2c7edd3](https://github.com/bitrouter/bitrouter/commit/2c7edd3a54014d59c5048424ae828f9011b68a3e))
- *(observe)* [**breaking**] GenAI-semconv trace hierarchy + outbound traceparent ([#495](https://github.com/bitrouter/bitrouter/pull/495)) - ([d05fd5c](https://github.com/bitrouter/bitrouter/commit/d05fd5cbd155f3d798ac70c0b341b00176ff9c55))
- *(observe)* Wire Prometheus into assembly and expose /metrics - ([1fdbedc](https://github.com/bitrouter/bitrouter/commit/1fdbedcdad3f4ac11fd8fe8d3e821831f795386a))
- *(params)* Typed sampling options across protocols - ([062a952](https://github.com/bitrouter/bitrouter/commit/062a9529f35b9974d33b2eafb44fe88e4a4215bf))
- *(policy)* Route models by reasoning effort ([#804](https://github.com/bitrouter/bitrouter/pull/804)) - ([fa402d4](https://github.com/bitrouter/bitrouter/commit/fa402d489ace1ecd7d7f5ac5bc01577b35ea8958))
- *(policy)* Route agent workflows by predicted role ([#790](https://github.com/bitrouter/bitrouter/pull/790)) - ([05fedf9](https://github.com/bitrouter/bitrouter/commit/05fedf9437a0d3c7beacd517b69c5fc590d47777))
- *(policy)* Add trajectory progress guard - ([8f03e48](https://github.com/bitrouter/bitrouter/commit/8f03e4801caa74d2dbb7fe40161b2c71baa91733))
- *(policy)* Harden cost-aware workflow routing ([#768](https://github.com/bitrouter/bitrouter/pull/768)) - ([185c5a1](https://github.com/bitrouter/bitrouter/commit/185c5a1aa792c63990e5bba78e462104e1e42fe6))
- *(policy)* Generalize agent trace routing ([#757](https://github.com/bitrouter/bitrouter/pull/757)) - ([2722d77](https://github.com/bitrouter/bitrouter/commit/2722d77bc9f6d184e6dd37e5cd769ac48de93b70))
- *(protocol)* FinishReason::Other/Error + stream error status map - ([fbfa8cb](https://github.com/bitrouter/bitrouter/commit/fbfa8cb2052cb8c67597f7f35c0f7885d220510e))
- *(protocol)* Cache_read/cache_write tokens through Usage + settlement - ([b1c5676](https://github.com/bitrouter/bitrouter/commit/b1c5676c24d8197e09c8dbd4e7324d8ff9ee3fc7))
- *(providers)* Claude & Codex OAuth subscriptions ([#519](https://github.com/bitrouter/bitrouter/pull/519)) - ([6983051](https://github.com/bitrouter/bitrouter/commit/6983051e5dc93b4b9bfb9ddcf1e26a2f1c29f8d7))
- *(registry)* Add agents and runtimes as a primitive pair ([#879](https://github.com/bitrouter/bitrouter/pull/879)) - ([f8c09b1](https://github.com/bitrouter/bitrouter/commit/f8c09b154256b704082aa20d89de1380b07e74f1))
- *(registry)* Registry-backed model resolution ([#589](https://github.com/bitrouter/bitrouter/pull/589)) - ([fa4083e](https://github.com/bitrouter/bitrouter/commit/fa4083ec5789cb970a5e291786609ef9f8005e00))
- *(reload)* Propagate API-key env vars from CLI to running daemon - ([9eb46a9](https://github.com/bitrouter/bitrouter/commit/9eb46a9332b02836a4d4386ff6dacc9359f470e4))
- *(result)* Finish-reason raw + schema description - ([d713a4d](https://github.com/bitrouter/bitrouter/commit/d713a4da1f7d65920c38fd2f6ece0ffdb9f9438b))
- *(router)* Add named routers and preset migration ([#916](https://github.com/bitrouter/bitrouter/pull/916)) - ([022f8aa](https://github.com/bitrouter/bitrouter/commit/022f8aa3de2d75d6567b55e08fee405f41ba93a5))
- *(routing)* Adaptive, self-optimizing policy-table routing ([#710](https://github.com/bitrouter/bitrouter/pull/710)) - ([0828e7e](https://github.com/bitrouter/bitrouter/commit/0828e7ed40104272da4b6e429b05cb3e5f03079d))
- *(routing)* Productize preset-bound policy locks - ([98516d3](https://github.com/bitrouter/bitrouter/commit/98516d3273ddb3846b7f23b1a2aef3ca4d07ef16))
- *(routing)* Gate learned downgrades on task rewards - ([ef9faf3](https://github.com/bitrouter/bitrouter/commit/ef9faf370637aecbb03bee8f511d5a1f2b1fa023))
- *(routing)* Feed benchmark failures into adequacy - ([8b9a7d8](https://github.com/bitrouter/bitrouter/commit/8b9a7d8c27dee06ed3c9116314be6115df4fb40a))
- *(routing)* Route policies by workflow state - ([068bdef](https://github.com/bitrouter/bitrouter/commit/068bdeff8f2d6a1cb523b7edf62df2a7600bc22d))
- *(routing)* Aggressive downgrade discovery + convergence eval - ([26d72dc](https://github.com/bitrouter/bitrouter/commit/26d72dcf91b206ad5735180e41ba143166a6709b))
- *(routing)* Online adequacy ledger for adaptive policy-table routing - ([38e7ca8](https://github.com/bitrouter/bitrouter/commit/38e7ca8a02b7095d727d016f4084ce7356d99644))
- *(routing)* Config-driven policy-table model router - ([d377037](https://github.com/bitrouter/bitrouter/commit/d377037ae2d4c401a7e016c9b6c7e8459d08321b))
- *(routing)* Multiple accounts per provider — failover + load-balance ([#473](https://github.com/bitrouter/bitrouter/pull/473)) - ([a248fd3](https://github.com/bitrouter/bitrouter/commit/a248fd3644b08aebbde8b64f24bfe05716a536e0))
- *(sdk)* Clarify request timing metrics ([#738](https://github.com/bitrouter/bitrouter/pull/738)) - ([a5b0730](https://github.com/bitrouter/bitrouter/commit/a5b073078c38c4d449b872cd1fb43fed9cc0bd78))
- *(sdk)* Expose server-tool usage for observability ([#568](https://github.com/bitrouter/bitrouter/pull/568)) - ([5eaf1c3](https://github.com/bitrouter/bitrouter/commit/5eaf1c35d399d6e84f8fd0cd9c644a0f6d1cf17b))
- *(sdk)* Surface MCP upstream 401/403 auth challenges (UpstreamAuth) ([#534](https://github.com/bitrouter/bitrouter/pull/534)) - ([f81a322](https://github.com/bitrouter/bitrouter/commit/f81a3221fa1918bf750612cb2c615bc4d59e67b4))
- *(sdk)* Implement VirtualModel priority/cascade strategy ([#521](https://github.com/bitrouter/bitrouter/pull/521)) - ([6b06d94](https://github.com/bitrouter/bitrouter/commit/6b06d94ac1d0a1f47cd463fd6d76b41b4c44732f))
- *(sdk)* [**breaking**] Per-target Messages auth scheme (x-api-key | Bearer) ([#516](https://github.com/bitrouter/bitrouter/pull/516)) - ([da10012](https://github.com/bitrouter/bitrouter/commit/da10012e4db97db650ccd69c8ac243ef2172e511))
- *(sdk)* [**breaking**] Streaming gen_ai.response.id via StreamPart::ResponseStarted ([#500](https://github.com/bitrouter/bitrouter/pull/500)) - ([4432941](https://github.com/bitrouter/bitrouter/commit/44329412689e41aca16b743f452b689c8e22d998))
- *(sdk)* Add RouterOptions to opt out of built-in routes ([#478](https://github.com/bitrouter/bitrouter/pull/478)) - ([c548527](https://github.com/bitrouter/bitrouter/commit/c5485279531f351d4ebd2422435c4fb968bc9bb1))
- *(sdk)* Structured outputs (response_format) for all 4 protocols ([#472](https://github.com/bitrouter/bitrouter/pull/472)) - ([8d4dc54](https://github.com/bitrouter/bitrouter/commit/8d4dc542e65f28ab751b38c1633839d8e9e4c243))
- *(sdk)* Derive JsonSchema on wire types ([#467](https://github.com/bitrouter/bitrouter/pull/467)) ([#469](https://github.com/bitrouter/bitrouter/pull/469)) - ([901ec3e](https://github.com/bitrouter/bitrouter/commit/901ec3e83fdbbe689f3943cee46de2a3c0f46cdb))
- *(sdk)* Per-provider AuthApplier for HttpExecutor - ([c42f43e](https://github.com/bitrouter/bitrouter/commit/c42f43e431d2f64c214dfc982dc3cf3e6e80fdab))
- *(sdk)* Split ProtocolAdapter into Inbound + Outbound + Transport - ([2cd8941](https://github.com/bitrouter/bitrouter/commit/2cd89412611f6f003d046a88818dad582d4633d8))
- *(sdk)* Thread config path into ConfigRoutingTable for reload - ([72f21cc](https://github.com/bitrouter/bitrouter/commit/72f21cc4c87c3b0f4dc62c4f1236f77afe25fd53))
- *(sdk)* Add MppVerifier credential-verification trait - ([7c1753b](https://github.com/bitrouter/bitrouter/commit/7c1753b955d71a7d436f75578f6725dd95c8dba6))
- *(sdk)* Wire ResponseCompleted through protocol adapters - ([771ef13](https://github.com/bitrouter/bitrouter/commit/771ef13ad44a2c747949dc47c92403662e97f7a3))
- *(sdk)* Add ResponseCompleted StreamPart variant - ([7dceb45](https://github.com/bitrouter/bitrouter/commit/7dceb4513e5cad9e4e4f991b5b7c2bebe0c4a392))
- *(sdk)* Add mcp and acp pure-routing protocol modules - ([469230b](https://github.com/bitrouter/bitrouter/commit/469230b035c6de7ae1979988ecbc63f3a5a1c788))
- *(sdk)* Wire skip_auth through App and server - ([35bb7d4](https://github.com/bitrouter/bitrouter/commit/35bb7d4b0ab5bb37975e192e347cca854864139c))
- *(sdk)* Registry-style config + cascade routing tables - ([e616309](https://github.com/bitrouter/bitrouter/commit/e61630944016622011e51bf8435a9b0466c301fa))
- *(sdk)* Wire four inbound protocols in axum server - ([0c3ffce](https://github.com/bitrouter/bitrouter/commit/0c3ffce0a4d63dc920282455ccf4555829897f31))
- *(sdk)* Add config_file loader with env substitution - ([c5d6242](https://github.com/bitrouter/bitrouter/commit/c5d62424b5c977fc8d2194acbf92cb226f4d53fa))
- *(sdk)* Add protocol-aware HttpExecutor - ([210e2c3](https://github.com/bitrouter/bitrouter/commit/210e2c366eaf73107476f76d0949b430dee0dd18))
- *(sdk)* Add OpenAI Responses adapter - ([f28798a](https://github.com/bitrouter/bitrouter/commit/f28798aef9192438c48367a763b27efb6a381bde))
- *(sdk)* Add Google Generative AI adapter - ([8867173](https://github.com/bitrouter/bitrouter/commit/8867173fc0faa34807f11da5e2662976734a77f4))
- *(sdk)* Add Anthropic Messages adapter - ([51b4664](https://github.com/bitrouter/bitrouter/commit/51b4664050a385dbd4c5cd3a8747aef151c320d1))
- *(sdk)* Add OpenAI Chat Completions adapter - ([e4d73bc](https://github.com/bitrouter/bitrouter/commit/e4d73bcae4830218853674d63cbd5e315791cb52))
- *(sdk)* Add protocol adapter traits and dispatch - ([03d2f7d](https://github.com/bitrouter/bitrouter/commit/03d2f7d96922500e78786778a8a275db5bf6bd44))
- *(sdk)* Add App builder and axum server skeleton - ([4530f85](https://github.com/bitrouter/bitrouter/commit/4530f8500ed3ccf1ac667174843e49d652b388a0))
- *(sdk)* Add language_model pipeline, streaming, routing - ([e656bba](https://github.com/bitrouter/bitrouter/commit/e656bbaa10d578933f45a0a1d001b9424acbe403))
- *(sdk)* Add language_model types, hooks and contexts - ([8bafd31](https://github.com/bitrouter/bitrouter/commit/8bafd3183a6563cb86c3b457a6a9ad8ef9770569))
- *(sdk)* Add caller, metrics and plugin shared code - ([f1f58a2](https://github.com/bitrouter/bitrouter/commit/f1f58a2915e7df8c24566662f22958cc8cf0e81d))
- *(sdk)* Add BitrouterError and typed EventBus - ([05f80e0](https://github.com/bitrouter/bitrouter/commit/05f80e0a11fd423a7669a815ab6f0e28b129c04d))
- *(sdk/mcp)* Expose headers, set_caller, evict for auth ([#494](https://github.com/bitrouter/bitrouter/pull/494)) - ([7bf6e76](https://github.com/bitrouter/bitrouter/commit/7bf6e76efc9ceecfcaf3da80efc5bedfb0a184f8))
- *(server-tools)* Built-in web_fetch tool (BYOK) ([#612](https://github.com/bitrouter/bitrouter/pull/612)) - ([ba5308d](https://github.com/bitrouter/bitrouter/commit/ba5308df77cf8457812b93af6ff557a8c8f24728))
- *(server-tools)* Add Tavily web_search backend, drop Perplexity ([#608](https://github.com/bitrouter/bitrouter/pull/608)) - ([6579faa](https://github.com/bitrouter/bitrouter/commit/6579faa122381208b5b4902b8c59b1429fd640e6))
- *(server-tools)* Built-in web_search with BYOK backends ([#603](https://github.com/bitrouter/bitrouter/pull/603)) - ([7ebc572](https://github.com/bitrouter/bitrouter/commit/7ebc572ef33cb902a8a1ecea2b7d351a1300c4c1))
- *(server-tools)* Replace owned provider-defined tools on inject ([#559](https://github.com/bitrouter/bitrouter/pull/559)) - ([df70e12](https://github.com/bitrouter/bitrouter/commit/df70e1260891f1694c802d32b319a66686bd2c17))
- *(server-tools)* Pass ToolContext to RouterToolset ([#558](https://github.com/bitrouter/bitrouter/pull/558)) - ([bffff57](https://github.com/bitrouter/bitrouter/commit/bffff57506fec640f6aa8e25d48094fd78a01127))
- *(server-tools)* Router-executed server-side tool loop ([#555](https://github.com/bitrouter/bitrouter/pull/555)) - ([e41e1ce](https://github.com/bitrouter/bitrouter/commit/e41e1cef6481929918f001ca1ec4e54e95d99988))
- *(skills)* [**breaking**] Unify discovery and conform to SEP-2640 ([#870](https://github.com/bitrouter/bitrouter/pull/870)) - ([7ee79ea](https://github.com/bitrouter/bitrouter/commit/7ee79eab35700d02bd3b6e795b0f5aafab36a64d))
- *(skills)* [**breaking**] Serve over MCP (SEP-2640), cut installer ([#770](https://github.com/bitrouter/bitrouter/pull/770)) - ([afeb3ab](https://github.com/bitrouter/bitrouter/commit/afeb3ab50d663199843cc1984fbfc0bf4de02814))
- *(source)* Web-search citations across protocols - ([0f5a768](https://github.com/bitrouter/bitrouter/commit/0f5a76881b28e8d6cedfb73d62d63f15957db2f3))
- *(stream)* Block-boundary lifecycle markers - ([afcd257](https://github.com/bitrouter/bitrouter/commit/afcd257aa5afdb4092e5f4199a4717698db2c624))
- *(tool-result)* Structured V3 output union + tool_name - ([615a95e](https://github.com/bitrouter/bitrouter/commit/615a95e748e41cdcc67e96226f5e2d6a34b04e1f))
- *(tools)* Dynamic MCP tool calls + inline results - ([d701171](https://github.com/bitrouter/bitrouter/commit/d701171ffbfe9ccc2e5ec5d55ec9cf025f7e2ef5))
- *(tools)* Function strict + provider-defined tools - ([0769c82](https://github.com/bitrouter/bitrouter/commit/0769c8288f01c7e7d210eb28aec9635b65b352fe))
- *(tools)* Provider-executed calls + typed tool choice - ([2751d4e](https://github.com/bitrouter/bitrouter/commit/2751d4eb34404377216bf720635dba7b36fef95a))
- *(tui)* Add supervised background agent control deck ([#927](https://github.com/bitrouter/bitrouter/pull/927)) - ([4ac3d62](https://github.com/bitrouter/bitrouter/commit/4ac3d620a0ac0ae7377095cde742b1e60b4a32b3))
- *(tui)* Replace dashboard with conversation - ([6589188](https://github.com/bitrouter/bitrouter/commit/65891881df6b4584a154955bb05c80cc29427351))
- *(tui)* Composite manager  ([#715](https://github.com/bitrouter/bitrouter/pull/715)) - ([7a72dff](https://github.com/bitrouter/bitrouter/commit/7a72dfffc997d9b7becc1cf2f77fc57573674f23))
- Add router request checks and receipts ([#919](https://github.com/bitrouter/bitrouter/pull/919)) - ([2b01d2e](https://github.com/bitrouter/bitrouter/commit/2b01d2e6eab72274fb4b3571534fb5c9e656746b))
- Optimize routes from observed history ([#833](https://github.com/bitrouter/bitrouter/pull/833)) - ([9fa7a4c](https://github.com/bitrouter/bitrouter/commit/9fa7a4c9973164bf2cb9b5f7339476b365888254))
- Publish bitrouter/auto as the router slug ([#788](https://github.com/bitrouter/bitrouter/pull/788)) - ([8b702eb](https://github.com/bitrouter/bitrouter/commit/8b702eb9376ea10025e5d4f1643a23e14f9e4565))
- Guide @auto workflow optimization ([#779](https://github.com/bitrouter/bitrouter/pull/779)) - ([bbca8ec](https://github.com/bitrouter/bitrouter/commit/bbca8eca2185bc036fb3d6ab3974dc1f07cfca59))
- Add cache-aware routing, Terminus 2 identity, and Claude bridge ([#717](https://github.com/bitrouter/bitrouter/pull/717)) - ([0a4f3ee](https://github.com/bitrouter/bitrouter/commit/0a4f3ee9b93958a3f6fc9725c1dbad334545a8f7))
- Add AWS Bedrock, Azure, Vertex built-in providers ([#647](https://github.com/bitrouter/bitrouter/pull/647)) - ([6b92fcf](https://github.com/bitrouter/bitrouter/commit/6b92fcf612ee5eff54e426f7ba8b50a023002caa))
- Configurable upstream HTTP timeouts ([#662](https://github.com/bitrouter/bitrouter/pull/662)) - ([88d1e4d](https://github.com/bitrouter/bitrouter/commit/88d1e4da5a774bf44a7e569be2fda3e60dd47c7a))
- Read the existing Claude Code session for Claude subscription auth ([#590](https://github.com/bitrouter/bitrouter/pull/590)) - ([a720eb2](https://github.com/bitrouter/bitrouter/commit/a720eb231faab462de5c9b8929f67323772b0d3f))
- Protocol-native routing ([#554](https://github.com/bitrouter/bitrouter/pull/554)) - ([197f812](https://github.com/bitrouter/bitrouter/commit/197f8126a108abd30a2b9f02cb59d9b0028b54d4))

### 🐛 Bug Fixes

- *(acp)* Close two correctness gaps from Phase 3 review - ([aa2373c](https://github.com/bitrouter/bitrouter/commit/aa2373ceb9deff48c4d71b241e315f5befa54073))
- *(anthropic)* Close block on text/thinking/tool kind transition - ([b8dc57b](https://github.com/bitrouter/bitrouter/commit/b8dc57b67b6140de6c31ed9347d1421cae7160a3))
- *(auth)* Preserve fallback semantics - ([6f6185d](https://github.com/bitrouter/bitrouter/commit/6f6185d13522d3f496c9d54e7b72d7a035e5f657))
- *(auth)* Redact extension failures - ([1a41c85](https://github.com/bitrouter/bitrouter/commit/1a41c85206327c76982e479880e393bf67cc14e4))
- *(billing,security)* V0 #463 / cloud #251 audit follow-ups - ([fc3d9ee](https://github.com/bitrouter/bitrouter/commit/fc3d9eee50ac5ab9f1795637db193433e3efe517))
- *(bro)* Stabilize native sessions and TUI ([#970](https://github.com/bitrouter/bitrouter/pull/970)) - ([44ff972](https://github.com/bitrouter/bitrouter/commit/44ff9728f2467e81447761d94a8c63bf75d2372a))
- *(chat)* Buffer streamed tool call until name arrives ([#577](https://github.com/bitrouter/bitrouter/pull/577)) - ([ab02eb6](https://github.com/bitrouter/bitrouter/commit/ab02eb6ddc4fd56110f5d7038b5b19830bb588ea))
- *(chat)* Drop provider-defined tools (no chat wire form) ([#553](https://github.com/bitrouter/bitrouter/pull/553)) - ([aa58bac](https://github.com/bitrouter/bitrouter/commit/aa58bacad91e2558b1b3bd76786a305d3c2b0631))
- *(ci)* Restore compatibility with Rust stable ([#955](https://github.com/bitrouter/bitrouter/pull/955)) - ([b0312a6](https://github.com/bitrouter/bitrouter/commit/b0312a667badb5bfd88833501eb03ad1a91956ad))
- *(claude-code)* Detect subscription traffic by the agent-profile beta ([#594](https://github.com/bitrouter/bitrouter/pull/594)) - ([d528b47](https://github.com/bitrouter/bitrouter/commit/d528b47420dc941e77484cb1f8f8c9155a680858))
- *(cli)* Onboard into the default ACP TUI ([#886](https://github.com/bitrouter/bitrouter/pull/886)) - ([d357ee9](https://github.com/bitrouter/bitrouter/commit/d357ee9a0370238ceb0ad4a6b6ba006a966b30da))
- *(codex)* Preserve ACP tools through Responses ([#892](https://github.com/bitrouter/bitrouter/pull/892)) - ([6f10d52](https://github.com/bitrouter/bitrouter/commit/6f10d52808ebf95ba98969f2652a998b13841c5e))
- *(codex)* Refresh subscription auth after 401 - ([92c267c](https://github.com/bitrouter/bitrouter/commit/92c267c5d0685bbbfba1b56ba7bc8bd52720440c))
- *(codex)* Harden Codex spawn checks ([#632](https://github.com/bitrouter/bitrouter/pull/632)) - ([e2264d6](https://github.com/bitrouter/bitrouter/commit/e2264d636e35689a4ac4366c421836cf4c43ed72))
- *(config)* Make server.log_level set the log filter ([#806](https://github.com/bitrouter/bitrouter/pull/806)) - ([6c90156](https://github.com/bitrouter/bitrouter/commit/6c9015690342fbae8d661aa8676fea149d7cb0a9))
- *(config)* Don't expand ${VAR} inside YAML comments ([#609](https://github.com/bitrouter/bitrouter/pull/609)) - ([7628d98](https://github.com/bitrouter/bitrouter/commit/7628d9855412da2a51e07ac11fe31a8b3510850e))
- *(mcp,http)* JSON-RPC id round-trip + 401 WWW-Authenticate - ([7268291](https://github.com/bitrouter/bitrouter/commit/72682916d5e229e6fb708bd15114ba81e4f92d58))
- *(messages)* Drop unsigned reasoning on Anthropic wire ([#669](https://github.com/bitrouter/bitrouter/pull/669)) - ([d78ddec](https://github.com/bitrouter/bitrouter/commit/d78ddec4a3bc05e11c60e00f64874387fbdccb38))
- *(metadata)* Drop dead message slot + system caching - ([e9bdf2a](https://github.com/bitrouter/bitrouter/commit/e9bdf2afabdd0478343efc2f7869fbde18b061a6))
- *(metering)* Preserve usage across trace restarts - ([fddb958](https://github.com/bitrouter/bitrouter/commit/fddb958645d91c5b277789c8c175312e5ab81f26))
- *(metering)* Close P0/P2/P3 findings from final OSS-refactor review - ([61ce2fb](https://github.com/bitrouter/bitrouter/commit/61ce2fbe64f046a326f49f4d78b766b19966ab50))
- *(pipeline)* Settle routing failures - ([3db74eb](https://github.com/bitrouter/bitrouter/commit/3db74ebabb75c545c868adcfaedc69923edfe1df))
- *(pipeline)* Run non-streaming requests to completion ([#538](https://github.com/bitrouter/bitrouter/pull/538)) - ([5528e3e](https://github.com/bitrouter/bitrouter/commit/5528e3e33fff495ee17ab41d1dc35962f5a6f2e8))
- *(policy)* Bind progress guard evidence - ([fcbbacc](https://github.com/bitrouter/bitrouter/commit/fcbbacc1bbb792b798773a5d169420a95dbfaa79))
- *(policy)* Make runtime mode authoritative ([#761](https://github.com/bitrouter/bitrouter/pull/761)) - ([a44a6c8](https://github.com/bitrouter/bitrouter/commit/a44a6c87f0a9da357d44eb5cca57e70914c8381c))
- *(protocol)* Preserve Anthropic tool extension fields ([#774](https://github.com/bitrouter/bitrouter/pull/774)) - ([119cd0f](https://github.com/bitrouter/bitrouter/commit/119cd0f0935d9d77beeae9d8671b14c817bb1a96))
- *(protocol)* Preserve Codex namespace tools - ([b173160](https://github.com/bitrouter/bitrouter/commit/b173160853063e20dd1f3389d0f80a95c6964bdb))
- *(protocol)* Drop unsigned thinking replay - ([8b3e2be](https://github.com/bitrouter/bitrouter/commit/8b3e2bea5c4e8f01ac288a0635a4afa83e040406))
- *(protocol)* Don't fragment streamed tool calls ([#552](https://github.com/bitrouter/bitrouter/pull/552)) - ([e140b2f](https://github.com/bitrouter/bitrouter/commit/e140b2fb105de59b600104ed0f1019cad66d1cc9))
- *(protocol)* Translate tool_choice across protocols ([#549](https://github.com/bitrouter/bitrouter/pull/549)) - ([8e60ce5](https://github.com/bitrouter/bitrouter/commit/8e60ce5e9e855d13f9d82e1381bafd4fcd0564bf))
- *(protocol)* Google stream/top-level extras + chat include_usage - ([9c57537](https://github.com/bitrouter/bitrouter/commit/9c57537210a0b0c61c8fbcdfcf7e0278d2c11111))
- *(protocol)* Inbound params pass through via params.extra - ([689614e](https://github.com/bitrouter/bitrouter/commit/689614e31d8c77fcf38942df8558f7ca1f60f80f))
- *(registry)* Recover sync and refresh catalog ([#969](https://github.com/bitrouter/bitrouter/pull/969)) - ([f252013](https://github.com/bitrouter/bitrouter/commit/f252013cafecfb7a18ead7accb7b148f99ad41e0))
- *(responses)* Include response creation time - ([e84ec0d](https://github.com/bitrouter/bitrouter/commit/e84ec0d8b04d109985d123474cf39ac516180467))
- *(responses)* Close continuation safety gaps - ([8028ef2](https://github.com/bitrouter/bitrouter/commit/8028ef24d2a0a732b25b1d619dc0b99114eebeb8))
- *(responses)* Require graceful reconciliation - ([82ed064](https://github.com/bitrouter/bitrouter/commit/82ed064f6f532304185fe4b49db637a6911cbc07))
- *(responses)* Fence continuation reconciliation - ([e964467](https://github.com/bitrouter/bitrouter/commit/e964467fc72104711f26fc7313025c2b7c391da3))
- *(responses)* Close continuation publication races - ([b8c96d4](https://github.com/bitrouter/bitrouter/commit/b8c96d4f547a10c2c4a89a9b10cc715bf9fa279c))
- *(responses)* Harden continuation publication - ([c3c35b3](https://github.com/bitrouter/bitrouter/commit/c3c35b38f26f546733827fb8abdc5bda0d5f5d96))
- *(responses)* Authorize continuation on delivery - ([2b2a5e1](https://github.com/bitrouter/bitrouter/commit/2b2a5e156e72c16a18c5b5564596a1390c80cba4))
- *(responses)* Harden continuation delivery - ([371aeb2](https://github.com/bitrouter/bitrouter/commit/371aeb24f3205b755c9393cd717c5cb7f1f07dab))
- *(responses)* Bind completed continuation - ([5d1b662](https://github.com/bitrouter/bitrouter/commit/5d1b66266d1b83bb496b4d6c6e8f9cd3cb9bd7f9))
- *(responses)* Persist gateway continuation - ([7623a0f](https://github.com/bitrouter/bitrouter/commit/7623a0f846b344798ef54c5d18b16416a7452607))
- *(responses)* Preserve native continuation - ([c297c29](https://github.com/bitrouter/bitrouter/commit/c297c29c799a4f2c3035e42eaa370827cc9c5a03))
- *(responses)* Stabilize stream identity - ([f05add8](https://github.com/bitrouter/bitrouter/commit/f05add8c89c90f6d5542033c983e99cb4ce37a9b))
- *(responses-sse)* Completed envelope carries output array + in_progress + tool lifecycle - ([1c23355](https://github.com/bitrouter/bitrouter/commit/1c2335568a1ef983b9371cfa1073214357fa529b))
- *(responses-sse)* Emit full output-item lifecycle (codex compat) - ([2fa73c6](https://github.com/bitrouter/bitrouter/commit/2fa73c60803234683af1cd6ae3b7c3e11c8e3626))
- *(routing)* Learn adequacy from settlement - ([a24bb9d](https://github.com/bitrouter/bitrouter/commit/a24bb9db2ef5d71ecd46fa9cf776a9c18959c8e2))
- *(sdk)* Surface Chat Completions SSE errors ([#948](https://github.com/bitrouter/bitrouter/pull/948)) - ([eb28fef](https://github.com/bitrouter/bitrouter/commit/eb28feffe748bf05bf668fd6eefc7e2bf89d234b))
- *(sdk)* Deduplicate server-tool stream usage ([#895](https://github.com/bitrouter/bitrouter/pull/895)) - ([4c01593](https://github.com/bitrouter/bitrouter/commit/4c01593e82aada654e024079026797da93bce7fb))
- *(sdk)* Preserve routed cache usage ([#893](https://github.com/bitrouter/bitrouter/pull/893)) - ([94a5ee9](https://github.com/bitrouter/bitrouter/commit/94a5ee9855be917b1a9d839cee24d8ee41271483))
- *(sdk)* Normalize routed stream usage ([#887](https://github.com/bitrouter/bitrouter/pull/887)) - ([b26635b](https://github.com/bitrouter/bitrouter/commit/b26635b5909e1d69a59a193aa520a9832e21726f))
- *(sdk)* Isolate protocol request extras ([#733](https://github.com/bitrouter/bitrouter/pull/733)) - ([dffc9a9](https://github.com/bitrouter/bitrouter/commit/dffc9a9efdd872e2e69605ccfae6f27b11d8bcd4))
- *(sdk)* Preserve upstream bad requests ([#716](https://github.com/bitrouter/bitrouter/pull/716)) - ([d6872ba](https://github.com/bitrouter/bitrouter/commit/d6872ba1ed73fd6d600b99c6c1a5adab063b699d))
- *(sdk)* Preserve streaming preflight errors ([#696](https://github.com/bitrouter/bitrouter/pull/696)) - ([24d9ecf](https://github.com/bitrouter/bitrouter/commit/24d9ecf51ce4343c9d97e33a00dedf4e82cffbaf))
- *(sdk)* Preserve token limits and upstream errors ([#692](https://github.com/bitrouter/bitrouter/pull/692)) - ([495b1e9](https://github.com/bitrouter/bitrouter/commit/495b1e9aa4de4e79f877ca2d7fda0cf88bffad65))
- *(sdk)* Pin V3 citations + faithful fidelity notes - ([688cdca](https://github.com/bitrouter/bitrouter/commit/688cdcadc64afd5f581302ed72f4c65971f40a9c))
- *(sdk)* UTF-8 boundary in deser preview + timeouts on discover - ([f1a4424](https://github.com/bitrouter/bitrouter/commit/f1a4424c2c5d110e5178038757f889f81a6d8f35))
- *(sdk)* Track detached settlements and drain on shutdown - ([6d3e395](https://github.com/bitrouter/bitrouter/commit/6d3e395e3fec8cca59cf0b9918fb4d9b5638a6ea))
- *(sdk)* Apply preset prompt-body overrides (003 §5.4) - ([635e391](https://github.com/bitrouter/bitrouter/commit/635e3917a9da940e707164fc0a791652321f2780))
- *(sdk)* Emit WWW-Authenticate on 402, Retry-After on 429 - ([3187bac](https://github.com/bitrouter/bitrouter/commit/3187bacc536558e7eb19547048118a8b50c435d6))
- *(sdk)* Consult FallbackPolicy when execution hooks observe - ([22896c9](https://github.com/bitrouter/bitrouter/commit/22896c9afaec0916d2d1c8dc4241d3d85931e76c))
- *(sdk)* Run discover_models on routing-table reload - ([02503ca](https://github.com/bitrouter/bitrouter/commit/02503ca91a3729563b6511a8e611bb7e6027e27b))
- *(sdk)* Protocol-shaped stream errors, parallel tool indices - ([dfc20cd](https://github.com/bitrouter/bitrouter/commit/dfc20cd6cbbb6324fab4e5a999c85d98bc3898c8))
- *(sdk/anthropic)* Fold cache buckets into prompt_tokens (Usage subset contract) ([#492](https://github.com/bitrouter/bitrouter/pull/492)) - ([5ee6921](https://github.com/bitrouter/bitrouter/commit/5ee6921a3125f6d2245338f23bff98640b71d1cc))
- *(source)* Valid anthropic pairing + gemini chunks - ([c5a4515](https://github.com/bitrouter/bitrouter/commit/c5a45158358f2775e1a8426cc2c65de9d8891bdf))
- *(stream)* Bill prompt tokens on client disconnect ([#537](https://github.com/bitrouter/bitrouter/pull/537)) - ([f0dc0f0](https://github.com/bitrouter/bitrouter/commit/f0dc0f0cddd55e79ff8304287ab5593f2fb8ccbd))
- *(streaming)* Make terminal settlement cancellation-safe - ([a4ce879](https://github.com/bitrouter/bitrouter/commit/a4ce879172df56d4230d0904e7285f4f29d80469))
- *(streaming)* Estimate zero usage frames - ([72fe056](https://github.com/bitrouter/bitrouter/commit/72fe05682e885e34b225028a2101520d5c3ddc47))
- *(test)* Synchronize permission replies and TUI frames ([#975](https://github.com/bitrouter/bitrouter/pull/975)) - ([80b94f6](https://github.com/bitrouter/bitrouter/commit/80b94f69d4071fdc54e570ff286b37e9dffaf486))
- *(tool-result)* Faithful Responses output + V3 gaps - ([ed83d4f](https://github.com/bitrouter/bitrouter/commit/ed83d4f03692501167e90cdfe8390b8e08899ac8))
- *(tools)* Valid responses degrade for unpaired MCP call - ([e732710](https://github.com/bitrouter/bitrouter/commit/e7327102eabbd343f1fbd5f20d1c9d1e935365b2))
- *(tools)* Lossless gemini tool choice + responses tools - ([e248e57](https://github.com/bitrouter/bitrouter/commit/e248e5741221ceef766391fc128b5b4a75a49775))
- *(trajectory)* Correct outbox delivery audit - ([f582224](https://github.com/bitrouter/bitrouter/commit/f58222498ba8325807234faa4aba5e20d3750ba5))
- Point stale surfaces at commands that exist ([#799](https://github.com/bitrouter/bitrouter/pull/799)) - ([4eccfe9](https://github.com/bitrouter/bitrouter/commit/4eccfe9b08cb2d607cb16c482d61c24ab48bc999))
- Harden provider fallback errors ([#755](https://github.com/bitrouter/bitrouter/pull/755)) - ([0b03e2a](https://github.com/bitrouter/bitrouter/commit/0b03e2a3ed7d0abbf0ddd3755b36f22def30aa38))
- Record canonical stream timing and OTEL latency ([#697](https://github.com/bitrouter/bitrouter/pull/697)) - ([639ec9b](https://github.com/bitrouter/bitrouter/commit/639ec9b3ed9f0548102108d49699d99a34cb2740))
- Preserve stream error settlement ([#656](https://github.com/bitrouter/bitrouter/pull/656)) - ([4e1df77](https://github.com/bitrouter/bitrouter/commit/4e1df77805a6f91a1607d44035ab1fea2e82d988))
- [**breaking**] Pre-1.0 security audit fixes + dead-code cleanup ([#523](https://github.com/bitrouter/bitrouter/pull/523)) - ([44ec022](https://github.com/bitrouter/bitrouter/commit/44ec02299a8df495ba4dbbb19408a39da9e3e55d))
- Openai-chat refusal + hardening pass - ([2477b6c](https://github.com/bitrouter/bitrouter/commit/2477b6c15e095148d60ea11e2a6b4d92011cea7e))
- Align with docs 004/007 — Pass on no-pricing, key sign - ([689fa6b](https://github.com/bitrouter/bitrouter/commit/689fa6b479df516ff315b4e6c30cc8dad147ef42))

### 🚜 Refactor

- *(acp)* [**breaking**] One ACP stack, from the engine to a flat chat loop ([#848](https://github.com/bitrouter/bitrouter/pull/848)) - ([4aa89a7](https://github.com/bitrouter/bitrouter/commit/4aa89a7a69e16ab4a3c20983198583bda5a68be0))
- *(acp)* Move the ACP proxy engine into bitrouter-sdk ([#793](https://github.com/bitrouter/bitrouter/pull/793)) - ([72636a2](https://github.com/bitrouter/bitrouter/commit/72636a2a011068d5f45c9e63c9665c209543a22f))
- *(ai)* [**breaking**] Extract model integration (WIP) ([#962](https://github.com/bitrouter/bitrouter/pull/962)) - ([5b63978](https://github.com/bitrouter/bitrouter/commit/5b639784a94ce35ab10c1b09a475bd112d50e1ff))
- *(auth)* Move bitrouter-auth → apps/bitrouter/src/auth/ - ([834d629](https://github.com/bitrouter/bitrouter/commit/834d629f6408172cccf1aae3e5930a553179d401))
- *(mcp)* Remove OSS origin server ([#913](https://github.com/bitrouter/bitrouter/pull/913)) - ([862a398](https://github.com/bitrouter/bitrouter/commit/862a398018309f79852c1395d75e0224c5b46047))
- *(policy)* Make selection trajectory-aware - ([a27548c](https://github.com/bitrouter/bitrouter/commit/a27548cecbe753d962ee668ee14c26c0152b94c5))
- *(routing)* Satisfy strict workspace clippy - ([e548910](https://github.com/bitrouter/bitrouter/commit/e548910244e989162bd048aa531c19f6e9b2976f))
- *(sdk)* [**breaking**] Move the OTLP renderer out of the SDK ([#851](https://github.com/bitrouter/bitrouter/pull/851)) - ([28fe8ef](https://github.com/bitrouter/bitrouter/commit/28fe8efd788b9070cbbc11ddddf4e59728f74a5f))
- *(sdk)* [**breaking**] Rename API protocols to spec names ([#504](https://github.com/bitrouter/bitrouter/pull/504)) - ([820ec67](https://github.com/bitrouter/bitrouter/commit/820ec675f56b7ae4b537829b4297fbee65c5274f))
- *(sdk)* Strip business types from bitrouter-sdk - ([4cd5e08](https://github.com/bitrouter/bitrouter/commit/4cd5e0857a544f37bea3a86bdabb7e77aafb979d))
- *(server-tools)* Move advisor/sub-agent into the SDK ([#576](https://github.com/bitrouter/bitrouter/pull/576)) - ([5ffe7b1](https://github.com/bitrouter/bitrouter/commit/5ffe7b13b0d808f45b82a70cc73f8450e5965114))
- Unify task-aware routing on agent_route/v1 ([#828](https://github.com/bitrouter/bitrouter/pull/828)) - ([ee4dbb2](https://github.com/bitrouter/bitrouter/commit/ee4dbb2e3930b5187828da6849af290d4e71882a))
- Remove attestation, fold plugins/ into crates/ ([#701](https://github.com/bitrouter/bitrouter/pull/701)) - ([e9896fa](https://github.com/bitrouter/bitrouter/commit/e9896fa321c29f753db3b76e4bd6d684d0307cf4))

### 📚 Documentation

- *(multimodal)* Clarify response-side output boundary - ([2a3db48](https://github.com/bitrouter/bitrouter/commit/2a3db486e466cbda7445738a4c2fa63c48832d95))
- *(protocol)* Cite spec sources + document MPP divergence - ([7135e71](https://github.com/bitrouter/bitrouter/commit/7135e71cfd275fedb2dba20f4a035657e39987ce))
- *(sdk)* Fix stale doc-comment refs after Phase 1 strip - ([64895de](https://github.com/bitrouter/bitrouter/commit/64895de05f280c4810109a336b6de53d418cce6c))
- *(sdk)* Pin drain_pending_settlements race contract - ([a109b7a](https://github.com/bitrouter/bitrouter/commit/a109b7a017d1380e380e282fc63368adecd2dd43))
- *(source)* Correct stale per-source metadata rationale - ([070fefc](https://github.com/bitrouter/bitrouter/commit/070fefc81e49872762d2bb7bab12193d853d5346))
- Harden Harbor EC2 network preflight ([#838](https://github.com/bitrouter/bitrouter/pull/838)) - ([9e05f3b](https://github.com/bitrouter/bitrouter/commit/9e05f3ba21fd2f802cdb96764ce0a188ce69b298))
- Post-OSS-cut cleanup of SDK doc + release-plz + e2e comments - ([15afe2f](https://github.com/bitrouter/bitrouter/commit/15afe2fa252126cf360c86a9ceacd73aab4d5dc8))

### 🎨 Styling

- Collapse nested if-let into let-chains (clippy collapsible_if) - ([6b53fbe](https://github.com/bitrouter/bitrouter/commit/6b53fbe66f055479d1ac7aeded4646ceb127e9d5))

### 🧪 Testing

- *(capabilities)* Lock advertise-only contract (web_search/logprobs not auto-detected) - ([0618e80](https://github.com/bitrouter/bitrouter/commit/0618e806ce4b4470a7fdcc1e168631e9aa6457b3))
- *(multimodal)* Cover gemini file-url output round-trip - ([1e1f48b](https://github.com/bitrouter/bitrouter/commit/1e1f48bcc39a87105759730b05f6ee8d5dd1582b))
- *(sdk)* Add 4x4 conversion matrix and bug regressions - ([f3ada80](https://github.com/bitrouter/bitrouter/commit/f3ada80aa59853a68e8ebec2654f8f8831be71f6))
- *(sdk)* Add Phase 1 pipeline exit-criteria tests - ([9ad1521](https://github.com/bitrouter/bitrouter/commit/9ad1521a52f70f0d4a9ce054a30ed2bdbe3669c9))
- Remove Python files and use Rust fixtures ([#979](https://github.com/bitrouter/bitrouter/pull/979)) - ([9b9a176](https://github.com/bitrouter/bitrouter/commit/9b9a176288896809040a6fce11c92d146a4f5909))

### ⚙️ Miscellaneous Tasks

- Sync registry catalog ([#983](https://github.com/bitrouter/bitrouter/pull/983)) - ([54c822a](https://github.com/bitrouter/bitrouter/commit/54c822a34d4dbfd75d484ca35e108b497048efef))
- Sync registry catalog ([#982](https://github.com/bitrouter/bitrouter/pull/982)) - ([eafa838](https://github.com/bitrouter/bitrouter/commit/eafa838383dcdfa3bd79a74a5be3d3c87c056d72))
- Sync registry catalog ([#980](https://github.com/bitrouter/bitrouter/pull/980)) - ([2fd95b1](https://github.com/bitrouter/bitrouter/commit/2fd95b16aa69977d9c4d9bcf581519b505c332fb))
- Sync registry catalog ([#972](https://github.com/bitrouter/bitrouter/pull/972)) - ([88c299e](https://github.com/bitrouter/bitrouter/commit/88c299e6c2217f34e481740b2ed8adf16d982203))
- Sync registry catalog ([#954](https://github.com/bitrouter/bitrouter/pull/954)) - ([93a4d67](https://github.com/bitrouter/bitrouter/commit/93a4d676ec928a935b79f5ed6201b7f8d26a36b7))
- Sync registry catalog ([#950](https://github.com/bitrouter/bitrouter/pull/950)) - ([d93ed73](https://github.com/bitrouter/bitrouter/commit/d93ed73be2411992cc44e3234b6e7b11df1effb2))
- Sync registry catalog ([#944](https://github.com/bitrouter/bitrouter/pull/944)) - ([7a8643c](https://github.com/bitrouter/bitrouter/commit/7a8643cde84a49f582d51c281179cef6f0ac01c4))
- Sync registry catalog ([#943](https://github.com/bitrouter/bitrouter/pull/943)) - ([d7abbb4](https://github.com/bitrouter/bitrouter/commit/d7abbb42a536fcb1a00210cc20ebb67aa455c8bd))
- Merge latest main - ([aa10285](https://github.com/bitrouter/bitrouter/commit/aa1028591bceadbd7f57e6627e8a4dcd3b1bb3c1))
- Refresh Rust dependencies and MSRV ([#699](https://github.com/bitrouter/bitrouter/pull/699)) - ([f7ad755](https://github.com/bitrouter/bitrouter/commit/f7ad75509b5a7195f279f93dbcad0e66c52e0a60))
- Migrate registry into OSS dist ([#636](https://github.com/bitrouter/bitrouter/pull/636)) - ([afdb4b7](https://github.com/bitrouter/bitrouter/commit/afdb4b7f68723456efa804e1d2760ed103a59a84))
- Fix doc / msrv / feature-isolation jobs for the v1 layout - ([4455398](https://github.com/bitrouter/bitrouter/commit/4455398ed3d79c8773e3acb2ec9d628ca14d1f9b))
- Migrate release automation from v0 + drop v0-only script - ([e40cb92](https://github.com/bitrouter/bitrouter/commit/e40cb9295339238db7435110c3a2f8c30790e6f7))
- Drop internal refs + comprehensive rustdoc for the SDK - ([0fa1c42](https://github.com/bitrouter/bitrouter/commit/0fa1c426dd704ff43736deecd808349513d53fcb))
- Scaffold v1 workspace, crates, plugins, apps - ([7b84dc0](https://github.com/bitrouter/bitrouter/commit/7b84dc0dd42ae8f3ad3fe775a5191641fbfbad60))

### Hardening

- Regex size limit, body limit, wire auto_discover - ([3a4d292](https://github.com/bitrouter/bitrouter/commit/3a4d2926288ac9ed4c22e1e2bc4ea4325b4b0bbb))


### Added

- Native BRO ACP v1 and draft v2 via bare `bro acp serve --model MODEL`, backed
  by the daemon's existing ThreadService. Unexpected disconnect preserves work
  and pending approval; cancel retains a paused queue, while close durably
  cancels every accepted input and waits for native cleanup. Runtime format 7
  preserves 2/3/4/5/6 reads. Explicit external-agent proxy commands retain their path.

### Changed

- Upgrade ACP schema to 1.10.2 with the matching SDK and Conductor 3.0.0,
  retaining stable v1 and draft v2 support.

- **Breaking (SDK):** Move the ACP controller, client, capture, telemetry,
  translation and process transport to `bitrouter_orchestrator::acp` behind
  its `acp` feature.
  Remove the SDK's `acp` module and feature. Agent configuration types now live
  in `bitrouter_sdk::config::agent`; existing `agents:` YAML, ACP wire behavior
  and CLI commands are unchanged.

- **Breaking:** remove native Gemini Generate Content HTTP endpoints, upstream codec, and Rust protocol variant. Metered `google` now uses the official Chat Completions endpoint with bearer `GEMINI_API_KEY`.
- Retire `google-ai` subscription inference, Vertex Express, and bundled Gemini CLI routing. Active native configs fail with migration guidance; older catalogs quarantine retired entries. Saved credentials and historical records remain intact, with unsupported replay rejected. No automatic subscription-to-metered billing migration occurs.
- Google Chat tool signatures carry credential-bound replay proof; clients must preserve each tool call’s `extra_content` in subsequent requests. Explicit cache references and unverified constraints are rejected. Replacement live-provider and SDK evidence is tracked in `docs/GEMINI_PROTOCOL_RETIREMENT_PROGRESS.md`.

## [1.0.0-alpha.33](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.32...v1.0.0-alpha.33)


### ⛰️ Features

- *(code)* Make slash launcher the primary control ([#935](https://github.com/bitrouter/bitrouter/pull/935)) - ([343d8f9](https://github.com/bitrouter/bitrouter/commit/343d8f984838d0eee3699910ad9a4a5cc4012f1b))
- *(extensions)* [**breaking**] Add native request-check host ([#923](https://github.com/bitrouter/bitrouter/pull/923)) - ([750d618](https://github.com/bitrouter/bitrouter/commit/750d618ad4235adefaaa33e4d864ef61169440fd))

### ⚙️ Miscellaneous Tasks

- Sync registry catalog ([#937](https://github.com/bitrouter/bitrouter/pull/937)) - ([27574c5](https://github.com/bitrouter/bitrouter/commit/27574c5fea312035cacea5ad2db6a5081bc007ec))


## [1.0.0-alpha.32](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.31...v1.0.0-alpha.32)


### ⛰️ Features

- *(router)* Add named routers and preset migration ([#916](https://github.com/bitrouter/bitrouter/pull/916)) - ([022f8aa](https://github.com/bitrouter/bitrouter/commit/022f8aa3de2d75d6567b55e08fee405f41ba93a5))
- *(tui)* Add supervised background agent control deck ([#927](https://github.com/bitrouter/bitrouter/pull/927)) - ([4ac3d62](https://github.com/bitrouter/bitrouter/commit/4ac3d620a0ac0ae7377095cde742b1e60b4a32b3))
- Add router request checks and receipts ([#919](https://github.com/bitrouter/bitrouter/pull/919)) - ([2b01d2e](https://github.com/bitrouter/bitrouter/commit/2b01d2e6eab72274fb4b3571534fb5c9e656746b))

### 🐛 Bug Fixes

- *(tui)* Cancel deck jobs before exit cleanup ([#928](https://github.com/bitrouter/bitrouter/pull/928)) - ([6c94e60](https://github.com/bitrouter/bitrouter/commit/6c94e6013f3a5699232a4aaad544d49fb9c9472a))
- Make saved and running config state explicit ([#918](https://github.com/bitrouter/bitrouter/pull/918)) - ([78b6911](https://github.com/bitrouter/bitrouter/commit/78b69112c70bf9fe1f78cb4104790f7faba115ec))

### ⚙️ Miscellaneous Tasks

- Sync registry catalog ([#929](https://github.com/bitrouter/bitrouter/pull/929)) - ([cea232d](https://github.com/bitrouter/bitrouter/commit/cea232dc73b3f676f06f2e9800d59642633cc352))
- Sync registry catalog ([#920](https://github.com/bitrouter/bitrouter/pull/920)) - ([9dabc10](https://github.com/bitrouter/bitrouter/commit/9dabc10e6d414db9698ee3a7561ead9482bfc1e9))
- Sync registry catalog ([#915](https://github.com/bitrouter/bitrouter/pull/915)) - ([1521a60](https://github.com/bitrouter/bitrouter/commit/1521a6063afc79ef8ca534f6dec82936ac943196))


## [1.0.0-alpha.31](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.30...v1.0.0-alpha.31)


### ⛰️ Features

- *(acp)* Add checkpoints and assessment revisions ([#903](https://github.com/bitrouter/bitrouter/pull/903)) - ([ad2bab7](https://github.com/bitrouter/bitrouter/commit/ad2bab71968e40752fe175433b23fd2098307c81))
- *(acp)* Persist canonical session trajectories ([#902](https://github.com/bitrouter/bitrouter/pull/902)) - ([17116a2](https://github.com/bitrouter/bitrouter/commit/17116a25e58c15f5965e2072be80de9436e36748))
- *(cli)* [**breaking**] Rename the invocation to `bro` - ([23cc164](https://github.com/bitrouter/bitrouter/commit/23cc164452fe844b04919aeb93e05a83286d775c))
- *(config)* Add provider header policies ([#910](https://github.com/bitrouter/bitrouter/pull/910)) - ([9691d84](https://github.com/bitrouter/bitrouter/commit/9691d849f4bf9813d412491e20d6c97419e7d600))
- *(control)* Add remote router administration ([#901](https://github.com/bitrouter/bitrouter/pull/901)) - ([a88ce8e](https://github.com/bitrouter/bitrouter/commit/a88ce8e2d6fba3e2c4359287d22e046224eb9616))
- *(evolution)* Add checkpoint TS routing ([#907](https://github.com/bitrouter/bitrouter/pull/907)) - ([94a3fb3](https://github.com/bitrouter/bitrouter/commit/94a3fb3d7103ad96cfdb2a43a2d8c68a9def68d7))
- *(mcp)* Add native skills gateway support ([#912](https://github.com/bitrouter/bitrouter/pull/912)) - ([b1cd8f3](https://github.com/bitrouter/bitrouter/commit/b1cd8f334b68999df7354071327afcc4c8188592))
- *(registry)* Automate canonical model lifecycle ([#876](https://github.com/bitrouter/bitrouter/pull/876)) - ([34fb604](https://github.com/bitrouter/bitrouter/commit/34fb60465975a99c8c4d28822cbefb88ec52d4f1))
- *(tui)* Adopt native scrollback ([#904](https://github.com/bitrouter/bitrouter/pull/904)) - ([b8ed285](https://github.com/bitrouter/bitrouter/commit/b8ed2854197a4a3d3dfb8ea4b20ff110420828bb))
- *(tui)* Replace dashboard with conversation - ([6589188](https://github.com/bitrouter/bitrouter/commit/65891881df6b4584a154955bb05c80cc29427351))
- *(tui)* Improve scrollback and transcript rendering ([#899](https://github.com/bitrouter/bitrouter/pull/899)) - ([a59bd6b](https://github.com/bitrouter/bitrouter/commit/a59bd6b3fe1537138d1ae88ddda1839b3673dfa9))

### 🐛 Bug Fixes

- *(codex)* Preserve ACP tools through Responses ([#892](https://github.com/bitrouter/bitrouter/pull/892)) - ([6f10d52](https://github.com/bitrouter/bitrouter/commit/6f10d52808ebf95ba98969f2652a998b13841c5e))
- *(update)* Delegate managed installs ([#898](https://github.com/bitrouter/bitrouter/pull/898)) - ([72384d2](https://github.com/bitrouter/bitrouter/commit/72384d2926b3c573eb5c4f380fa3e458430d074a))

### 🚜 Refactor

- *(mcp)* Remove OSS origin server ([#913](https://github.com/bitrouter/bitrouter/pull/913)) - ([862a398](https://github.com/bitrouter/bitrouter/commit/862a398018309f79852c1395d75e0224c5b46047))

### 📚 Documentation

- Correct MCP spend report command ([#911](https://github.com/bitrouter/bitrouter/pull/911)) - ([9b5e581](https://github.com/bitrouter/bitrouter/commit/9b5e581dbb95f0cd3b888303433d722b87f0bc01))

### ⚙️ Miscellaneous Tasks

- Sync registry catalog ([#914](https://github.com/bitrouter/bitrouter/pull/914)) - ([f1f29db](https://github.com/bitrouter/bitrouter/commit/f1f29db0d78bcd20669558f304c30a28e1c34a80))


- **Breaking (MCP): remove the OSS first-party origin server.** The
  `bitrouter-mcp` crate, `bro mcp serve`, `bro mcp install`, daemon
  `/mcp-control` endpoint, and automatic `bitrouter_skills` injection are
  removed. Local agents operate BitRouter through the shipped Agent Skill and
  structured `bro` CLI. The upstream MCP client/gateway, aggregate `/mcp`,
  Skills-over-MCP relay/cache, server-side tool loop, and `bro mcp check`
  remain.

- **Breaking (CLI): the invocation is now `bro`.** `bitrouter <cmd>` becomes
  `bro <cmd>`. Only the invocation changed — the product, crate, npm package,
  Homebrew tap, `bitrouter.yaml`, `~/.bitrouter`, `BITROUTER_*` environment
  variables, `brk_` key prefix, and telemetry identifiers keep their names.
  Shell, PowerShell, npm, and Homebrew installers additionally create a
  deprecated `bitrouter` alias; invoking it prints one migration note on
  stderr while preserving structured stdout. The alias is installer-only,
  release archives contain `bro`, and the alias is removed in 1.0.0.

## [1.0.0-alpha.30](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.29...v1.0.0-alpha.30)


### ⛰️ Features

- *(cli)* Unify ACP lifecycle surfaces ([#885](https://github.com/bitrouter/bitrouter/pull/885)) - ([63b05b8](https://github.com/bitrouter/bitrouter/commit/63b05b874db10ba29fb73d3c923e99a5c329757d))

### 🐛 Bug Fixes

- *(sdk)* Deduplicate server-tool stream usage ([#895](https://github.com/bitrouter/bitrouter/pull/895)) - ([4c01593](https://github.com/bitrouter/bitrouter/commit/4c01593e82aada654e024079026797da93bce7fb))


## [1.0.0-alpha.29](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.28...v1.0.0-alpha.29)


### ⛰️ Features

- *(cli)* Complete searchable ACP onboarding ([#888](https://github.com/bitrouter/bitrouter/pull/888)) - ([ef2c13a](https://github.com/bitrouter/bitrouter/commit/ef2c13a8185cc736f86c7837d86e2ed9bc22325a))

### 🐛 Bug Fixes

- *(sdk)* Preserve routed cache usage ([#893](https://github.com/bitrouter/bitrouter/pull/893)) - ([94a5ee9](https://github.com/bitrouter/bitrouter/commit/94a5ee9855be917b1a9d839cee24d8ee41271483))


## [1.0.0-alpha.28](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.27...v1.0.0-alpha.28)


### ⛰️ Features

- *(acp)* Headless permission policy and output formats ([#866](https://github.com/bitrouter/bitrouter/pull/866)) - ([878178a](https://github.com/bitrouter/bitrouter/commit/878178a45d84a805d0ae1a489ecf60f79f575a6a))
- *(acp)* Read harness authentication from the protocol ([#860](https://github.com/bitrouter/bitrouter/pull/860)) - ([205f5d1](https://github.com/bitrouter/bitrouter/commit/205f5d149bb01bd5b601aa20d7064209fe5b379f))
- *(acp)* Harden v1 controller routing ([#853](https://github.com/bitrouter/bitrouter/pull/853)) - ([d97924e](https://github.com/bitrouter/bitrouter/commit/d97924e78c7dab7c6fdf93c779014583e079b97e))
- *(acp)* Add controller identity and session routing ([#852](https://github.com/bitrouter/bitrouter/pull/852)) - ([d392008](https://github.com/bitrouter/bitrouter/commit/d3920084d0f8ddc8ac0d9ef1d1994e2b2d2b9908))
- *(acp)* Add multi-session controller ([#849](https://github.com/bitrouter/bitrouter/pull/849)) - ([d6e9327](https://github.com/bitrouter/bitrouter/commit/d6e93272a70a3ecc6b807848a4447188296f6513))
- *(acp)* Rebuild the ACP agent surface and add the chat TUI ([#816](https://github.com/bitrouter/bitrouter/pull/816)) - ([f3f86ab](https://github.com/bitrouter/bitrouter/commit/f3f86ab2d47bf560a85a0ca57055458958512914))
- *(actions)* [**breaking**] One table for status, list_models, route ([#869](https://github.com/bitrouter/bitrouter/pull/869)) - ([956f394](https://github.com/bitrouter/bitrouter/commit/956f39489456dec937393b204d44c039157fc68d))
- *(chat)* Resolve slash commands against the actions table ([#880](https://github.com/bitrouter/bitrouter/pull/880)) - ([61dd773](https://github.com/bitrouter/bitrouter/commit/61dd77333a91f7aab01647b0ae7d8625d3039ffe))
- *(cli)* Operate trajectory history - ([87695fb](https://github.com/bitrouter/bitrouter/commit/87695fb5a60715cfbc657b58a4b960155c49065d))
- *(eval)* Publish trajectory operations - ([3c250db](https://github.com/bitrouter/bitrouter/commit/3c250db59629e95b365bcdbe3f3877a165ddfabc))
- *(eval)* Add generic route evaluation workflow ([#763](https://github.com/bitrouter/bitrouter/pull/763)) - ([4b86a51](https://github.com/bitrouter/bitrouter/commit/4b86a5134d5c5e239c5b4ae8097f9da243ec2de7))
- *(eval)* Make policy locks the routing source ([#762](https://github.com/bitrouter/bitrouter/pull/762)) - ([ef3fb69](https://github.com/bitrouter/bitrouter/commit/ef3fb6993605351dabc635e5a7bab89ca4937907))
- *(launch)* Route every catalog harness ([#781](https://github.com/bitrouter/bitrouter/pull/781)) - ([5cefa6d](https://github.com/bitrouter/bitrouter/commit/5cefa6da240cddbd5763f6eb04ec3b4750125798))
- *(mcp)* [**breaking**] Drop the complete tool from the origin server ([#875](https://github.com/bitrouter/bitrouter/pull/875)) - ([be7e21c](https://github.com/bitrouter/bitrouter/commit/be7e21c8209040ec4b866e0a7d1c23181bd6d5b8))
- *(mcp)* Upgrade to stable rmcp 3.1 for MCP 2026-07-28 ([#760](https://github.com/bitrouter/bitrouter/pull/760)) - ([5536fe5](https://github.com/bitrouter/bitrouter/commit/5536fe525829ec51d373ac611ae89d1fed449dfc))
- *(observability)* [**breaking**] Live router view and hosted launch ([#803](https://github.com/bitrouter/bitrouter/pull/803)) - ([25f69b7](https://github.com/bitrouter/bitrouter/commit/25f69b71f5660253c7ee46874b460370e954060d))
- *(optimize)* Add agentic workflow loop ([#777](https://github.com/bitrouter/bitrouter/pull/777)) - ([6b0e73e](https://github.com/bitrouter/bitrouter/commit/6b0e73e5c68992eda45d755a799b24cfde2a3351))
- *(policy)* Route models by reasoning effort ([#804](https://github.com/bitrouter/bitrouter/pull/804)) - ([fa402d4](https://github.com/bitrouter/bitrouter/commit/fa402d489ace1ecd7d7f5ac5bc01577b35ea8958))
- *(policy)* Route agent workflows by predicted role ([#790](https://github.com/bitrouter/bitrouter/pull/790)) - ([05fedf9](https://github.com/bitrouter/bitrouter/commit/05fedf9437a0d3c7beacd517b69c5fc590d47777))
- *(policy)* Add trajectory progress guard - ([8f03e48](https://github.com/bitrouter/bitrouter/commit/8f03e4801caa74d2dbb7fe40161b2c71baa91733))
- *(policy)* Harden cost-aware workflow routing ([#768](https://github.com/bitrouter/bitrouter/pull/768)) - ([185c5a1](https://github.com/bitrouter/bitrouter/commit/185c5a1aa792c63990e5bba78e462104e1e42fe6))
- *(policy)* Generalize agent trace routing ([#757](https://github.com/bitrouter/bitrouter/pull/757)) - ([2722d77](https://github.com/bitrouter/bitrouter/commit/2722d77bc9f6d184e6dd37e5cd769ac48de93b70))
- *(registry)* Add agents and runtimes as a primitive pair ([#879](https://github.com/bitrouter/bitrouter/pull/879)) - ([f8c09b1](https://github.com/bitrouter/bitrouter/commit/f8c09b154256b704082aa20d89de1380b07e74f1))
- *(router)* Add shadow classifier bakeoff ([#865](https://github.com/bitrouter/bitrouter/pull/865)) - ([d77d52d](https://github.com/bitrouter/bitrouter/commit/d77d52d50969f8a6a6c4d9961827601b4782620a))
- *(router)* Freeze routing measurement ([#862](https://github.com/bitrouter/bitrouter/pull/862)) - ([c2f7c78](https://github.com/bitrouter/bitrouter/commit/c2f7c783122b3f70b02f797622ca0fef09784bce))
- *(skills)* [**breaking**] Unify discovery and conform to SEP-2640 ([#870](https://github.com/bitrouter/bitrouter/pull/870)) - ([7ee79ea](https://github.com/bitrouter/bitrouter/commit/7ee79eab35700d02bd3b6e795b0f5aafab36a64d))
- *(skills)* [**breaking**] Serve over MCP (SEP-2640), cut installer ([#770](https://github.com/bitrouter/bitrouter/pull/770)) - ([afeb3ab](https://github.com/bitrouter/bitrouter/commit/afeb3ab50d663199843cc1984fbfc0bf4de02814))
- *(substrate)* Detach/reattach session robustness ([#743](https://github.com/bitrouter/bitrouter/pull/743)) - ([9fb7f92](https://github.com/bitrouter/bitrouter/commit/9fb7f92a588608c424e73ee5dea28d1475244fc9))
- *(trajectory)* Reduce replayable health - ([53114b4](https://github.com/bitrouter/bitrouter/commit/53114b404e66202aa35340db4a547cd4c5e75bdd))
- *(trajectory)* Add durable event ledger - ([41ef9ad](https://github.com/bitrouter/bitrouter/commit/41ef9ad3a892df40c5ebf47e6b38555b98318171))
- Optimize routes from observed history ([#833](https://github.com/bitrouter/bitrouter/pull/833)) - ([9fa7a4c](https://github.com/bitrouter/bitrouter/commit/9fa7a4c9973164bf2cb9b5f7339476b365888254))
- Publish bitrouter/auto as the router slug ([#788](https://github.com/bitrouter/bitrouter/pull/788)) - ([8b702eb](https://github.com/bitrouter/bitrouter/commit/8b702eb9376ea10025e5d4f1643a23e14f9e4565))
- Guide @auto workflow optimization ([#779](https://github.com/bitrouter/bitrouter/pull/779)) - ([bbca8ec](https://github.com/bitrouter/bitrouter/commit/bbca8eca2185bc036fb3d6ab3974dc1f07cfca59))
- Add MCP registry discovery (mcp search/list/add) ([#759](https://github.com/bitrouter/bitrouter/pull/759)) - ([6096874](https://github.com/bitrouter/bitrouter/commit/60968740e275e2ee84cc42f94dd184a775592d64))
- Add cache-aware routing, Terminus 2 identity, and Claude bridge ([#717](https://github.com/bitrouter/bitrouter/pull/717)) - ([0a4f3ee](https://github.com/bitrouter/bitrouter/commit/0a4f3ee9b93958a3f6fc9725c1dbad334545a8f7))

### 🐛 Bug Fixes

- *(auth)* Preserve fallback semantics - ([6f6185d](https://github.com/bitrouter/bitrouter/commit/6f6185d13522d3f496c9d54e7b72d7a035e5f657))
- *(auth)* Redact extension failures - ([1a41c85](https://github.com/bitrouter/bitrouter/commit/1a41c85206327c76982e479880e393bf67cc14e4))
- *(chat)* Show the session log when a launch fails ([#857](https://github.com/bitrouter/bitrouter/pull/857)) - ([6ca6e60](https://github.com/bitrouter/bitrouter/commit/6ca6e60066c1e0bb8955b3a65a544bb49cb650b8))
- *(cli)* Onboard into the default ACP TUI ([#886](https://github.com/bitrouter/bitrouter/pull/886)) - ([d357ee9](https://github.com/bitrouter/bitrouter/commit/d357ee9a0370238ceb0ad4a6b6ba006a966b30da))
- *(cli)* Harden trajectory operations - ([9ea8d4a](https://github.com/bitrouter/bitrouter/commit/9ea8d4a314880cdbef4eeafbe87a1de48b18b9cc))
- *(config)* Make server.log_level set the log filter ([#806](https://github.com/bitrouter/bitrouter/pull/806)) - ([6c90156](https://github.com/bitrouter/bitrouter/commit/6c9015690342fbae8d661aa8676fea149d7cb0a9))
- *(daemon)* Authenticate restart kill target - ([ba3a5a4](https://github.com/bitrouter/bitrouter/commit/ba3a5a45c4d86b2c275a6bde1c7ba52aac5695cf))
- *(daemon)* Await restart process exit - ([159270e](https://github.com/bitrouter/bitrouter/commit/159270e0d3f5dd5670d6972c66e61c997461eb16))
- *(db)* Normalize anchored SQLite URLs on Windows - ([7193cd9](https://github.com/bitrouter/bitrouter/commit/7193cd92c403034a1e1c5d9f78b4f77a4b124358))
- *(eval)* Preserve unguarded policy subjects - ([2fffd8f](https://github.com/bitrouter/bitrouter/commit/2fffd8f7d311e97c33017b1eb5e06eab63010a85))
- *(eval)* Scope trajectory decision credit - ([9bb038f](https://github.com/bitrouter/bitrouter/commit/9bb038fcf77607f1dec3eb4f733ac4c129462ff3))
- *(launch)* Say `launch:` not `spawn:` in console output ([#798](https://github.com/bitrouter/bitrouter/pull/798)) - ([ea92378](https://github.com/bitrouter/bitrouter/commit/ea923785ad598dd853fd95778f74a4cc296fbf74))
- *(pipeline)* Settle routing failures - ([3db74eb](https://github.com/bitrouter/bitrouter/commit/3db74ebabb75c545c868adcfaedc69923edfe1df))
- *(policy)* Bind progress guard evidence - ([fcbbacc](https://github.com/bitrouter/bitrouter/commit/fcbbacc1bbb792b798773a5d169420a95dbfaa79))
- *(policy)* Make runtime mode authoritative ([#761](https://github.com/bitrouter/bitrouter/pull/761)) - ([a44a6c8](https://github.com/bitrouter/bitrouter/commit/a44a6c87f0a9da357d44eb5cca57e70914c8381c))
- *(protocol)* Preserve Anthropic tool extension fields ([#774](https://github.com/bitrouter/bitrouter/pull/774)) - ([119cd0f](https://github.com/bitrouter/bitrouter/commit/119cd0f0935d9d77beeae9d8671b14c817bb1a96))
- *(responses)* Budget shutdown drain per batch - ([5b13538](https://github.com/bitrouter/bitrouter/commit/5b13538fe66c3894c59120bbcb5f111c7fb89f6e))
- *(responses)* Drain after slow reconciler shutdown - ([06b25fa](https://github.com/bitrouter/bitrouter/commit/06b25fad4b1012ee17dce6ec96a150f28a49b523))
- *(responses)* Close continuation safety gaps - ([8028ef2](https://github.com/bitrouter/bitrouter/commit/8028ef24d2a0a732b25b1d619dc0b99114eebeb8))
- *(responses)* Require graceful reconciliation - ([82ed064](https://github.com/bitrouter/bitrouter/commit/82ed064f6f532304185fe4b49db637a6911cbc07))
- *(responses)* Fence continuation reconciliation - ([e964467](https://github.com/bitrouter/bitrouter/commit/e964467fc72104711f26fc7313025c2b7c391da3))
- *(responses)* Bound continuation ownership state - ([3a044ce](https://github.com/bitrouter/bitrouter/commit/3a044ce11b76dbbcb2dd74e8efd2447c227d9316))
- *(responses)* Close continuation publication races - ([b8c96d4](https://github.com/bitrouter/bitrouter/commit/b8c96d4f547a10c2c4a89a9b10cc715bf9fa279c))
- *(responses)* Harden continuation publication - ([c3c35b3](https://github.com/bitrouter/bitrouter/commit/c3c35b38f26f546733827fb8abdc5bda0d5f5d96))
- *(responses)* Authorize continuation on delivery - ([2b2a5e1](https://github.com/bitrouter/bitrouter/commit/2b2a5e156e72c16a18c5b5564596a1390c80cba4))
- *(responses)* Harden continuation delivery - ([371aeb2](https://github.com/bitrouter/bitrouter/commit/371aeb24f3205b755c9393cd717c5cb7f1f07dab))
- *(responses)* Bind completed continuation - ([5d1b662](https://github.com/bitrouter/bitrouter/commit/5d1b66266d1b83bb496b4d6c6e8f9cd3cb9bd7f9))
- *(responses)* Persist gateway continuation - ([7623a0f](https://github.com/bitrouter/bitrouter/commit/7623a0f846b344798ef54c5d18b16416a7452607))
- *(responses)* Preserve native continuation - ([c297c29](https://github.com/bitrouter/bitrouter/commit/c297c29c799a4f2c3035e42eaa370827cc9c5a03))
- *(responses)* Stabilize stream identity - ([f05add8](https://github.com/bitrouter/bitrouter/commit/f05add8c89c90f6d5542033c983e99cb4ce37a9b))
- *(responses)* Include response creation time - ([e84ec0d](https://github.com/bitrouter/bitrouter/commit/e84ec0d8b04d109985d123474cf39ac516180467))
- *(router)* Correct research measurement reports ([#874](https://github.com/bitrouter/bitrouter/pull/874)) - ([4fcd801](https://github.com/bitrouter/bitrouter/commit/4fcd8018d8c6141a2e08105bad894d33c8298900))
- *(sdk)* Normalize routed stream usage ([#887](https://github.com/bitrouter/bitrouter/pull/887)) - ([b26635b](https://github.com/bitrouter/bitrouter/commit/b26635b5909e1d69a59a193aa520a9832e21726f))
- *(tests)* Escape Windows database URLs in YAML - ([1b5a29f](https://github.com/bitrouter/bitrouter/commit/1b5a29f07441292ed4b48e2a587121df742bc877))
- *(tests)* Drain settlements before restart assertions - ([a621140](https://github.com/bitrouter/bitrouter/commit/a621140dbefa8521b89a9c8ba74cbb1362e21036))
- *(trajectory)* Hold continuation starts at episode head ([#829](https://github.com/bitrouter/bitrouter/pull/829)) - ([01d8892](https://github.com/bitrouter/bitrouter/commit/01d88924a38e07b1e4511c344421b036bbd0b741))
- *(trajectory)* Align start and settlement clocks - ([8c63b43](https://github.com/bitrouter/bitrouter/commit/8c63b439d77df5c371ac67cfef70cc95e1cb839b))
- *(trajectory)* Count recovery transitions - ([8738aaa](https://github.com/bitrouter/bitrouter/commit/8738aaacde94b4ef7799b107154d5f60180edcb8))
- *(trajectory)* Preserve truncated ancestry honesty - ([4677b61](https://github.com/bitrouter/bitrouter/commit/4677b61cb6789fae2ab4d0f13ee57dcf659f31f0))
- *(trajectory)* Correct outbox delivery audit - ([f582224](https://github.com/bitrouter/bitrouter/commit/f58222498ba8325807234faa4aba5e20d3750ba5))
- *(trajectory)* Bind reused evaluation - ([970952d](https://github.com/bitrouter/bitrouter/commit/970952d68cdeefc3d5ca27d884fa3deb0d1e088a))
- *(trajectory)* Stabilize failed retries - ([9bff7c7](https://github.com/bitrouter/bitrouter/commit/9bff7c7fbe7445f1cf9f4408401649be761c491d))
- *(trajectory)* Expose ancestry conflicts - ([2dc8559](https://github.com/bitrouter/bitrouter/commit/2dc8559fa525eb1da1e57557fe92ceac6d872189))
- *(trajectory)* Separate hold from tier selection - ([3625eb4](https://github.com/bitrouter/bitrouter/commit/3625eb4d6713da9b78720906106465b40aa4160f))
- *(trajectory)* Validate initial episode start - ([e5c8b71](https://github.com/bitrouter/bitrouter/commit/e5c8b715f84d08def66eb7b1e5a5ad7af2330b7b))
- *(trajectory)* Enforce replay lifecycle - ([8f57f0a](https://github.com/bitrouter/bitrouter/commit/8f57f0a2bb6143bc263b830d7e79d10c03641535))
- *(trajectory)* Bind native evidence key - ([779aa83](https://github.com/bitrouter/bitrouter/commit/779aa83ad1f6c2db14efa4819a972f3d9313920f))
- *(trajectory)* Close correlation edge cases - ([d0bc7ff](https://github.com/bitrouter/bitrouter/commit/d0bc7ff27df03db0391d0ce018dc71f75c1aed6d))
- *(trajectory)* Harden correlation runtime - ([d6700ca](https://github.com/bitrouter/bitrouter/commit/d6700cad3d787b7740d1abe595ce05dc1a6d190a))
- *(trajectory)* Harden ledger persistence - ([1316b40](https://github.com/bitrouter/bitrouter/commit/1316b403ab8242f97e9793ec95222e235b2e15bc))
- Point stale surfaces at commands that exist ([#799](https://github.com/bitrouter/bitrouter/pull/799)) - ([4eccfe9](https://github.com/bitrouter/bitrouter/commit/4eccfe9b08cb2d607cb16c482d61c24ab48bc999))
- Harden provider fallback errors ([#755](https://github.com/bitrouter/bitrouter/pull/755)) - ([0b03e2a](https://github.com/bitrouter/bitrouter/commit/0b03e2a3ed7d0abbf0ddd3755b36f22def30aa38))

### 🚜 Refactor

- *(acp)* [**breaking**] One ACP stack, from the engine to a flat chat loop ([#848](https://github.com/bitrouter/bitrouter/pull/848)) - ([4aa89a7](https://github.com/bitrouter/bitrouter/commit/4aa89a7a69e16ab4a3c20983198583bda5a68be0))
- *(acp)* Move the ACP proxy engine into bitrouter-sdk ([#793](https://github.com/bitrouter/bitrouter/pull/793)) - ([72636a2](https://github.com/bitrouter/bitrouter/commit/72636a2a011068d5f45c9e63c9665c209543a22f))
- *(docs)* Consolidate internal specs under docs/ ([#750](https://github.com/bitrouter/bitrouter/pull/750)) - ([b83775d](https://github.com/bitrouter/bitrouter/commit/b83775da9b67d0029bc6c17be76901e8effca823))
- *(policy)* Make selection trajectory-aware - ([a27548c](https://github.com/bitrouter/bitrouter/commit/a27548cecbe753d962ee668ee14c26c0152b94c5))
- *(sdk)* [**breaking**] Move the OTLP renderer out of the SDK ([#851](https://github.com/bitrouter/bitrouter/pull/851)) - ([28fe8ef](https://github.com/bitrouter/bitrouter/commit/28fe8efd788b9070cbbc11ddddf4e59728f74a5f))
- *(substrate)* Drop detach/reattach + fleet machinery ([#751](https://github.com/bitrouter/bitrouter/pull/751)) - ([641d384](https://github.com/bitrouter/bitrouter/commit/641d3844be54a8b24d37958480134ad383004d10))
- *(tui)* [**breaking**] Move all drawing into bitrouter-tui ([#830](https://github.com/bitrouter/bitrouter/pull/830)) - ([afd3b5a](https://github.com/bitrouter/bitrouter/commit/afd3b5a3610754d0e8205629d9def2dda81701ff))
- Consolidate BitRouter Cloud SDK ownership ([#846](https://github.com/bitrouter/bitrouter/pull/846)) - ([85bf264](https://github.com/bitrouter/bitrouter/commit/85bf264c27f53cec1bf5c56b7f3763475562d2da))
- Unify task-aware routing on agent_route/v1 ([#828](https://github.com/bitrouter/bitrouter/pull/828)) - ([ee4dbb2](https://github.com/bitrouter/bitrouter/commit/ee4dbb2e3930b5187828da6849af290d4e71882a))

### 📚 Documentation

- Sync status and launch docs ([#840](https://github.com/bitrouter/bitrouter/pull/840)) - ([5775cbc](https://github.com/bitrouter/bitrouter/commit/5775cbc0eea6ec4501fe7462ce4b44299b9cf3cb))
- Harden Harbor EC2 network preflight ([#838](https://github.com/bitrouter/bitrouter/pull/838)) - ([9e05f3b](https://github.com/bitrouter/bitrouter/commit/9e05f3ba21fd2f802cdb96764ce0a188ce69b298))
- Align docs with current CLI surfaces ([#802](https://github.com/bitrouter/bitrouter/pull/802)) - ([109f3ec](https://github.com/bitrouter/bitrouter/commit/109f3ece91cde14d0a248ec2141355014a63c36b))
- Correct routing surface docs ([#741](https://github.com/bitrouter/bitrouter/pull/741)) - ([735b316](https://github.com/bitrouter/bitrouter/commit/735b3162dd29264ced65d2e655fa4b47d68f188f))

### ⚡ Performance

- *(trajectory)* Bound prefix lookup cardinality - ([af531e1](https://github.com/bitrouter/bitrouter/commit/af531e188ef6954f623d950699962a3d086f9668))
- *(trajectory)* Bound prefix correlation - ([ae6baac](https://github.com/bitrouter/bitrouter/commit/ae6baaca9785c33cd50de3252127796649b9ae23))

### 🧪 Testing

- *(trajectory)* Prove progress control - ([2cf82f1](https://github.com/bitrouter/bitrouter/commit/2cf82f1f8e82453138c61d34c25bad5d3c72bdda))


- **Headless permissions and formats for `acp prompt` / `spawn -p`.** A
  headless run used to deny every `session/request_permission`; now the caller
  states the rule: `--deny-all` (still the default), `--approve-reads` (the ACP
  tool kind is `read` or `search`), `--approve-all`, and a per-tool
  `--permission-policy '{"autoApprove":[…],"autoDeny":[…],"defaultAction":…}'`
  (or `@path`). Each answer is a new NDJSON line,
  `{"type":"permission","decision":"approved"|"denied","title":…,"kind":…}`,
  and the process exits **5** when at least one request was denied and none
  approved. `--format text` prints the transcript exactly as `bitrouter chat`
  prints it to a pipe; `--format quiet` prints the assistant's text only;
  `json` (the default) is the unchanged NDJSON. The decision is made by
  `bitrouter_tui::permission::Policy` and reaches the agent through one shared
  interpreter (`chat/effects.rs`) that the interactive TUI, the piped `chat`,
  and `acp prompt` all run — the first session-verb parity between the
  terminal and the headless CLI.

- **Breaking (MCP):** the origin MCP server's `complete` tool is **removed**.
  MCP is BitRouter's control and introspection surface; running a completion is
  what the daemon's HTTP inference API is for. Use it instead — it is the
  transport built for inference, and it always had the surface the tool did
  not: streaming, the full parameter set, and the metering path.

  ```jsonc
  // before — MCP tools/call
  { "name": "complete",
    "arguments": { "model": "openai/gpt-4o",
                   "messages": [{ "role": "user", "content": "hi" }] } }
  ```

  ```bash
  # after — the daemon's HTTP API (OpenAI-shaped; /v1/messages is the
  # Anthropic-shaped twin)
  curl http://127.0.0.1:4356/v1/chat/completions \
    -H 'content-type: application/json' \
    -d '{"model":"openai/gpt-4o","messages":[{"role":"user","content":"hi"}]}'
  ```

  What remains on `bitrouter mcp serve` is `list_models`, `status`,
  `route_preview` and the skills pair: what is routable, where it would go, and
  what it has cost. The stdio profile is now `list_models` + `status` +
  `route_preview` + `skills_search` + `skills_get`; the HTTP profile is
  `list_models`, plus `status` where the backend can answer it.

  The one-line spend footer went with it. It only ever rode successful
  `complete` results; `status` reports the same figure as typed structured
  content under `spend`, from the same metering database, with `unpriced` and
  the remaining cap a footer had no room for.

  **Breaking (Rust API):** `bitrouter_mcp::backend::Backend::complete` and the
  `CompleteRequest` / `CompleteResponse` / `Usage` types are gone; `Backend` is
  now a pure port-handover trait (`status_port` + `models_port`). Also removed:
  `server::Builder::completion`, `server::CostFooter`,
  `BitrouterMcp::with_cost_footer`, `server::CompleteArgs`, and
  `ServeOptions::cost_footer`. `server::serve_stdio` loses its `cost_footer`
  parameter and now takes the handler alone. `serve_http_on`, `CloudBackend`
  and `CloudAuth` are unchanged.

- **Breaking (MCP wire, SEP-2640):** the skills extension's `resources` field
  now matches the specification the MCP Core Maintainers **accepted on
  2026-09-03**. BitRouter previously implemented an earlier draft and emitted
  entries a conforming host must refuse. Three changes:

  ```jsonc
  // before — a skill entry BitRouter published
  {
    "uri": "skill://pdf-processing/SKILL.md",
    "frontmatter": { "name": "pdf-processing", "description": "…" },
    "resources": [
      { "uri": "skill://pdf-processing/SKILL.md", "digest": "sha256:…" }
    ]
  }

  // after
  {
    "uri": "skill://pdf-processing/SKILL.md",
    "frontmatter": { "name": "pdf-processing", "description": "…" },
    "resources": [
      { "uri": "skill://pdf-processing/SKILL.md", "digest": "sha256:…",
        "size": 5120 }                 // new, REQUIRED on every entry
    ]
  }
  ```

  1. **`size` is required** on every `resources` entry — the file's raw byte
     length. It lets a host budget a skill from the listing alone, and a read
     whose length differs from `size` is now a verification failure *equivalent
     to a digest mismatch*, whether or not the digest is computed.
  2. **`resources` is required, and "dynamic" is a string marker.** The draft
     let the key be omitted to mean "generated dynamically"; the accepted
     specification requires the key and takes either the complete array or the
     literal string `"dynamic"`. An entry with neither "is invalid, and hosts
     MUST NOT load it". The gateway therefore no longer republishes an upstream
     entry that omits `resources`; it skips it, as it already skipped other
     malformed entries.
  3. **Per-skill limits are fixed**: 512 `resources` entries and 16 MiB
     (16,777,216 bytes) summed over `size`. BitRouter will not serve or
     re-publish a skill exceeding either.

  **Breaking (Rust API):** `bitrouter_sdk::mcp::skills::SkillResource` gains
  `size: u64`, and `SkillEntry::resources` changes from
  `Option<Vec<SkillResource>>` to the new `SkillResources` enum
  (`Enumerated(Vec<SkillResource>)` | `Dynamic`).

- **Breaking (CLI):** `bitrouter skills list --json` changes shape, and the
  command sees more skills than it used to. `bitrouter skills list` and the MCP
  `skills_search` tool are now one action over one report type.

  ```jsonc
  // before
  { "skills": [{ "name": "alpha", "path": "/p/.claude/skills/alpha" }] }

  // after
  { "skills": [{
      "name": "alpha",
      "description": "What alpha does",          // new
      "dir": "/p/.claude/skills/alpha",          // was `path`
      "skill_md": "/p/.claude/skills/alpha/SKILL.md",  // new
      "valid": true,                             // new
      "problem": "…"                             // new, omitted when valid
  }] }
  ```

  `path` meant the skill *directory* to the CLI and the *`SKILL.md` file* to the
  MCP tool. Neither was wrong; sharing one key for both was, so both surfaces
  now carry `dir` and `skill_md`.

  Three behaviour fixes ride along, all from collapsing three discovery rules
  over two roots into one:

  - **All three conventional layouts are listed.** `bitrouter skills list` read
    only `<root>/.claude/skills`, so a `./skills/foo` skill was invisible to it
    while the agent could see it.
  - **A skill that cannot be loaded is listed and explained**, with
    `valid: false` and a `problem`, instead of being listed unmarked by the CLI
    and dropped silently by the agent's surface. SEP-2640's `skills/list` still
    publishes only the loadable ones, which the specification requires.
  - **The MCP surfaces read the user-global root**, which only `-g` used to
    reach. An agent no longer misses a skill because of where it was installed.

- **Fixed (MCP):** a `bitrouter mcp install`-ed client now sees the skills
  tools. `mcp install` writes `["mcp", "serve"]`, but the skills and SEP-2640
  surfaces were wired only under `mcp serve --backend skills`, so an installed
  client never saw a skill. Every **stdio** profile now carries them — the
  identity argument that makes `--backend skills` stdio-only (the server is a
  subprocess of the caller whose machine it is) holds identically for stdio
  `mcp serve`. The multi-tenant HTTP profile is unchanged and still carries
  neither.

- **Breaking (CLI):** `bitrouter route --json` changes shape. `bitrouter route`
  and the MCP `route_preview` tool are now one action over one report type, and
  the report keeps `route_preview`'s richer vocabulary — it was the superset,
  and it is the one an agent reads. Duplicated keys for one fact are exactly the
  drift the unification exists to remove, so the old names are gone rather than
  deprecated.

  ```jsonc
  // before — `bitrouter route gpt-5 --json`
  {
    "model": "gpt-5",
    "resolved_via": "config",
    "chain": [{ "provider": "openai", "service_id": "gpt-5", "protocol": "openai" }]
  }

  // after
  {
    "requested_model": "gpt-5",          // was `model`
    "effective_model": "gpt-5-codex",    // new: what the policy table selects
    "effective_effort": "high",          // new, omitted when policy selects none
    "resolved_via": "config",            // now live | config | zero_config
                                         // (was live daemon | config | zero-config)
    "policy_decision": { … },            // new, omitted on the live-daemon path
    "provider_chain": [                  // was `chain`
      { "provider": "openai", "service_id": "gpt-5-codex", "api_protocol": "openai" }
                                         // `protocol` → `api_protocol`
    ],
    "estimated_cost": { … }              // new, omitted when the registry prices nothing
  }
  ```

  Migration is mechanical: `.model` → `.requested_model` (read
  `.effective_model` if you want what would actually run), `.chain` →
  `.provider_chain`, `.chain[].protocol` → `.provider_chain[].api_protocol`,
  and `.resolved_via == "live daemon"` → `"live"` / `"zero-config"` →
  `"zero_config"`. The `resolved_via` values are now the same words
  `bitrouter models --json` uses for the same fact (`live` / `config`), instead
  of two spellings that almost matched.

  Two behaviour fixes ride along. `bitrouter route` now runs the **policy
  table** in its config fallback, as `route_preview` always did — it could
  previously name a model the daemon would never pick, which is why
  `effective_model` is a separate field from `requested_model`. (The
  live-daemon path is unchanged on both surfaces: the daemon's `route` verb
  resolves the model as given, so a `live` answer reports
  `effective_model == requested_model` and no `policy_decision`.) And
  `route_preview` now resolves config **per call** instead of snapshotting it at
  `bitrouter mcp serve` start, so an edited `bitrouter.yaml` is visible to a
  long-lived MCP server, as it always was to the CLI.

- **Breaking (Rust API):** `bitrouter_mcp::capabilities::routing` is gone. The
  routing port moved to `bitrouter_mcp::actions::route` and is now typed:
  `RoutingQuery::preview(RoutePreviewArgs) -> serde_json::Value` becomes
  `RouteQuery::route(RouteInput) -> RouteReport`. `ServeOptions::routing` takes
  the new trait object.

- **Breaking (MCP tools):** the origin server's `status` and `list_models`
  tools return the same report types as `bitrouter status` and
  `bitrouter models`, and advertise them as `output_schema`.
  - `list_models` was a bare `[{ id, provider }]` that kept only the **first**
    provider of each model. It is now
    `{ models: [{ id, providers: [...] }], resolved_via: "live" | "config" }`
    — the whole fallback chain per model — with an optional `provider`
    argument (the same filter as `bitrouter models --provider`). On
    stdio + local it reads the daemon's live routing table over the control
    socket and falls back to a config parse, so it **answers with no daemon
    running**; `resolved_via` says which view it is.
  - `status` was a `GET /v1/models` in disguise (`{ listen, models, providers }`
    locally, the raw balance on cloud). It is now
    `{ running, pid?, listen?, models?, providers, socket?, spend? }`: a
    stopped daemon is `running: false`, **not a tool error**, and `spend`
    carries two independent halves — `spent` (today's locally metered
    estimate, with `unpriced` saying how partial it is) and `limit` (a
    metered account's remaining credit). It is no longer served on HTTP +
    local — nothing on that transport can read the daemon's control socket —
    and no longer carries the free-text spend footer, which is now the typed
    `spend` block (the footer is gone from the server entirely — see the
    `complete` removal above).

- `bitrouter status --json` gains `providers[]` (the distinct providers
  behind the routable models) and `spend`; `bitrouter models --json` gains
  `resolved_via`. Both are additive — every pre-existing key is unchanged.
  `bitrouter models` now prefers the running daemon's catalog, like
  `bitrouter route`, and falls back to the config parse it always used;
  standalone answers (`models`, `route`, `spawn`'s Codex preflight) now
  resolve a config the way the daemon does, so a subscription-backed
  provider (`claude-code`, `google-ai`) is no longer missing from them.

- **Breaking (Rust API):** `bitrouter_mcp::backend::{ModelInfo, StatusInfo,
  ProviderStatus}` and `Backend::{list_models, status}` are gone; `Backend`
  gains `status_port` / `models_port` (`Option<Arc<dyn StatusQuery>>` /
  `Option<Arc<dyn ModelsQuery>>`), `Builder::completion_local` is gone (its
  `models(...)` half is what survives — see the `complete` removal above), and
  `ServeOptions` gains
  `status` and `models`. `bitrouter_sdk::language_model::routing::ModelInfo`
  is the element type of the shared report and now derives `JsonSchema`,
  `PartialEq` and `Eq`.

- **Breaking (CLI):** `bitrouter skills add`, `remove`, `find`, and `update` are
  removed, along with the `bitrouter-skills` crate that backed them. Installing
  skills is the ecosystem's job — `npx skills add`, or the Claude Code / Codex
  plugin marketplaces (this repo ships as one). BitRouter **reads** the
  installed-skills directory and serves it over MCP; it does not populate it.
  That is the same line as "server, not host" applied to content lifecycle:
  BitRouter handles transport, not distribution.

  `bitrouter skills list` and `bitrouter skills init` remain. `SKILL.md` format
  support moved into the binary (`apps/bitrouter/src/skills/`), where its only
  consumers live; the git-clone, source-resolution, install-to-disk, and
  registry-client code is gone. The `--registry` / `--namespace` flags and the
  `api.bitrouter.ai` skills-hub client went with them.

- **Skills over MCP (SEP-2640).** BitRouter now serves Agent Skills as an MCP
  server and proxies them as a gateway, over stdio and Streamable HTTP alike.
  `bitrouter mcp serve --backend skills` answers `skills/list`, `skills/get`,
  `resources/list`, and `resources/read` over the installed-skills root, with
  complete `sha256:` digests per file; the existing `skills_search` /
  `skills_get` tools are unchanged and still served. The daemon's aggregate
  `POST /mcp` merges upstream skill catalogs, namespacing each under its
  configured server name (`skill://<server>/<skill-path>/SKILL.md`) so two
  upstreams publishing the same URI cannot shadow one another. BitRouter is a
  skills server and gateway, never a host: no daemon path installs skills, and
  gateway-sourced content never touches a filesystem skill-discovery path.
  Caveats are documented in `skills/bitrouter/references/mcp-server.md` — the
  gateway is not a security boundary, and remote catalogs are daemon-scoped
  rather than caller-scoped.

- **Breaking (behaviour):** aggregate `resources/read` (`POST /mcp`) no longer
  tries each member and returns the first success. It resolves exactly one
  owning member — by skill-URI label, else by which member enumerates the URI —
  and errors when zero or several match, naming the candidates. First-success
  scanning let configuration order silently decide which upstream answered a
  URI two members both served, which is a cross-origin misroute (and the
  impersonation surface SEP-2640 names for skills). A URI that no member
  enumerates is now an error on the aggregate endpoint; read it from that
  server's direct route (`POST /mcp/{server}`) instead.

- The gateway's `initialize` now advertises the `resources` capability and
  declares the `io.modelcontextprotocol/skills` extension. Skills are read
  through `resources/read`, so a compliant client that saw no `resources`
  capability would never issue one. The extension is declared optimistically —
  upstream capabilities are discovered lazily, so the gateway cannot know at
  handshake time whether any member serves skills.

- **Breaking (Rust API):** `BenchmarkOutcomeRecord` has a new `request_id`
  field and strict reward feedback joins it to the persisted
  `CapturedIngressTrace.id`. Migrate Rust struct literals to
  `BenchmarkOutcomeRecord::new(session_key, task_id, reward)` followed by
  `.with_request_id(trace_id)` when producing reward-feedback artifacts. Older
  outcome JSONL remains serde-compatible (`request_id` defaults to absent),
  but it is analytical-only and strict feedback rejects it.

- **Breaking (policy routes):** active `policy_table` routing now uses one
  predictive route contract:
  `agent_route/v1|<task-family>|<role>|<risk>`. Exact task-family routes fall
  back to the corresponding `unknown`-family role/risk baseline, then to the
  policy default. Observed `agent_trace/v2` keys remain telemetry only. Static
  `agent_trace` routes, three-segment predictive v1 routes, and all v2
  predictive routes are rejected during config and lock validation. Regenerate
  policy locks and certificates with the current predictor contract.

  `key_strategy: agent_trace` selects this deterministic predictor; the retired
  `workflow_state` and `legacy_fingerprint` spellings are rejected.
  `adequacy.max_downgraded_requests_per_session` is rejected: session identity
  is diagnostic-only and no longer affects routing. `adequacy.explore_opening`
  is honored for source-neutral opening projections.

- **Rust API:** `PolicyKeyStrategy` now exposes only the canonical `AgentTrace`
  variant. `PolicyDecision` keeps
  `workflow_state_kind` and `workflow_identity`; `PolicyDecisionRecord` keeps
  `workflow_state` and `workflow_identity`; and `PolicyDecisionSummary` keeps
  `by_workflow_state`. Their JSON output uses canonical `trace_state`,
  `trace_identity`, and `by_trace_state` names while accepting the old JSON
  spellings on input. The matching `trace_*` accessors are available for new
  Rust callers.

## [1.0.0-alpha.27](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.26...v1.0.0-alpha.27)


### ⛰️ Features

- *(sdk)* Clarify request timing metrics ([#738](https://github.com/bitrouter/bitrouter/pull/738)) - ([a5b0730](https://github.com/bitrouter/bitrouter/commit/a5b073078c38c4d449b872cd1fb43fed9cc0bd78))
- *(tui)* Scrollable PTY panes (host scrollback + mouse forwarding) ([#734](https://github.com/bitrouter/bitrouter/pull/734)) - ([9eb1186](https://github.com/bitrouter/bitrouter/commit/9eb1186a011dd2f5036d3c9964572ee6604fabe2))
- *(tui)* Wire MCP and skills gateways into harnesses ([#732](https://github.com/bitrouter/bitrouter/pull/732)) - ([b199653](https://github.com/bitrouter/bitrouter/commit/b1996539706991407a1e5f1dc67ab59bf951f80a))
- *(tui)* Composite manager  ([#715](https://github.com/bitrouter/bitrouter/pull/715)) - ([7a72dff](https://github.com/bitrouter/bitrouter/commit/7a72dfffc997d9b7becc1cf2f77fc57573674f23))

### 🐛 Bug Fixes

- *(fleet)* Non-blocking spawn/prompt so long turns don't time out ([#737](https://github.com/bitrouter/bitrouter/pull/737)) - ([84c1e99](https://github.com/bitrouter/bitrouter/commit/84c1e992fdebf7b9379caaeecfc6cfb19c581023))
- *(sdk)* Isolate protocol request extras ([#733](https://github.com/bitrouter/bitrouter/pull/733)) - ([dffc9a9](https://github.com/bitrouter/bitrouter/commit/dffc9a9efdd872e2e69605ccfae6f27b11d8bcd4))

### 🚜 Refactor

- *(tui)* Split state.rs into a state/ module ([#730](https://github.com/bitrouter/bitrouter/pull/730)) - ([d730e8e](https://github.com/bitrouter/bitrouter/commit/d730e8e9b1e6ad17dc452c27d6816c8dc053ee3f))


## [1.0.0-alpha.26](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.25...v1.0.0-alpha.26)


### ⛰️ Features

- *(acp)* Route spawn sub-agents through daemon by default ([#705](https://github.com/bitrouter/bitrouter/pull/705)) - ([e069c8c](https://github.com/bitrouter/bitrouter/commit/e069c8cded12add2aa3d9a26f64394919e5cb562))
- *(cli)* Add cloud API command ([#718](https://github.com/bitrouter/bitrouter/pull/718)) - ([26cea96](https://github.com/bitrouter/bitrouter/commit/26cea96221250ceec0892ab2a7e71e890c675141))
- *(routing)* Adaptive, self-optimizing policy-table routing ([#710](https://github.com/bitrouter/bitrouter/pull/710)) - ([0828e7e](https://github.com/bitrouter/bitrouter/commit/0828e7ed40104272da4b6e429b05cb3e5f03079d))

### 🐛 Bug Fixes

- *(sdk)* Preserve upstream bad requests ([#716](https://github.com/bitrouter/bitrouter/pull/716)) - ([d6872ba](https://github.com/bitrouter/bitrouter/commit/d6872ba1ed73fd6d600b99c6c1a5adab063b699d))

### 🚜 Refactor

- Remove attestation, fold plugins/ into crates/ ([#701](https://github.com/bitrouter/bitrouter/pull/701)) - ([e9896fa](https://github.com/bitrouter/bitrouter/commit/e9896fa321c29f753db3b76e4bd6d684d0307cf4))
- Relocate bitrouter-mcp crate under crates/ ([#704](https://github.com/bitrouter/bitrouter/pull/704)) - ([a01a118](https://github.com/bitrouter/bitrouter/commit/a01a1184bf7ebaf352c082897bdd1b6d0be3fd30))


## [1.0.0-alpha.25](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.24...v1.0.0-alpha.25)


### ⚙️ Miscellaneous Tasks

- Refresh Rust dependencies and MSRV ([#699](https://github.com/bitrouter/bitrouter/pull/699)) - ([f7ad755](https://github.com/bitrouter/bitrouter/commit/f7ad75509b5a7195f279f93dbcad0e66c52e0a60))


## [1.0.0-alpha.24](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.23...v1.0.0-alpha.24)


### 🐛 Bug Fixes

- *(sdk)* Preserve streaming preflight errors ([#696](https://github.com/bitrouter/bitrouter/pull/696)) - ([24d9ecf](https://github.com/bitrouter/bitrouter/commit/24d9ecf51ce4343c9d97e33a00dedf4e82cffbaf))
- *(update)* Detect npm installs in self-updater ([#689](https://github.com/bitrouter/bitrouter/pull/689)) - ([0cb6ce2](https://github.com/bitrouter/bitrouter/commit/0cb6ce2f40284d169e16114bb2e0c59f577b07aa))
- Record canonical stream timing and OTEL latency ([#697](https://github.com/bitrouter/bitrouter/pull/697)) - ([639ec9b](https://github.com/bitrouter/bitrouter/commit/639ec9b3ed9f0548102108d49699d99a34cb2740))


## [1.0.0-alpha.23](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.22...v1.0.0-alpha.23)


### ⛰️ Features

- *(agents)* Add pi-acp to the ACP catalog ([#685](https://github.com/bitrouter/bitrouter/pull/685)) - ([c12f5d8](https://github.com/bitrouter/bitrouter/commit/c12f5d842932d1fb843dc218fbdf81662d6e3248))
- *(providers)* Add SuperGrok BYO subscription ([#676](https://github.com/bitrouter/bitrouter/pull/676)) - ([ad266c7](https://github.com/bitrouter/bitrouter/commit/ad266c7bf13aa480e9f02ea46e7f9716f88fc4cf))
- Ship Claude Code and Codex agent plugins ([#683](https://github.com/bitrouter/bitrouter/pull/683)) - ([6ea6458](https://github.com/bitrouter/bitrouter/commit/6ea6458926e0bde62ffd55415d4cd2460c7af34a))
- Add AWS Bedrock, Azure, Vertex built-in providers ([#647](https://github.com/bitrouter/bitrouter/pull/647)) - ([6b92fcf](https://github.com/bitrouter/bitrouter/commit/6b92fcf612ee5eff54e426f7ba8b50a023002caa))

### 🐛 Bug Fixes

- *(sdk)* Preserve token limits and upstream errors ([#692](https://github.com/bitrouter/bitrouter/pull/692)) - ([495b1e9](https://github.com/bitrouter/bitrouter/commit/495b1e9aa4de4e79f877ca2d7fda0cf88bffad65))


## [1.0.0-alpha.22](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.21...v1.0.0-alpha.22)


### ⛰️ Features

- Configurable upstream HTTP timeouts ([#662](https://github.com/bitrouter/bitrouter/pull/662)) - ([88d1e4d](https://github.com/bitrouter/bitrouter/commit/88d1e4da5a774bf44a7e569be2fda3e60dd47c7a))

### 🐛 Bug Fixes

- *(messages)* Drop unsigned reasoning on Anthropic wire ([#669](https://github.com/bitrouter/bitrouter/pull/669)) - ([d78ddec](https://github.com/bitrouter/bitrouter/commit/d78ddec4a3bc05e11c60e00f64874387fbdccb38))


## [1.0.0-alpha.21](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.20...v1.0.0-alpha.21)


### ⛰️ Features

- Make registry dist runtime-only ([#641](https://github.com/bitrouter/bitrouter/pull/641)) - ([733bf7f](https://github.com/bitrouter/bitrouter/commit/733bf7f576607dd5e47ca0853e241df531e3468e))
- Refactor registry provider variants ([#639](https://github.com/bitrouter/bitrouter/pull/639)) - ([0436241](https://github.com/bitrouter/bitrouter/commit/04362417b9821b715d9c858789083534cf302a3b))

### 🐛 Bug Fixes

- Preserve stream error settlement ([#656](https://github.com/bitrouter/bitrouter/pull/656)) - ([4e1df77](https://github.com/bitrouter/bitrouter/commit/4e1df77805a6f91a1607d44035ab1fea2e82d988))


## [1.0.0-alpha.20](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.19...v1.0.0-alpha.20)


### ⛰️ Features

- *(cli)* Spawn codex agent ([#631](https://github.com/bitrouter/bitrouter/pull/631)) - ([27b5bae](https://github.com/bitrouter/bitrouter/commit/27b5baecccd20ace3cf3bb211ff5a577a7cbc4a7))
- *(cli)* Route `update` through the JSON output layer ([#620](https://github.com/bitrouter/bitrouter/pull/620)) - ([a2edc3f](https://github.com/bitrouter/bitrouter/commit/a2edc3f409627cd595ffe9429196ec4648ad40ae))
- *(cli)* JSON output layer with a uniform error envelope ([#610](https://github.com/bitrouter/bitrouter/pull/610)) - ([df6e3cc](https://github.com/bitrouter/bitrouter/commit/df6e3cc487bba9e60461828c86a028656a619e21))
- *(server-tools)* Built-in web_fetch tool (BYOK) ([#612](https://github.com/bitrouter/bitrouter/pull/612)) - ([ba5308d](https://github.com/bitrouter/bitrouter/commit/ba5308df77cf8457812b93af6ff557a8c8f24728))
- *(server-tools)* Add Tavily web_search backend, drop Perplexity ([#608](https://github.com/bitrouter/bitrouter/pull/608)) - ([6579faa](https://github.com/bitrouter/bitrouter/commit/6579faa122381208b5b4902b8c59b1429fd640e6))
- *(server-tools)* Built-in web_search with BYOK backends ([#603](https://github.com/bitrouter/bitrouter/pull/603)) - ([7ebc572](https://github.com/bitrouter/bitrouter/commit/7ebc572ef33cb902a8a1ecea2b7d351a1300c4c1))
- *(update)* Add bitrouter update self-updater ([#607](https://github.com/bitrouter/bitrouter/pull/607)) - ([2b95d0c](https://github.com/bitrouter/bitrouter/commit/2b95d0c941930c8a7b10c58e33b5c66e2db16e4d))

### 🐛 Bug Fixes

- *(codex)* Harden Codex spawn checks ([#632](https://github.com/bitrouter/bitrouter/pull/632)) - ([e2264d6](https://github.com/bitrouter/bitrouter/commit/e2264d636e35689a4ac4366c421836cf4c43ed72))
- *(config)* Don't expand ${VAR} inside YAML comments ([#609](https://github.com/bitrouter/bitrouter/pull/609)) - ([7628d98](https://github.com/bitrouter/bitrouter/commit/7628d9855412da2a51e07ac11fe31a8b3510850e))
- *(observe)* Stop the settle span double-counting as a telemetry event ([#605](https://github.com/bitrouter/bitrouter/pull/605)) - ([2453112](https://github.com/bitrouter/bitrouter/commit/2453112f6e7703186c67cb1ae1f8578a5548084f))

### 📚 Documentation

- Align docs with current CLI ([#637](https://github.com/bitrouter/bitrouter/pull/637)) - ([29a693d](https://github.com/bitrouter/bitrouter/commit/29a693d9e97913b516449f9cfe11908d35c6af82))

### ⚙️ Miscellaneous Tasks

- Migrate registry into OSS dist ([#636](https://github.com/bitrouter/bitrouter/pull/636)) - ([afdb4b7](https://github.com/bitrouter/bitrouter/commit/afdb4b7f68723456efa804e1d2760ed103a59a84))


## [1.0.0-alpha.19](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.18...v1.0.0-alpha.19)


### ⛰️ Features

- *(cli)* Wait for daemon readiness on start; auto-start it on spawn ([#597](https://github.com/bitrouter/bitrouter/pull/597)) - ([7d6c85d](https://github.com/bitrouter/bitrouter/commit/7d6c85d5f06ce718dbe63c68f0a9c0ff5d7d1f2f))
- *(telemetry)* Live account-bearer refresh + $lib/$screen_name attributes ([#598](https://github.com/bitrouter/bitrouter/pull/598)) - ([b0f45e8](https://github.com/bitrouter/bitrouter/commit/b0f45e827e787d031d92d8f61770f636a1136475))


## [1.0.0-alpha.18](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.17...v1.0.0-alpha.18)


### ⛰️ Features

- *(claude-code)* Route Claude Code traffic to a dedicated subscription provider ([#593](https://github.com/bitrouter/bitrouter/pull/593)) - ([15fde63](https://github.com/bitrouter/bitrouter/commit/15fde6346c209e4f864d0b35e573006588765022))
- *(messages)* Preserve thinking-block signature through the streaming round-trip ([#592](https://github.com/bitrouter/bitrouter/pull/592)) - ([4d76676](https://github.com/bitrouter/bitrouter/commit/4d766767a29cc90149c919a3197e24d47bebad4a))
- *(observe)* Opt-in first-party telemetry export ([#588](https://github.com/bitrouter/bitrouter/pull/588)) - ([e95977f](https://github.com/bitrouter/bitrouter/commit/e95977f4fc4e484aa1311d17510a97ae26a67dca))
- *(providers)* Source built-ins from the registry snapshot ([#591](https://github.com/bitrouter/bitrouter/pull/591)) - ([e58c623](https://github.com/bitrouter/bitrouter/commit/e58c62332d74e402553ebba78939634dc5212f9b))
- *(registry)* Registry-backed model resolution ([#589](https://github.com/bitrouter/bitrouter/pull/589)) - ([fa4083e](https://github.com/bitrouter/bitrouter/commit/fa4083ec5789cb970a5e291786609ef9f8005e00))
- *(spawn)* Launch coding agents through BitRouter via `bitrouter spawn` ([#584](https://github.com/bitrouter/bitrouter/pull/584)) - ([5ca7ac8](https://github.com/bitrouter/bitrouter/commit/5ca7ac8c823d0b5d7f85f89d8d18b1dbcdc0cef0))
- *(telemetry)* Account-authenticated exports + anonymous opt-out ([#596](https://github.com/bitrouter/bitrouter/pull/596)) - ([d6406dd](https://github.com/bitrouter/bitrouter/commit/d6406ddfbb939beddb9e5cb9f4b504eb7f62f4ab))
- Read the existing Claude Code session for Claude subscription auth ([#590](https://github.com/bitrouter/bitrouter/pull/590)) - ([a720eb2](https://github.com/bitrouter/bitrouter/commit/a720eb231faab462de5c9b8929f67323772b0d3f))

### 🐛 Bug Fixes

- *(claude-code)* Detect subscription traffic by the agent-profile beta ([#594](https://github.com/bitrouter/bitrouter/pull/594)) - ([d528b47](https://github.com/bitrouter/bitrouter/commit/d528b47420dc941e77484cb1f8f8c9155a680858))


## [1.0.0-alpha.17](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.15...v1.0.0-alpha.17)


### ⛰️ Features

- *(attestation)* NEAR confidential-inference verification (L1 + L1.5) ([#563](https://github.com/bitrouter/bitrouter/pull/563)) - ([9dc0a1d](https://github.com/bitrouter/bitrouter/commit/9dc0a1d9cea18b74f7ee112c85e5968545957103))
- *(config)* JSON Schema + config validate for IaC ([#575](https://github.com/bitrouter/bitrouter/pull/575)) - ([8ab2382](https://github.com/bitrouter/bitrouter/commit/8ab2382240964dbbdb7804c808d4c22eead3b672))
- *(fusion)* Multi-model deliberation server tool + alias ([#574](https://github.com/bitrouter/bitrouter/pull/574)) - ([c7bfac9](https://github.com/bitrouter/bitrouter/commit/c7bfac9482d86287e8c44d789f532edc16516e62))
- *(mcp)* Answer the MCP initialize/ping handshake at the gateway ([#561](https://github.com/bitrouter/bitrouter/pull/561)) - ([43d1365](https://github.com/bitrouter/bitrouter/commit/43d1365fce758f4fb2b4ea528d01ef55fdd92d82))
- *(sdk)* Expose server-tool usage for observability ([#568](https://github.com/bitrouter/bitrouter/pull/568)) - ([5eaf1c3](https://github.com/bitrouter/bitrouter/commit/5eaf1c35d399d6e84f8fd0cd9c644a0f6d1cf17b))

### 🐛 Bug Fixes

- *(attestation)* [**breaking**] Pin & assert TDX base MRs ([#567](https://github.com/bitrouter/bitrouter/pull/567)) ([#578](https://github.com/bitrouter/bitrouter/pull/578)) - ([62e412c](https://github.com/bitrouter/bitrouter/commit/62e412cd5b035b7b4e379cb14829f9d4c599bfec))
- *(attestation)* Enforce DCAP TCB status floor ([#573](https://github.com/bitrouter/bitrouter/pull/573)) - ([5193bf9](https://github.com/bitrouter/bitrouter/commit/5193bf9cece32e1ae006e876e180d73d0c8e7356))
- *(chat)* Buffer streamed tool call until name arrives ([#577](https://github.com/bitrouter/bitrouter/pull/577)) - ([ab02eb6](https://github.com/bitrouter/bitrouter/commit/ab02eb6ddc4fd56110f5d7038b5b19830bb588ea))

### 🚜 Refactor

- *(server-tools)* Move advisor/sub-agent into the SDK ([#576](https://github.com/bitrouter/bitrouter/pull/576)) - ([5ffe7b1](https://github.com/bitrouter/bitrouter/commit/5ffe7b13b0d808f45b82a70cc73f8450e5965114))


## [1.0.0-alpha.15](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.14...v1.0.0-alpha.15)


### ⛰️ Features

- *(server-tools)* Router-executed server-side tool loop ([#555](https://github.com/bitrouter/bitrouter/pull/555)) - ([e41e1ce](https://github.com/bitrouter/bitrouter/commit/e41e1cef6481929918f001ca1ec4e54e95d99988))
- *(server-tools)* Replace owned provider-defined tools on inject ([#559](https://github.com/bitrouter/bitrouter/pull/559)) - ([df70e12](https://github.com/bitrouter/bitrouter/commit/df70e1260891f1694c802d32b319a66686bd2c17))
- *(server-tools)* Pass ToolContext to RouterToolset ([#558](https://github.com/bitrouter/bitrouter/pull/558)) - ([bffff57](https://github.com/bitrouter/bitrouter/commit/bffff57506fec640f6aa8e25d48094fd78a01127))
- Protocol-native routing ([#554](https://github.com/bitrouter/bitrouter/pull/554)) - ([197f812](https://github.com/bitrouter/bitrouter/commit/197f8126a108abd30a2b9f02cb59d9b0028b54d4))


## [1.0.0-alpha.14](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.13...v1.0.0-alpha.14)


### 🐛 Bug Fixes

- *(chat)* Drop provider-defined tools (no chat wire form) ([#553](https://github.com/bitrouter/bitrouter/pull/553)) - ([b0b4ef3](https://github.com/bitrouter/bitrouter/commit/b0b4ef365d83d9a5f2ee380067320ff5303d9f2c))


## [1.0.0-alpha.13](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.12...v1.0.0-alpha.13)


### ⛰️ Features

- *(language-model)* LanguageModelV3 parity for the canonical IR ([#548](https://github.com/bitrouter/bitrouter/pull/548)) - ([610ee27](https://github.com/bitrouter/bitrouter/commit/610ee2788f0496cf7268dd5244b7676543de9303))
- *(metering)* Context-tier ("staged") pricing ([#551](https://github.com/bitrouter/bitrouter/pull/551)) - ([0464703](https://github.com/bitrouter/bitrouter/commit/04647033f11f3e908178b354482bd1333ee289eb))

### 🐛 Bug Fixes

- *(protocol)* Don't fragment streamed tool calls ([#552](https://github.com/bitrouter/bitrouter/pull/552)) - ([e140b2f](https://github.com/bitrouter/bitrouter/commit/e140b2fb105de59b600104ed0f1019cad66d1cc9))


## [1.0.0-alpha.12](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.11...v1.0.0-alpha.12)


### ⛰️ Features

- *(mcp)* Origin MCP server for BitRouter ([#526](https://github.com/bitrouter/bitrouter/pull/526)) ([#530](https://github.com/bitrouter/bitrouter/pull/530)) - ([aa71d4c](https://github.com/bitrouter/bitrouter/commit/aa71d4c3bf571cae64c2a50ed1bf54603328f8f2))

### 🐛 Bug Fixes

- *(protocol)* Translate tool_choice across protocols ([#549](https://github.com/bitrouter/bitrouter/pull/549)) - ([8e60ce5](https://github.com/bitrouter/bitrouter/commit/8e60ce5e9e855d13f9d82e1381bafd4fcd0564bf))


## [1.0.0-alpha.11](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.10...v1.0.0-alpha.11)


### ⛰️ Features

- *(capabilities)* Extend Capability vocabulary (tools, reasoning, web_search, logprobs) ([#536](https://github.com/bitrouter/bitrouter/pull/536)) - ([e369f2e](https://github.com/bitrouter/bitrouter/commit/e369f2e184832c877f42bbb7b0673b9f38f3ef06))

### 🐛 Bug Fixes

- *(pipeline)* Run non-streaming requests to completion ([#538](https://github.com/bitrouter/bitrouter/pull/538)) - ([5528e3e](https://github.com/bitrouter/bitrouter/commit/5528e3e33fff495ee17ab41d1dc35962f5a6f2e8))
- *(stream)* Bill prompt tokens on client disconnect ([#537](https://github.com/bitrouter/bitrouter/pull/537)) - ([f0dc0f0](https://github.com/bitrouter/bitrouter/commit/f0dc0f0cddd55e79ff8304287ab5593f2fb8ccbd))


## [1.0.0-alpha.10](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.9...v1.0.0-alpha.10)


### ⛰️ Features

- *(capabilities)* Capability-aware routing primitives + typed structured-output fields (B4) ([#528](https://github.com/bitrouter/bitrouter/pull/528)) - ([e4fee7c](https://github.com/bitrouter/bitrouter/commit/e4fee7c85b9a7e134152cce3cf412d958183e419))
- *(observe)* [**breaking**] Forward cloud span attrs + content capture ([#535](https://github.com/bitrouter/bitrouter/pull/535)) - ([2c7edd3](https://github.com/bitrouter/bitrouter/commit/2c7edd3a54014d59c5048424ae828f9011b68a3e))
- *(sdk)* Surface MCP upstream 401/403 auth challenges (UpstreamAuth) ([#534](https://github.com/bitrouter/bitrouter/pull/534)) - ([f81a322](https://github.com/bitrouter/bitrouter/commit/f81a3221fa1918bf750612cb2c615bc4d59e67b4))
- *(sdk)* Implement VirtualModel priority/cascade strategy ([#521](https://github.com/bitrouter/bitrouter/pull/521)) - ([6b06d94](https://github.com/bitrouter/bitrouter/commit/6b06d94ac1d0a1f47cd463fd6d76b41b4c44732f))

### 🐛 Bug Fixes

- [**breaking**] Pre-1.0 security audit fixes + dead-code cleanup ([#523](https://github.com/bitrouter/bitrouter/pull/523)) - ([44ec022](https://github.com/bitrouter/bitrouter/commit/44ec02299a8df495ba4dbbb19408a39da9e3e55d))

### 🚜 Refactor

- *(cli)* [**breaking**] Subcommand cleanup (placeholders + auth) ([#524](https://github.com/bitrouter/bitrouter/pull/524)) - ([c2f5fef](https://github.com/bitrouter/bitrouter/commit/c2f5fefbcf91315b117a4739c2e3a912088a3331))


## [1.0.0-alpha.9](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.8...v1.0.0-alpha.9)


### ⛰️ Features

- *(providers)* Claude & Codex OAuth subscriptions ([#519](https://github.com/bitrouter/bitrouter/pull/519)) - ([6983051](https://github.com/bitrouter/bitrouter/commit/6983051e5dc93b4b9bfb9ddcf1e26a2f1c29f8d7))


## [1.0.0-alpha.8](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.7...v1.0.0-alpha.8)


### ⛰️ Features

- *(sdk)* [**breaking**] Per-target Messages auth scheme (x-api-key | Bearer) ([#516](https://github.com/bitrouter/bitrouter/pull/516)) - ([da10012](https://github.com/bitrouter/bitrouter/commit/da10012e4db97db650ccd69c8ac243ef2172e511))


## [1.0.0-alpha.7](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.6...v1.0.0-alpha.7)


### ⛰️ Features

- *(skills)* [**breaking**] Converge marketplace.json on Claude Code native schema ([#512](https://github.com/bitrouter/bitrouter/pull/512)) - ([0f83a39](https://github.com/bitrouter/bitrouter/commit/0f83a399fa21c414a4c55c6c0e4e93001aac6d43))

### 🐛 Bug Fixes

- *(cli)* Correct auth login --scope help to namespace-read ([#513](https://github.com/bitrouter/bitrouter/pull/513)) - ([b420fc1](https://github.com/bitrouter/bitrouter/commit/b420fc1d347a73e97558d5c4133d2c318bbc17e4))


## [1.0.0-alpha.6](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.5...v1.0.0-alpha.6)


### ⛰️ Features

- *(cloud-sdk)* Namespace-scoped management client + CLI ([#508](https://github.com/bitrouter/bitrouter/pull/508)) - ([f4ff599](https://github.com/bitrouter/bitrouter/commit/f4ff599cd2927d56c163b5281162b92b62fdfdcb))
- *(messages)* Integrate Opus 4.8 Messages API parameters ([#509](https://github.com/bitrouter/bitrouter/pull/509)) - ([c768b50](https://github.com/bitrouter/bitrouter/commit/c768b508f6374e861566ab1ba94e546a0ed1a30d))
- *(observe)* [**breaking**] Split otel feature into transport sub-features ([#506](https://github.com/bitrouter/bitrouter/pull/506)) - ([dcb3830](https://github.com/bitrouter/bitrouter/commit/dcb3830a1612b8284a092798ec0247543b4c622b))
- *(skills)* Skills gateway client — bitrouter-skills crate + CLI ([#511](https://github.com/bitrouter/bitrouter/pull/511)) - ([5de0786](https://github.com/bitrouter/bitrouter/commit/5de0786a811a0eedaf77ba981fd64f418350cee7))


## [1.0.0-alpha.5](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.4...v1.0.0-alpha.5)


### ⛰️ Features

- *(guardrails)* [**breaking**] Plugin + per-account rule resolution ([#503](https://github.com/bitrouter/bitrouter/pull/503)) - ([ee06962](https://github.com/bitrouter/bitrouter/commit/ee069621f1b56eaecc45d54b9409b69ce3383d92))
- *(observe)* [**breaking**] GenAI-semconv trace hierarchy + outbound traceparent ([#495](https://github.com/bitrouter/bitrouter/pull/495)) - ([d05fd5c](https://github.com/bitrouter/bitrouter/commit/d05fd5cbd155f3d798ac70c0b341b00176ff9c55))
- *(sdk)* [**breaking**] Streaming gen_ai.response.id via StreamPart::ResponseStarted ([#500](https://github.com/bitrouter/bitrouter/pull/500)) - ([4432941](https://github.com/bitrouter/bitrouter/commit/44329412689e41aca16b743f452b689c8e22d998))

### 🚜 Refactor

- *(sdk)* [**breaking**] Rename API protocols to spec names ([#504](https://github.com/bitrouter/bitrouter/pull/504)) - ([820ec67](https://github.com/bitrouter/bitrouter/commit/820ec675f56b7ae4b537829b4297fbee65c5274f))


## [1.0.0-alpha.4](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.3...v1.0.0-alpha.4)


### ⛰️ Features

- *(bitrouter)* Windows support for the daemon control surface ([#490](https://github.com/bitrouter/bitrouter/pull/490)) - ([9a960df](https://github.com/bitrouter/bitrouter/commit/9a960df6e8e65555e62db979fb1cfd19e5330609))
- *(sdk/mcp)* Expose headers, set_caller, evict for auth ([#494](https://github.com/bitrouter/bitrouter/pull/494)) - ([7bf6e76](https://github.com/bitrouter/bitrouter/commit/7bf6e76efc9ceecfcaf3da80efc5bedfb0a184f8))

### 🐛 Bug Fixes

- *(sdk/anthropic)* Fold cache buckets into prompt_tokens (Usage subset contract) ([#492](https://github.com/bitrouter/bitrouter/pull/492)) - ([5ee6921](https://github.com/bitrouter/bitrouter/commit/5ee6921a3125f6d2245338f23bff98640b71d1cc))


## [1.0.0-alpha.3](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.2...v1.0.0-alpha.3)


### ⛰️ Features

- *(cli)* Bitrouter cloud subcommand for /v1/* management ([#489](https://github.com/bitrouter/bitrouter/pull/489)) - ([c0dc761](https://github.com/bitrouter/bitrouter/commit/c0dc761b824777c8b3e36483a3328746e525c824))
- *(cli)* OAuth 2.0 device-flow auth subcommand ([#480](https://github.com/bitrouter/bitrouter/pull/480)) - ([f5cba27](https://github.com/bitrouter/bitrouter/commit/f5cba270795ace61eec682eee449c60fe2e11c7b))
- *(mcp)* Aggregation + caching + SSE + sampling-deny ([#484](https://github.com/bitrouter/bitrouter/pull/484)) - ([f1ab313](https://github.com/bitrouter/bitrouter/commit/f1ab313582bc92655c9eec9c27524a0622e3e063))
- *(observe)* Migrate to OpenTelemetry with multi-tenant attribution ([#475](https://github.com/bitrouter/bitrouter/pull/475)) - ([be513cf](https://github.com/bitrouter/bitrouter/commit/be513cf631b121a86fe57decc4ef89509802e73f))
- Bitrouter-cloud SDK + LLM provider with OAuth or API-key onboarding ([#486](https://github.com/bitrouter/bitrouter/pull/486)) - ([35e8894](https://github.com/bitrouter/bitrouter/commit/35e8894a461088a78540b7536c50137a1703d414))
- PKCE OAuth login for Anthropic and OpenAI Codex ([#481](https://github.com/bitrouter/bitrouter/pull/481)) - ([62d3b29](https://github.com/bitrouter/bitrouter/commit/62d3b29855039c51828a35e003a454cac457df18))

### 🐛 Bug Fixes

- *(bitrouter)* Drop duplicate reqwest dep entry ([#485](https://github.com/bitrouter/bitrouter/pull/485)) - ([d65fd1b](https://github.com/bitrouter/bitrouter/commit/d65fd1b6a2a3389f4707676de658c8a40a8488fb))


## [1.0.0-alpha.2](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.1...v1.0.0-alpha.2)


### ⛰️ Features

- *(routing)* Multiple accounts per provider — failover + load-balance ([#473](https://github.com/bitrouter/bitrouter/pull/473)) - ([a248fd3](https://github.com/bitrouter/bitrouter/commit/a248fd3644b08aebbde8b64f24bfe05716a536e0))
- *(sdk)* Structured outputs (response_format) for all 4 protocols ([#472](https://github.com/bitrouter/bitrouter/pull/472)) - ([8d4dc54](https://github.com/bitrouter/bitrouter/commit/8d4dc542e65f28ab751b38c1633839d8e9e4c243))
- *(sdk)* Add RouterOptions to opt out of built-in routes ([#478](https://github.com/bitrouter/bitrouter/pull/478)) - ([c548527](https://github.com/bitrouter/bitrouter/commit/c5485279531f351d4ebd2422435c4fb968bc9bb1))
- *(sdk)* Derive JsonSchema on wire types ([#467](https://github.com/bitrouter/bitrouter/pull/467)) ([#469](https://github.com/bitrouter/bitrouter/pull/469)) - ([901ec3e](https://github.com/bitrouter/bitrouter/commit/901ec3e83fdbbe689f3943cee46de2a3c0f46cdb))

### 🐛 Bug Fixes

- *(daemon)* Re-apply built-in provider catalog on file reload ([#476](https://github.com/bitrouter/bitrouter/pull/476)) - ([0bd11aa](https://github.com/bitrouter/bitrouter/commit/0bd11aaf84bf115fd6e9bfd4315400b33dfff166))

### 🚜 Refactor

- *(db)* Sea-orm migrations + all backends, no concrete driver ([#474](https://github.com/bitrouter/bitrouter/pull/474)) - ([f6a28dd](https://github.com/bitrouter/bitrouter/commit/f6a28dd78b52f1d038de2538b2a699e82d388888))
