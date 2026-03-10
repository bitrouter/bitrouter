## [unreleased]

### 🚀 Features

- *(core)* Language model
- *(core)* Update language and image models
- *(core)* Add error handling and cancellation handling
- *(openai)* OpenAI chat completions and responses adapters
- *(anthropic)* Anthropic Messages API provider in `bitrouter-anthropic` (#3)
- OpenAI Responses API support in `bitrouter-openai` (#4)
- *(core)* Define router traits
- Update core models definitions, implement warp router crate (#6)
- Use `dynosaur`-generated types for type-erased models
- Core server contracts and add trait-driven API filter adapters (#10)
- *(runtime)* Scaffold runtime + CLI
- Google Generative AI provider (#15)
- *(runtime)* Config-based routing table
- *(runtime)* Split config crate from runtime
- *(config)* Built-in provider registry
- *(runtime)* Implement daemon server
- *(runtime)* Implement server
- *(runtime)* Implement runtime router instance
- *(runtime)* Add Google generative AI as upstream provider (#18)
- *(tui)* Add bitrouter-tui crate with welcome screen (#16)
- *(cli)* Update daemon status and path resolution (#21)

### 🐛 Bug Fixes

- *(core)* Don't use associated consts in `LanguageModel` trait
- Clippy errors (#2)
- *(api)* Gate shared helpers by feature usage (#22)

### 💼 Other

- Model id is specified per request

### 🚜 Refactor

- *(openai)* Split openai api into smaller parts
- *(bitrouter)* Move `bitrouter` crate to a separate directory
- Move `model_id` to call options
- *(api)* Rename `bitrouter-warp-router` to `bitrouter-api`

### 📚 Documentation

- *(readme)* Add comprehensive project README
- *(readme)* Update workspace and crate README (#19)
- *(repo)* Refactor workspace docs (#23)
- *(readme)* Add badges for build status and social links (#31)

### 🧪 Testing

- *(openai)* Add tests for openai provider
- *(router)* Add streaming tests for messages and responses API

### ⚙️ Miscellaneous Tasks

- *(repo)* Initialize rust monorepo
- *(repo)* Add repository to Cargo.toml
- *(repo)* Create adapter crates
- *(repo)* Format root Cargo.toml
- Add basic github CI workflows (#1)
- *(repo)* Add `bitrouter-warp-router` crate
- *(openai)* Format code
- *(repo)* Format Cargo.toml
- *(repo)* Update cratre repo url
- *(router)* Remove deadcode check bypass marks
- *(config)* Fix clippy warnings
- *(config)* Format code
- Update .gitignore
- *(rust)* Add feature-matrix builds and cross-platform runtime coverage (#20)
- Create `release.toml` to configure commit and tag message
- *(release)* Publish v{{version}}
- *(repo)* Fix release commit message config
- *(release)* Publish v0.1.1
