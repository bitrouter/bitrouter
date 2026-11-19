# Comparing split gateway runtimes with synthetic traffic

The standalone workflow checks out the harness, pinned M0 and pinned candidate
separately. It first imports the minimum and HTTP/2 release artifacts from
the pinned M0 qualification and the exact candidate's successful native
qualification jobs. It checks the delivery digests, build receipts, signed
executables, lockfiles and matched compiler, SDK and release settings before
starting either workload. If the pair is absent, inaccessible, or built with
different settings, the job builds both sides with the same locked release
producer on the runner. An integrity or identity failure stops the job.
Both campaigns run sequentially in one ARM macOS job. It uses Node built-ins and the public
gateway APIs. The mock, inference listener, and tenant control listener are
ephemeral loopback services. The read-only GitHub job token is scoped to
artifact import and is absent from fixture processes. No stored repository
or provider secrets are required.

Three protocol groups cover eight distinct paths: minimum HTTP/1.1 (direct,
gateway and strict SQLite tenant), HTTP/2-enabled HTTP/1.1 (the same three),
and h2c (gateway and tenant). Every path rotates through five repeats, at four
conversations at a time. A repeat includes 128 JSON replies, 128 instant SSE
replies, 100 two-leg tool conversations, 20 paced ten-second SSE replies and
100 cancellations after three SSE data events. The fixture verifies real
transport, semantic first output and terminal content, tool arguments and
result, usage, provider counts and tenant receipt attribution. It checks the
receipt outcome of every intentional cancellation plus the successes in each
scenario and tool leg. Warmups are excluded from latency and throughput.

The runner collects all 23,040 measured requests and 40 trials on the baseline
before writing and hashing the budget. The candidate uses the same producer,
CPU/memory environment, inputs, sample count and scoring parameters. Rates,
startup, idle RSS and sampled peak RSS retain one value per trial. Quantiles
use nearest rank; the paced stream's latency quantiles pool the 100 samples per
path while keeping all five trial quantiles for dispersion. Its p99 is the
second-largest sample and is a thin estimate, not five independent tails.
The budget records both variability and the numerical allowances; direct
control drift and unresolved metrics prevent a clean verdict.

Public artifacts contain the synthetic summary, comparison, frozen budget,
build identity, and method. Individual trials stay on the runner and are not
uploaded. The fixture does not retain service configuration, tokens, databases,
gateway logs, full HTTP messages or the developer environment in its evidence.
A failed attempt publishes only the allowed aggregate files it completed;
never combine samples from different attempts. The benchmark is local gateway
evidence for this workload and hardware and does not measure remote provider
latency or other platforms.
