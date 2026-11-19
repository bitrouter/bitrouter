# Linux synthetic runtime qualification

The separate benchmark branch runs four locked, stripped Linux x64 release
builds in parallel: M0 and candidate, each with minimum and HTTP/2 features.
The paired job imports all four releases from run 44500284575, attempt 1.
It checks the producer run and build jobs, pinned archive hashes, embedded
build identity, ELF architecture, executable hash, lockfile, and equal
compiler, linker, libc and release settings before sampling.

Run on one fresh standard Ubuntu 24.04 x64 hosted VM. Each timed trial uses a
fresh Node driver, mock and, when applicable, gateway. Both sides use 1,024
JSON requests, 1,024 instant SSE requests, 512 two-leg tool conversations,
20 paced streams and 100 cancellations per path and repeat, with C4, eight
paths and five repeats per path. The larger short cohorts give 3,192 measured
HTTP legs per trial and 127,680 per side. The same 24 warmup legs and the
strict tenant's 105 receipt checks per trial still apply: one successful
receipt per scenario or tool leg, plus every cancellation. Validate all
HTTP outcomes, provider counts, terminal usage and exact tool content.
Each driver sends one typed result. The parent audits the assembled trial
before saving it and the scorer repeats that audit before freezing the
baseline or comparing the candidate.

Record these counts in the method before the first baseline trial. Run 40 M0
trials, hash and freeze the new budget, then run 40 candidate trials. The
scorer uses the declared cohort counts and retains all numerical allowances,
dispersion caps and quantile rules. Direct-control drift still leaves affected
rows unresolved. Fresh drivers reduce carryover, but the baseline-first order
may still expose time-dependent drift.

Before each side require three consecutive one-second CPU intervals strictly
above 80% idle, ten seconds apart between sample windows, with a 15-minute
limit. Use the Linux aggregate CPU counters; I/O wait and steal do not count
as idle. Record aggregate idle/steal per side across the measured trials.
The measurement job uploads only synthetic aggregates, method, frozen
budget and build provenance. Its input artifacts are the four public release
binaries and their build receipts, not trial evidence.

Treat the result as a separate Linux qualification. Do not pool it with
macOS samples or reuse their frozen budgets.
