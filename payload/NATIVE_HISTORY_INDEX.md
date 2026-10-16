# Native historical evidence checkpoint

Parent: `aa6f87a61818aac47990afed5bf5e2b1c515dc4d`. The overall refactor remains active and PR #981 remains unpushed.

The historical reader now first omits selected native public display details if its existing 48 KiB envelope would otherwise lose all execution identities and reports. It counts those omissions, marks public/history/timeline completeness false and retains original native SDK reports and receipt-verified archive references. ACP timing events are untouched. A still-oversized or invalid projection retains the existing explicit unavailable fallback. No execution, permission, cost, store, schema or protocol version was changed.

## Evidence

- green1 runs actual native work and an instruction revision through real SQLite, then finds no original execution in the historical projection. Its 472-file failed filesystem image is independently extracted and hash/mode verified.
- green2 retains the production reader's exact result: the original 48 KiB fallback omitted the execution. Its 473-file failed image also includes the history JSON and real SQLite copies before/after shutdown. These are filesystem/database checkpoints, not process/private-session images.
- green3 passes the actual archive boundary and six related evidence tests, strict workspace/all-target/all-feature Clippy and fmt. The scenario verifies two actual native Goal versions, stable owner/work identity, the original archived Thread/Turn, retained reported model identity, explicit omission/incompleteness, the unchanged 48 KiB limit, the production UI reader and tampered-archive refusal. Reading does not add a Thread fact or model request. Provider/semantic verdicts are fixtures; this does not attest a real provider or model quality.
- follow4 has byte-identical source: all 86 Goal runtime tests and three archive-matrix stress iterations pass. Both passing runs have no source drift or nextest LEAK markers. Earlier failures remain distinct and do not acquire later passing status.

The main implementation document was reduced from the preserved 1,339-line chronology to a 206-line current architecture/acceptance guide. The exact old bytes are in `native-history-prior-ledger5.md` and match the tested source hash. The guide preserves partial stage status, ACP cold-session gaps, native recovery limitations, budget/input behavior, necessary reproduction and the absence of a final current full-workspace pass. DEVELOPMENT also describes bounded historical reads and corrects its stale Goal protocol table to v8. Those two documentation files are the only post-test deltas.

`native-history-applicability5.json` verifies all four test-source tars, the final dirty-source tar, identical green3/follow4 bytes and the exact retained prior ledger. `native-history-manifest5.json` hashes raw results, sources, archives, runners and the index. A verified Git bundle/checkpoint record will bind the resulting commit to that manifest. Evidence remains local pending the sanitized independent publication requested for final PR delivery.

Main was fetched read-only at `54c822a34d4dbfd75d484ca35e108b497048efef`: one catalog-only commit after the prior main baseline (81 lines in five registry/catalog files). It has not yet been integrated into this frozen checkpoint. Confirmed ACP same-session recovery, remaining S0–S6/A01–A22 audit, live qualification limits, final main integration/full checks, archive publication, PR update and hosted CI remain open.
