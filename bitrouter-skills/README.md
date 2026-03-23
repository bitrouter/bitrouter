# bitrouter-skills

GitHub repository: [bitrouter/bitrouter](https://github.com/bitrouter/bitrouter)

Skills registry for BitRouter — tracks and manages agent skills following the
[agentskills.io](https://agentskills.io) open standard.

Skills are prompt-level knowledge (SKILL.md files) injected into LLM context,
distinct from MCP (runtime tool connectivity). Both share the unified `ToolEntry`
type from `bitrouter-core`, enabling skills to appear alongside MCP tools in
the `GET /v1/tools` discovery endpoint.

## Design

This crate is a **tracking layer**, not a content store. Agents (Claude Code,
OpenClaw, etc.) own their own skill files on disk. BitRouter tracks skill
metadata in a database so it can:

- Expose registered skills via `GET /v1/tools` for unified tool discovery
- Provide CRUD endpoints at `GET/POST/DELETE /v1/skills` following the
  Anthropic Skills API shape
- Declare which upstream providers a skill depends on, enabling BitRouter to
  handle payment (402/MPP) transparently

## API Endpoints

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/v1/tools` | Unified tool discovery (MCP tools + skills) |
| `POST` | `/v1/skills` | Register a new skill |
| `GET` | `/v1/skills` | List all registered skills |
| `GET` | `/v1/skills/:name` | Retrieve a single skill |
| `DELETE` | `/v1/skills/:name` | Delete a skill |

## Configuration

Skills can be pre-registered in `bitrouter.yaml`:

```yaml
skills:
  - name: "code-review"
    description: "Reviews code for quality, security issues, and best practices"
    source: "https://github.com/anthropics/skills"
    required_apis:
      - provider: anthropic

  - name: "translate"
    description: "Translates text between languages using LLM-powered translation"
```

Config-driven skills appear in `GET /v1/tools` immediately. The `/v1/skills`
CRUD endpoints operate on the database for runtime skill management.

## Includes

- Skill types (`Skill`, `SkillSource`, `InstalledBy`) in `skill`
- Sea-ORM entity for the `skills` table in `entity`
- Database migration in `migration`
- `ConfigSkillRegistry` (config-driven, no DB) and `SkillRegistry` (DB-backed CRUD) in `registry`
- Both implement `ToolRegistry` and `SkillService` traits from `bitrouter-core`
