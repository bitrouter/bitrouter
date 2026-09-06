---
type: changed
breaking: true
title: "The skills extension's `resources` field conforms to accepted SEP-2640"
pr: 870
---

The skills extension's `resources` field now matches the specification the MCP
Core Maintainers **accepted on 2026-09-03**. BitRouter previously implemented an
earlier draft and emitted entries a conforming host must refuse.

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
   whose length differs from `size` is now a verification failure *equivalent to
   a digest mismatch*, whether or not the digest is computed.
2. **`resources` is required, and "dynamic" is a string marker.** The draft let
   the key be omitted to mean "generated dynamically"; the accepted
   specification requires the key and takes either the complete array or the
   literal string `"dynamic"`. An entry with neither "is invalid, and hosts MUST
   NOT load it". The gateway therefore no longer republishes an upstream entry
   that omits `resources`; it skips it, as it already skipped other malformed
   entries.
3. **Per-skill limits are fixed**: 512 `resources` entries and 16 MiB
   (16,777,216 bytes) summed over `size`. BitRouter will not serve or re-publish
   a skill exceeding either.

**Rust API:** `bitrouter_sdk::mcp::skills::SkillResource` gains `size: u64`, and
`SkillEntry::resources` changes from `Option<Vec<SkillResource>>` to the new
`SkillResources` enum (`Enumerated(Vec<SkillResource>)` | `Dynamic`).
