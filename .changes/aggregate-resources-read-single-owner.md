---
type: changed
breaking: true
title: "Aggregate `resources/read` resolves one owning member instead of scanning"
pr: 770
---

Aggregate `resources/read` (`POST /mcp`) no longer tries each member and
returns the first success. It resolves exactly one owning member — by skill-URI
label, else by which member enumerates the URI — and errors when zero or
several match, naming the candidates.

First-success scanning let configuration order silently decide which upstream
answered a URI two members both served, which is a cross-origin misroute (and
the impersonation surface SEP-2640 names for skills). A URI that no member
enumerates is now an error on the aggregate endpoint; read it from that
server's direct route (`POST /mcp/{server}`) instead.
