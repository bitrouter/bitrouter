# Terminal continuation ordering

Local proof: terminal-continue-final77. Full source/config/logs/results and seven-file source tar are bound explicitly. All four Goal PTY cases and the full workspace, strict Clippy, formatting, doctests, generated artifacts, feature isolation and MSRV checks pass with two outer test slots and strict output-leak detection. Product Rust changed in the shared suspend helper and all four callers; unlike final62, this is not solely a fixture change.

Original Linux02f shard:33 passed,1 failed,6 skipped,178 unrun due fail-fast. Active Goal passed; the next scenario caught raw mode before continuation. The next matrix now collects all failures, preserving assertions and retries. Original hosted filesystem images were not exported. The failed intermediate final76 compile remains preserved without relabeling. Vendor/provider live gate remains unpassed.
