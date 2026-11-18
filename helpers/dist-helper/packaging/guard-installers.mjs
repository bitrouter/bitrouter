// cargo-dist 0.31 resolves platform, download URL, checksums, install layout and
// updater environment variables. Guard its resolved destination before replacement.
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { readFileSync, writeFileSync } from 'node:fs';
import { basename } from 'node:path';

const manifest = JSON.parse(readFileSync(process.argv[2], 'utf8'));
const uploads = new Map(manifest.upload_files.map(path => [basename(path), path]));
const shGuard = readFileSync(new URL('ownership.sh', import.meta.url), 'utf8');
const psGuard = readFileSync(new URL('ownership.ps1', import.meta.url), 'utf8');

function replaceOnce(script, marker, replacement) {
  assert.equal(script.split(marker).length, 2, `cargo-dist template changed at ${marker}`);
  return script.replace(marker, () => replacement);
}

for (const { app_name: pkg } of manifest.releases) {
  assert.ok(['bitrouter', 'bitrouter-bro'].includes(pkg));
  const bin = pkg === 'bitrouter' ? 'bitrouter' : 'bro';
  const asset = suffix => uploads.get(pkg + suffix);
  const npm = JSON.parse(execFileSync('tar', ['xOf', asset('-npm-package.tar.gz'), 'package/package.json']));
  assert.equal(npm.name, pkg);
  assert.deepEqual(Object.keys(npm.bin), [bin]);
  const brew = readFileSync(asset('.rb'), 'utf8');
  assert.deepEqual([...new Set([...brew.matchAll(/bin\.install "([^"]+)"/g)].map(m => m[1]))], [bin]);
  assert.match(brew, /BINARY_ALIASES = \{\s*(?:"[^"]+": \{\},?\s*)+\}/);

  const shellFile = asset('-installer.sh');
  let shell = readFileSync(shellFile, 'utf8');
  const install = 'install() {';
  shell = replaceOnce(shell, install, `binary_sha256() {
    local sum
    if command -v sha256sum >/dev/null; then
        sum=$(sha256sum -b < "$1") || return 1
    else
        sum=$(shasum -a 256 -b < "$1") || return 1
    fi
    printf '%s\\n' "\${sum%% *}"
}

verify_archive_sha256() {
    local actual
    actual=$(binary_sha256 "$1") || err "Cannot calculate archive digest"
    [ "\${#actual}" = 64 ] && [ "$actual" = "$2" ] || err "Archive checksum mismatch for $3"
}

${install}`);
  // Some hosts have shasum but lack sha256sum. Always verify before extraction.
  shell = replaceOnce(shell, '            verify_checksum "$_file" "$_checksum_style" "$_checksum_value"', `            [ "$_checksum_style" = sha256 ] || err "Expected an archive SHA-256"
            verify_archive_sha256 "$_file" "$_checksum_value" "$_artifact_name"`);
  shell = replaceOnce(shell, '            say "no checksums to verify" 1>&2', `            ensure downloader "$_url.sha256" "$_dir/checksum"
            read -r expected_digest checksum_file < "$_dir/checksum"
            verify_archive_sha256 "$_file" "$expected_digest" "$_artifact_name"`);
  const destination = '    say "installing to $_install_dir"';
  shell = replaceOnce(shell, destination, shGuard + '\n' + destination);
  shell = replaceOnce(shell, 'echo "$RECEIPT" > "$RECEIPT_HOME/$APP_NAME-receipt.json"',
    'ensure mv "$lock_dir/receipt" "$owned_receipt"');
  writeFileSync(shellFile, shell);

  const psFile = asset('-installer.ps1');
  let ps = readFileSync(psFile, 'utf8');
  const downloaded = '  $wc.downloadFile($url, $dir_path)';
  ps = replaceOnce(ps, downloaded, downloaded + `
  $expectedHash = ($wc.DownloadString("$url.sha256").Trim() -split '\\s+')[0]
  if ((Get-FileHash -LiteralPath $dir_path -Algorithm SHA256).Hash -ne $expectedHash) {
    throw [System.IO.InvalidDataException]::new("Archive checksum mismatch for $artifact_name")
  }`);
  // Integrity failures are terminal. cargo-dist's mirror retry is for transfer
  // failures and otherwise hides the reason that a damaged archive was refused.
  const retry = '    } catch {\n      Write-Information "failed to download from $url"';
  ps = replaceOnce(ps, retry, '    } catch [System.IO.InvalidDataException] {\n      throw\n' + retry);
  const mkdir = '  $dest_dir = New-Item -Force -ItemType Directory -Path $dest_dir';
  // Format cargo-dist's receipt while the archive still lives in staging.
  const start = ps.indexOf('  $formatted_bins =');
  const finish = ps.indexOf('  # Write the install receipt', start);
  assert.ok(start > 0 && finish > start);
  const formatReceipt = ps.slice(start, finish);
  ps = replaceOnce(ps, formatReceipt, '');
  ps = replaceOnce(ps, mkdir, psGuard + '\n' + formatReceipt + `
  if ($install_updater) {
    $finalReceipt = $receipt | ConvertFrom-Json -ErrorAction Stop
    $hash = (Get-FileHash -LiteralPath $artifacts.bin_paths[0] -Algorithm SHA256).Hash.ToLowerInvariant()
    $finalReceipt | Add-Member -NotePropertyName binary_sha256 -NotePropertyValue $hash
    $staged = Join-Path $receipt_home ([IO.Path]::GetRandomFileName())
    try {
      [IO.File]::WriteAllText($staged, ($finalReceipt | ConvertTo-Json -Depth 5), (New-Object System.Text.UTF8Encoding $false))
      [IO.File]::Move($staged, $pending)
    } finally { if ([IO.File]::Exists($staged)) { [IO.File]::Delete($staged) } }
  }
` + mkdir);
  // Replace by rename on the destination volume, including Windows rollback.
  ps = replaceOnce(ps, '    Copy-Item "$bin_path" -Destination "$dest_dir" -ErrorAction Stop', `
    $staged = Join-Path $dest_dir ([IO.Path]::GetRandomFileName())
    try {
      [IO.File]::Copy($bin_path, $staged)
      if ($existing) { [IO.File]::Replace($staged, $binary, [Management.Automation.Language.NullString]::Value) }
      else { [IO.File]::Move($staged, $binary) }
    } finally {
      if ([IO.File]::Exists($staged)) { [IO.File]::Delete($staged) }
    }`);
  ps = replaceOnce(ps, `[IO.File]::WriteAllLines("$receipt_home/${pkg}-receipt.json", "$receipt", $Utf8NoBomEncoding)`, `
    if ($record) { [IO.File]::Replace($pending, $receiptFile, [Management.Automation.Language.NullString]::Value) }
    else { [IO.File]::Move($pending, $receiptFile) }`);
  const end = '\n}\n\n# Attempt to do CI-specific rituals';
  ps = replaceOnce(ps, end, `
  } finally {
    try { Exit-InstallerLock $receiptLock $receiptLease } finally { Exit-InstallerLock $binLock $binLease }
  }` + end);
  writeFileSync(psFile, ps);
}
