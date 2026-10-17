# PR #981 BRO refactor evidence

Final hosted CI: [exact-head results and raw logs](hosted-ci/HOSTED_CI_INDEX.md), [provenance](HOSTED_CI_PROVENANCE.json), [additional original file manifest](HOSTED_CI_PAYLOAD.json). Prior local/source archives remain unchanged at their pinned commits. SHA256SUMS covers this complete tree.

Latest local source: [Fixture image and complete source acceptance](fixture-image/FIXTURE_IMAGE_INDEX.md), [provenance](FIXTURE_IMAGE_PROVENANCE.json), [additional original files](FIXTURE_IMAGE_PAYLOAD.json). Earlier archives stay pinned at their own commits. SHA256SUMS covers this whole tree.

Latest local source: [Unix input-source selection and verification](unix-tty/UNIX_TTY_INDEX.md), [provenance](UNIX_TTY_PROVENANCE.json), [additional original files](UNIX_TTY_PAYLOAD.json). Earlier archives stay pinned at their own commits. SHA256SUMS covers this whole tree.

Latest local source: [Terminal continuation ordering and verification](terminal-continue/TERMINAL_CONTINUE_INDEX.md), [provenance](TERMINAL_CONTINUE_PROVENANCE.json), [additional original files](TERMINAL_CONTINUE_PAYLOAD.json). Earlier archives stay pinned at their own commits. SHA256SUMS covers this whole tree.

Latest local source: [Linux PTY and complete integration coverage](linux-pty/LINUX_PTY_INDEX.md), [provenance](LINUX_PTY_PROVENANCE.json), [additional original files](LINUX_PTY_PAYLOAD.json). Earlier archives stay pinned at their own commits. SHA256SUMS covers this whole tree.

Latest correction: [CI portability and final regression](ci-portability/CI_PORTABILITY_INDEX.md), [provenance](CORRECTION_PROVENANCE.json), [additional original file manifest](CORRECTION_PAYLOAD.json). The initial archive below remains unchanged at its own pinned commit. SHA256SUMS covers this complete archive tree.

Feature checkpoint: 6a72cfb1da0e2d4641a96097e09a507d7d032189. Final local verification: acp-continuation-final15. This separate branch preserves original failures, superseded runs, retained fixtures, source bindings, exact configurations, runners and recoverable Git bundles. Actual vendor harness/provider requalification remains unpassed.

Start with [the final index](payload/ACP_CONTINUATION_INDEX.md) and other *_INDEX.md files. Feature documentation pins this archive commit separately; production inputs match the final run's source hashes.

Verify all published files with `shasum -a 256 -c SHA256SUMS`. PAYLOAD.json records every original file's bytes, SHA-256 and mode. Ordinary root files are in payload/. The five original directories are complete tar files in packed-directories/ so nested hidden Git pointers remain inert bytes rather than submodule entries. Copy payload/ contents into an empty directory, then extract each packed-directories/*.tar.gz there to reconstruct the original archive. The independently restored 1:1 file/mode/hash check passed before publication.

Dirty source tars require their recorded HEAD. Git bundles require their recorded public base; in a clone containing that base, use `git bundle verify payload/<bundle>` before fetching. SQLite/filesystem copies are controlled evidence, not process images or proof of forced crash takeover. Vendor/private context/provider/cost qualification is explicitly separate.

Earlier pre-refactor evidence remains pinned at 382c2b58c5c25b02805175a33d60707dc4227b46.
