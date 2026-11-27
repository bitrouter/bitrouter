const fs = require("node:fs"),
  os = require("node:os"),
  path = require("node:path"),
  cp = require("node:child_process"),
  assert = require("node:assert/strict");
const script = path.resolve(".github/scripts/registry-propose.sh");
const root = fs.mkdtempSync(path.join(os.tmpdir(), "registry-propose-test-"));
const bare = path.join(root, "origin.git"),
  repo = path.join(root, "repo"),
  bin = path.join(root, "bin");
fs.mkdirSync(repo);
fs.mkdirSync(bin);
function git(args) {
  return cp
    .execFileSync("git", args, {
      cwd: repo,
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
    })
    .trim();
}
cp.execFileSync("git", ["init", "--bare", bare], { stdio: "ignore" });
git(["init", "-b", "main"]);
git(["config", "user.name", "bitrouter-automation[bot]"]);
git([
  "config",
  "user.email",
  "267229870+bitrouter-automation[bot]@users.noreply.github.com",
]);
git(["remote", "add", "origin", bare]);
fs.mkdirSync(path.join(repo, "registry"));
fs.mkdirSync(path.join(repo, "target"));
fs.writeFileSync(path.join(repo, "registry/models.yaml"), "accepted\n");
for (const dir of ["dist/registry", "apps/bitrouter/registry-dist"]) {
  fs.mkdirSync(path.join(repo, dir), { recursive: true });
  fs.writeFileSync(path.join(repo, dir, "registry.json"), "{}");
}
git(["add", "."]);
git(["commit", "-m", "initial"]);
git(["push", "origin", "main"]);
const base = git(["rev-parse", "HEAD"]);
git(["checkout", "--detach", base]);
git(["checkout", "--detach", base]);
const state = path.join(root, "pr.json"),
  calls = path.join(root, "calls");
fs.writeFileSync(state, "{}");
fs.writeFileSync(calls, "");
fs.writeFileSync(
  path.join(bin, "gh"),
  '#!/usr/bin/env bun\nconst fs=require("fs");const a=process.argv.slice(2);if(a[0]==="pr"&&a[1]==="list"){console.log(fs.readFileSync(process.env.TEST_PR_STATE,"utf8"));}else{fs.appendFileSync(process.env.TEST_GH_CALLS,a.join(" ")+"\\n");}\n',
  { mode: 0o755 },
);
const env = {
  ...process.env,
  PATH: bin + path.delimiter + process.env.PATH,
  REGISTRY_BASE: "main",
  REGISTRY_BRANCH: "automation/test",
  REGISTRY_BASE_SHA: base,
  REGISTRY_PROPOSAL_SHA: "",
  RUNNER_TEMP: root,
  TEST_PR_STATE: state,
  TEST_GH_CALLS: calls,
};
function run(success = true) {
  let r = cp.spawnSync("bash", [script], { cwd: repo, env, encoding: "utf8" });
  assert.equal(r.status, success ? 0 : 1, r.stdout + r.stderr);
  return r.stdout;
}
assert.match(run(), /no commit or PR needed/);
assert.equal(git(["rev-parse", "HEAD"]), base);
fs.writeFileSync(
  path.join(repo, "target/registry-sync.json"),
  JSON.stringify({
    audit: { outcome: "passed" },
    identity_repairs: [],
    feeds: [],
  }),
);
fs.writeFileSync(path.join(repo, "registry/new.yaml"), "changed\n");
assert.match(run(), /chore: sync registry catalog/);
const proposal = git(["rev-parse", "HEAD"]);
assert.notEqual(proposal, base);
assert.match(fs.readFileSync(calls, "utf8"), /pr create/);
fs.writeFileSync(
  state,
  JSON.stringify({ number: 1, isDraft: false, headRefOid: proposal }),
);
env.REGISTRY_PROPOSAL_SHA = proposal;
git(["reset", "--hard", base]);
fs.writeFileSync(path.join(repo, "registry/new.yaml"), "changed\n");
assert.match(run(), /no commit or PR update needed/);
assert.equal(git(["rev-parse", "HEAD"]), base);
git(["reset", "--hard", base]);
assert.match(run(), /Closed superseded/);
assert.match(fs.readFileSync(calls, "utf8"), /pr close 1/);
// An obsolete captured head cannot overwrite a later writer.
env.REGISTRY_PROPOSAL_SHA = "";
assert.match(run(false), /changed during fetch/);
// Human edits on the proposal are protected.
git(["checkout", "--detach", proposal]);
git(["config", "user.email", "human@example.test"]);
fs.writeFileSync(path.join(repo, "registry/human.yaml"), "manual\n");
git(["add", "registry/human.yaml"]);
git(["commit", "-m", "human edit"]);
const human = git(["rev-parse", "HEAD"]);
git(["push", "--force", "origin", "HEAD:automation/test"]);
git(["checkout", "--detach", base]);
env.REGISTRY_PROPOSAL_SHA = human;
fs.writeFileSync(
  state,
  JSON.stringify({ number: 1, isDraft: false, headRefOid: human }),
);
const result = cp.spawnSync("bash", [script], {
  cwd: repo,
  env,
  encoding: "utf8",
});
assert.equal(result.status, 1);
assert.match(result.stdout, /human edits/);
console.log(
  JSON.stringify({
    cases: [
      "accepted no-op",
      "untracked content creates PR",
      "identical pending no-op",
      "superseded PR closed",
      "stale head rejected",
      "human edits protected",
    ],
    result: "passed",
  }),
);

fs.rmSync(root, { recursive: true, force: true });
