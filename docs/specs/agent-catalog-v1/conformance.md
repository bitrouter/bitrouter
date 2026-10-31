# Agent catalog conformance cases

1. A bare agent ID and its `local/` qualified form identify the same local
   runtime entry; a catalog-only agent is not assumed to be launchable.
2. Unknown routing or transport semantics do not cause an arbitrary command or
   expression to be executed.
3. An unpinned package invocation with a conformance record fails validation;
   a direct binary identifies its required binary.
4. A failed or unrun conformance tier is never published as `pass`. Agent and
   runtime pair, suite version and measured version remain attributable.
