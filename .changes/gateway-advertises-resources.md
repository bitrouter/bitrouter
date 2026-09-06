---
type: changed
title: "The gateway handshake advertises `resources` and the skills extension"
pr: 770
---

The gateway's `initialize` now advertises the `resources` capability and
declares the `io.modelcontextprotocol/skills` extension. Skills are read
through `resources/read`, so a compliant client that saw no `resources`
capability would never issue one.

The extension is declared optimistically — upstream capabilities are discovered
lazily, so the gateway cannot know at handshake time whether any member serves
skills.
