// Compose two ordinary product installers from a workspace release. Each
// native executable keeps the install and upgrade rules of its owning channel.
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

function insert(script, marker, replacement) {
  assert.equal(script.split(marker).length, 2, `cargo-dist template changed at ${marker}`);
  return script.replace(marker, () => replacement);
}

function writeNpmMetadata(dir, digests) {
  const path = join(dir, 'package.json');
  const pkg = JSON.parse(readFileSync(path, 'utf8'));
  for (const platform of Object.values(pkg.supportedPlatforms)) {
    platform.sha256 = digests.get(platform.artifactName);
  }
  writeFileSync(path, JSON.stringify(pkg, null, 2) + '\n');
  const installer = join(dir, 'binary-install.js');
  let template = readFileSync(installer, 'utf8');
  // The Windows release ZIP is flat. Use the same native extraction tool as
  // the Rust updater, with separate arguments for each path.
  const windows = template.indexOf('                  // Windows does not have "unzip"');
  const otherZip = template.indexOf('\n                } else {\n                  result = spawnSync("unzip"', windows);
  assert.ok(windows >= 0 && otherZip > windows, 'Windows npm extractor template changed');
  template = template.slice(0, windows)
    + '                  result = spawnSync("tar.exe", ["xf", tempFile, "-C", this.installDirectory]);'
    + template.slice(otherZip);
  const extracted = '      .then(() => {\n        if (!suppressLogs) {';
  template = insert(template, extracted, `      .then(() => {
        if (!this.exists()) throw new Error(\`Archive did not install \${this.name}\`);
        if (!suppressLogs) {`);
  const verify = `sink.on("close", async () => {
              const { createReadStream, rmSync } = require("node:fs");
              try {
                const expected = this.platform.sha256;
                if (!/^[a-f0-9]{64}$/.test(expected || "")) {
                  throw new Error(\`Missing SHA-256 for \${this.filename}\`);
                }
                const hash = require("node:crypto").createHash("sha256");
                for await (const chunk of createReadStream(tempFile)) {
                  hash.update(chunk);
                }
                if (hash.digest("hex") !== expected) {
                  throw new Error(\`Archive checksum mismatch for \${this.filename}\`);
                }
              } catch (error) {
                rmSync(directory, { recursive: true, force: true });
                reject(error);
                return;
              }
              try {`;
  const guarded = insert(template, 'sink.on("close", () => {', verify);
  const end = '            });\n          });\n        });\n      })';
  writeFileSync(installer, insert(guarded, end, `              } finally {
                rmSync(directory, { recursive: true, force: true });
              }
${end}`));
  return pkg;
}

function packageNpm(uploads, digests, bundled) {
  const bro = uploads.get('bitrouter-bro-npm-package.tar.gz');
  const core = uploads.get('bitrouter-npm-package.tar.gz');
  const scratch = mkdtempSync(join(tmpdir(), 'bitrouter-npm-'));
  try {
    execFileSync('tar', ['xf', bro, '-C', scratch]);
    const root = join(scratch, 'package');
    const pkg = writeNpmMetadata(root, digests);
    writeFileSync(join(root, 'README.md'), [
      '# BitRouter BRO',
      '',
      bundled ? 'This package installs the `bro` and `bitrouter` commands.' : 'This package installs the `bro` command.',
      '',
      'See [Diagnosing BitRouter](https://github.com/bitrouter/bitrouter/blob/main/skills/bitrouter/references/diagnose.md) for installation and runtime checks.',
      '',
    ].join('\n'));
    if (bundled) {
      const nested = join(root, 'core');
      mkdirSync(nested);
      execFileSync('tar', ['xf', core, '--strip-components', '1', '-C', nested]);
      const peer = writeNpmMetadata(nested, digests);
      assert.equal(peer.version, pkg.version);
      assert.deepEqual(peer.artifactDownloadUrls, pkg.artifactDownloadUrls);
      assert.deepEqual(peer.dependencies, pkg.dependencies);
      pkg.bin.bitrouter = 'core/run-bitrouter.js';
      writeFileSync(join(root, 'package.json'), JSON.stringify(pkg, null, 2) + '\n');
      writeFileSync(join(root, 'install.js'), `#!/usr/bin/env node
// Require the same version of both native programs before npm reports success.
Promise.resolve(require("./binary").install(false))
  .then(() => require("./core/binary").install(false))
  .catch(error => { console.error(error.message); process.exitCode = 1; });
`);
    }
    execFileSync('tar', ['czf', bro, '-C', scratch, 'package']);
  } finally {
    rmSync(scratch, { recursive: true, force: true });
  }
}

function bundleWindows(uploads) {
  const bro = uploads.get('bitrouter-bro-installer.ps1');
  const core = uploads.get('bitrouter-installer.ps1');
  // The standalone entry point exits with an error code. The combined entry
  // point catches the same exception and reports which product succeeded.
  const guarded = insert(readFileSync(core, 'utf8'),
    '  Write-Information $_\n  exit 1\n}', '  throw\n}');
  const peer = Buffer.from(guarded).toString('base64');
  let ps = readFileSync(bro, 'utf8');
  ps = insert(ps, '[switch]$Help\n)', `[switch]$Help,
    [Parameter(HelpMessage = "Install only the BRO harness")]
    [switch]$BroOnly
)

if (!$Help -and !$BroOnly -and ($env:BITROUTER_BRO_UNMANAGED_INSTALL -or
    $env:BITROUTER_UNMANAGED_INSTALL -or $env:BITROUTER_BRO_DISABLE_UPDATE -or
    $env:BITROUTER_DISABLE_UPDATE)) {
  throw "The combined installer requires managed installations. Use -BroOnly to install BRO separately."
}`);
  writeFileSync(bro, ps + `
# The peer installer is generated and guarded for this same release. Execute
# it in a child scope and keep both products' independently recoverable receipts.
if (!$Help -and !$BroOnly) {
  $coreInstaller = [ScriptBlock]::Create([Text.Encoding]::UTF8.GetString(
    [Convert]::FromBase64String('${peer}')))
  $previousInstallDir = $env:BITROUTER_INSTALL_DIR
  try {
    if (!$previousInstallDir) { $env:BITROUTER_INSTALL_DIR = $env:BITROUTER_BRO_INSTALL_DIR }
    & $coreInstaller -NoModifyPath:$NoModifyPath
  } catch {
    throw "BRO was installed. Retry this installer to finish installing the BitRouter core. $_"
  } finally {
    $env:BITROUTER_INSTALL_DIR = $previousInstallDir
  }
}
`);
}

export function bundleBroInstallers(manifest, uploads, digests) {
  const packages = new Set(manifest.releases.map(release => release.app_name));
  if (!packages.has('bitrouter-bro')) return;
  const bundled = packages.has('bitrouter');
  packageNpm(uploads, digests, bundled);
  if (bundled) {
    const formula = uploads.get('bitrouter-bro.rb');
    const brew = readFileSync(formula, 'utf8');
    assert.ok(brew.startsWith('class BitrouterBro < Formula\n'));
    writeFileSync(formula, brew.replace('class BitrouterBro < Formula\n',
      'class BitrouterBro < Formula\n  depends_on "bitrouter/tap/bitrouter"\n'));
    bundleWindows(uploads);
  }
}
