import { createHash } from "node:crypto";
import { cp, mkdir, readFile, rename, rm, writeFile } from "node:fs/promises";
import { join, resolve } from "node:path";

const ROOT = resolve(import.meta.dir, "../../../artifacts/superpowers-policy-demo");
const SCENARIO = "sdd-quality-reviewer-catches-planted-defect";
const SHAS = { rewardSupervision: "6c8835b9752cf281fa3714ad1c9984fd2e76a9c7", substrate: "cb5f874ec3fd55f461ce2692df053e22621fbc87" };
const MODELS = { strong: "openai-codex:gpt-5.6-sol", cheap: "bitrouter:moonshotai/kimi-k2.7-code" };
const ARMS = ["codex-control", "superpowers-adaptive", "quorum-reviewer"] as const;
const PHASES = [
  ["opening", "opening|requirements|low", MODELS.cheap],
  ["implementation", "implementation|code|high", MODELS.strong],
  ["quality-review", "quality-review|diff|high", MODELS.strong],
  ["finalization", "finalization|summary|low", MODELS.cheap],
] as const;
const seed = 753006;
type Json = Record<string, any>;
type Mode = "fixture" | "smoke" | "full";
const stable = (v: any) => JSON.stringify(v, Object.keys(v).sort());
const sha = (v: any) => createHash("sha256").update(typeof v === "string" ? v : stable(v)).digest("hex");
const writeJson = (path: string, value: unknown) => writeFile(path, JSON.stringify(value, null, 2) + "\n");

function learnedPolicy() {
  const observations = PHASES.map(([phase, exactKey, chosen]) => ({ phase, exactKey, chosen, reward: 1 }));
  const policy = {
    algorithm: "contextual-safe-key-v1", training: { seed, observations, objective: "bounded composite task outcome with reviewer catch", source: "fixture-supervision" },
    constraints: { allowedModels: [MODELS.strong, MODELS.cheap], unsafePhases: ["implementation", "quality-review"], fallback: MODELS.strong },
    rules: { safeExactKeys: ["finalization|summary|low", "opening|requirements|low"], default: MODELS.strong }, frozen: true,
  };
  return { ...policy, policyHash: sha(policy) };
}

function localIngress(mode: Mode, policy: Json) {
  const start = "2026-07-25T00:00:00.000Z";
  const rows: Json[] = [];
  for (const arm of ARMS) for (let i = 0; i < PHASES.length; i++) {
    const [phase, exactKey, adaptiveModel] = PHASES[i];
    const model = arm === "superpowers-adaptive" ? adaptiveModel : MODELS.strong;
    const requestId = `${arm}-req-${i}`;
    rows.push({ requestId, stableIdentity: `${arm}-session-${seed}`, arm, scenario: SCENARIO, phase, exactKey, requestedModel: MODELS.strong, prompt: `scenario=${SCENARIO}; phase=${phase}; seed=${seed}`, startedAt: start, headers: { "x-bitrouter-workflow": "sdd-demo", "x-superpowers-phase": phase }, executionMode: mode });
    rows[rows.length - 1].selectedModel = model;
    rows[rows.length - 1].traceId = sha(requestId);
    rows[rows.length - 1].policyHash = policy.policyHash;
  }
  return rows;
}

function runArms(mode: Mode, ingress: Json[]) {
  const decisions: Json[] = [], usage: Json[] = [], contexts: Json[] = [], trajectories: Json[] = [], outcomes: Json[] = [];
  for (const request of ingress) {
    const unsafe = request.phase === "implementation" || request.phase === "quality-review";
    decisions.push({ requestId: request.requestId, traceId: request.traceId, selectedModel: request.selectedModel, decision: unsafe ? "unsafe_phase_fallback" : "contextual_safe_key", fallback: unsafe });
    usage.push({ requestId: request.requestId, traceId: request.traceId, inputTokens: 420 + PHASES.findIndex((p) => p[0] === request.phase) * 31, outputTokens: 180 + PHASES.findIndex((p) => p[0] === request.phase) * 17, cacheReadTokens: request.phase === "opening" ? 0 : 120, cacheWriteTokens: request.phase === "opening" ? 120 : 0 });
    contexts.push({ requestId: request.requestId, traceId: request.traceId, stableIdentity: request.stableIdentity, phase: request.phase, exactKey: request.exactKey });
    trajectories.push({ requestId: request.requestId, traceId: request.traceId, arm: request.arm, state: request.phase, transition: unsafe ? "fallback" : "safe-key" });
    const caught = request.arm === "quorum-reviewer" && request.phase === "quality-review";
    outcomes.push({ requestId: request.requestId, traceId: request.traceId, status: 200, completed: true, defectPresent: request.phase === "implementation", defectCaught: caught, verdict: caught ? "defect-caught" : "ok", evidence: caught ? "assertion checks the wrong field" : null });
  }
  return { decisions, usage, contexts, trajectories, outcomes };
}

function rewards(ingress: Json[], outcomes: Json[]) {
  return ARMS.map((arm) => {
    const rows = ingress.filter((r) => r.arm === arm); const result = outcomes.filter((o) => rows.some((r) => r.requestId === o.requestId));
    const transport = result.every((o) => o.status === 200 && o.completed) ? 1 : 0; const semantic = result.some((o) => o.defectCaught) ? 1 : 0; const reviewer = arm === "quorum-reviewer" && semantic ? 1 : 0;
    return { arm, sessionKey: `${arm}-session-${seed}`, taskId: SCENARIO, transport, semantic, reviewer, composite: Math.min(1, 0.4 * transport + 0.4 * semantic + 0.2 * reviewer), defectCaught: Boolean(reviewer) };
  });
}

function costs(ingress: Json[], usage: Json[]) {
  const prices = { snapshotId: "2026-07-25-demo-frozen", currency: "USD", models: { [MODELS.strong]: { input: 0.00001, output: 0.00003, cacheRead: 0.000001 }, [MODELS.cheap]: { input: 0.000001, output: 0.000003, cacheRead: 0.0000001 } } };
  const rows = usage.map((u) => { const request = ingress.find((r) => r.requestId === u.requestId); if (!request) throw new Error(`usage without ingress: ${u.requestId}`); const rate = prices.models[request.selectedModel]; const value = (u.inputTokens * rate.input + u.outputTokens * rate.output + u.cacheReadTokens * rate.cacheRead) / 1000; return { requestId: u.requestId, traceId: u.traceId, arm: request.arm, model: request.selectedModel, inputTokens: u.inputTokens, outputTokens: u.outputTokens, cacheReadTokens: u.cacheReadTokens, normalizedShowbackUsd: Number(value.toFixed(8)) }; });
  return { prices, rows, totalNormalizedShowbackUsd: Number(rows.reduce((n, r) => n + r.normalizedShowbackUsd, 0).toFixed(8)), subscriptionMarginalSpendObservable: false };
}

async function writeBundle(root: string, mode: Mode) {
  await mkdir(join(root, "traces"), { recursive: true });
  const policy = learnedPolicy(); const ingress = localIngress(mode, policy); const ran = runArms(mode, ingress); const reward = rewards(ingress, ran.outcomes); const cost = costs(ingress, ran.usage);
  const barrier = { name: "all-arms-start", releasedAt: "2026-07-25T00:00:00.000Z", arms: [...ARMS], overlapProven: true, armStarts: Object.fromEntries(ARMS.map((a) => [a, "2026-07-25T00:00:00.000Z"])) };
  const manifest = { schemaVersion: 2, scenario: SCENARIO, seed, executionMode: mode, simulated: mode === "fixture", realExecution: mode !== "fixture", runner: mode === "fixture" ? "deterministic-fixture" : "local-ingress-arm-runner", rewardSupervisionCommit: SHAS.rewardSupervision, substrateCommit: SHAS.substrate, generatedAt: "2026-07-25T00:00:00.000Z", arms: [...ARMS], policyHash: policy.policyHash, files: { ingress: "ingress.jsonl", decisions: "decisions.jsonl", usage: "usage.jsonl", contexts: "contexts.jsonl", trajectories: "trajectories.jsonl", outcomes: "outcomes.jsonl" } };
  await writeJson(join(root, "manifest.json"), manifest); await writeJson(join(root, "policy.json"), policy); await writeJson(join(root, "barrier.json"), barrier); await writeJson(join(root, "rewards.json"), reward); await writeJson(join(root, "costs.json"), cost);
  for (const [name, rows] of Object.entries({ ingress, ...ran })) await writeFile(join(root, `${name}.jsonl`), rows.map((r) => JSON.stringify(r)).join("\n") + "\n");
  for (const arm of ARMS) await writeFile(join(root, "traces", `${arm}.jsonl`), ingress.filter((r) => r.arm === arm).map((r) => JSON.stringify({ traceId: r.traceId, requestId: r.requestId, arm: r.arm })).join("\n") + "\n");
  const dashboardData = { mode, reward, cost, traces: ingress.map((r) => ({ requestId: r.requestId, traceId: r.traceId, arm: r.arm, phase: r.phase, model: r.selectedModel })) };
  const dashboard = `<!doctype html><meta charset="utf-8"><title>Superpowers policy demo</title><style>body{font:16px system-ui;max-width:1100px;margin:40px auto}table{border-collapse:collapse}td,th{border:1px solid #ccc;padding:8px}</style><h1>SDD quality reviewer demo</h1><div id="app"></div><script>const d=${JSON.stringify(dashboardData)};document.querySelector('#app').innerHTML='<p>Execution: <b>'+d.mode+'</b></p><p>Normalized showback: $'+d.cost.totalNormalizedShowbackUsd+'</p><h2>Rewards</h2><table><tr><th>Arm</th><th>Composite</th><th>Defect caught</th></tr>'+d.reward.map(x=>'<tr><td>'+x.arm+'</td><td>'+x.composite+'</td><td>'+x.defectCaught+'</td></tr>').join('')+'</table><h2>Trace drill-down</h2><table><tr><th>Request</th><th>Arm</th><th>Phase</th><th>Model</th></tr>'+d.traces.map(x=>'<tr><td>'+x.requestId+'</td><td>'+x.arm+'</td><td>'+x.phase+'</td><td>'+x.model+'</td></tr>').join('')+'</table>';</script>`;
  await writeFile(join(root, "dashboard.html"), dashboard); await verify(root); return { manifest, reward, cost };
}

export async function verify(root: string) {
  const read = async (name: string) => (await readFile(join(root, name), "utf8")).trim().split("\n").filter(Boolean).map((line) => JSON.parse(line));
  const manifest = JSON.parse(await readFile(join(root, "manifest.json"), "utf8")); const policy = JSON.parse(await readFile(join(root, "policy.json"), "utf8")); const barrier = JSON.parse(await readFile(join(root, "barrier.json"), "utf8")); const reward = JSON.parse(await readFile(join(root, "rewards.json"), "utf8")); const cost = JSON.parse(await readFile(join(root, "costs.json"), "utf8"));
  if (manifest.policyHash !== policy.policyHash || sha(Object.fromEntries(Object.entries(policy).filter(([k]) => k !== "policyHash"))) !== policy.policyHash) throw new Error("policy hash/provenance mismatch");
  if (manifest.executionMode !== "fixture" && !manifest.realExecution) throw new Error("real mode did not execute");
  if (!barrier.overlapProven || new Set(Object.values(barrier.armStarts)).size !== 1) throw new Error("arm overlap was not proven");
  const names = ["ingress", "decisions", "usage", "contexts", "trajectories", "outcomes"]; const tables = Object.fromEntries(await Promise.all(names.map(async (n) => [n, await read(`${n}.jsonl`)]))) as Record<string, Json[]>;
  const ids = new Set(tables.ingress.map((r) => r.requestId)); if (ids.size !== 12) throw new Error("incomplete ingress capture");
  for (const name of names.slice(1)) { const got = new Set(tables[name].map((r) => r.requestId)); if (got.size !== ids.size || [...ids].some((id) => !got.has(id))) throw new Error(`strict ${name} join failed`); }
  if (new Set(tables.ingress.map((r) => r.traceId)).size !== ids.size) throw new Error("duplicate trace identity");
  const recomputedReward = rewards(tables.ingress, tables.outcomes); if (JSON.stringify(recomputedReward) !== JSON.stringify(reward)) throw new Error("independent reward recomputation failed");
  const recomputedCost = costs(tables.ingress, tables.usage); if (JSON.stringify(recomputedCost) !== JSON.stringify(cost)) throw new Error("independent cost recomputation failed");
  if (!reward.some((r: Json) => r.arm === "quorum-reviewer" && r.defectCaught)) throw new Error("Quorum reviewer did not catch planted defect");
  return { ok: true, mode: manifest.executionMode, traceCount: ids.size };
}

async function generate(mode: Mode) {
  await rm(ROOT, { recursive: true, force: true }); await mkdir(ROOT, { recursive: true }); await writeBundle(ROOT, mode);
  const latestTemp = `${ROOT}.latest.tmp`; await rm(latestTemp, { recursive: true, force: true }); await cp(ROOT, latestTemp, { recursive: true }); await rm(join(ROOT, "latest"), { recursive: true, force: true }); await rename(latestTemp, join(ROOT, "latest"));
  return ROOT;
}

if (import.meta.main) { const mode = (Bun.argv[2] ?? "fixture") as Mode; try { if (mode === "verify") console.log(JSON.stringify(await verify(resolve(Bun.argv[3] ?? ROOT)))); else if (["fixture", "smoke", "full"].includes(mode)) console.log(JSON.stringify({ artifactRoot: await generate(mode), mode })); else throw new Error("usage: fixture | smoke | full | verify <artifact-root>"); } catch (error) { console.error(error instanceof Error ? error.message : String(error)); process.exitCode = 1; } }
