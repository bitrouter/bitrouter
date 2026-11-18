# Inserted after cargo-dist resolves the destination and before it writes files.
$exe = if ($app_name -eq 'bitrouter') { 'bitrouter.exe' } else { 'bro.exe' }
if (@($info.bins).Count -ne 1 -or $info.bins[0] -cne $exe -or $info.aliases_json -ne '{}') {
  throw "Unexpected executable or aliases for $app_name"
}
$binary = Join-Path $dest_dir $exe
$receiptFile = Join-Path $receipt_home "$app_name-receipt.json"
[IO.Directory]::CreateDirectory($dest_dir) | Out-Null
$binLock = Join-Path $dest_dir ".$app_name-install-lock"
$null = New-Item -Path $binLock -ItemType Directory -ErrorAction Stop
$receiptLock = $null
try {
  if ($install_updater) {
    [IO.Directory]::CreateDirectory($receipt_home) | Out-Null
    $receiptLock = New-Item -Path (Join-Path $receipt_home '.install-lock') -ItemType Directory -ErrorAction Stop
  }
  $legacyFile = Join-Path (Split-Path $receipt_home) 'bitrouter\bitrouter-receipt.json'
  $aliasFile = Get-Item -LiteralPath (Join-Path $dest_dir 'bitrouter.exe') -Force -ErrorAction SilentlyContinue
  if ($aliasFile -and ($aliasFile.LinkType -in 'HardLink','SymbolicLink') -and (Test-Path -LiteralPath $legacyFile)) {
    $legacy = Get-Content -Raw -LiteralPath $legacyFile | ConvertFrom-Json -ErrorAction Stop
    $legacyBin = switch ($legacy.install_layout) {
      'cargo-home' { Join-Path $legacy.install_prefix 'bin' }
      'flat' { $legacy.install_prefix }
    }
    $broFile = [IO.Path]::GetFullPath((Join-Path $dest_dir 'bro.exe'))
    if ($legacyBin -and [IO.Path]::GetFullPath($legacyBin) -eq [IO.Path]::GetFullPath($dest_dir) -and
        $legacy.provider.source -eq 'cargo-dist' -and $legacy.source.app_name -eq 'bitrouter' -and
        $legacy.source.owner -eq 'bitrouter' -and $legacy.source.name -eq 'bitrouter' -and
        $legacy.source.release_type -eq 'github' -and ($legacy.binaries -join ',') -eq 'bro.exe' -and
        ($legacy.binary_aliases.'bro.exe' -join ',') -eq 'bitrouter.exe' -and $aliasFile.Target -contains $broFile) {
      throw "Verified legacy BitRouter installation. Stop M0, snapshot its data, and move bro.exe, its bitrouter.exe alias and $legacyFile together to a backup. Then run each desired product installer."
    }
  }
  $existing = Get-Item -LiteralPath $binary -Force -ErrorAction SilentlyContinue
  $record = Get-Item -LiteralPath $receiptFile -Force -ErrorAction SilentlyContinue
  if ($existing -or ($install_updater -and $record)) {
    if (!$install_updater -or !$existing -or !$record -or $existing.PSIsContainer -or $existing.LinkType -or
        $record.PSIsContainer -or $record.LinkType) {
      throw "Refusing to replace a linked or unowned installation at $binary or $receiptFile. Preserve the previous installation before migrating."
    }
    $previous = Get-Content -Raw -LiteralPath $receiptFile | ConvertFrom-Json -ErrorAction Stop
    $hash = (Get-FileHash -LiteralPath $binary -Algorithm SHA256).Hash
    if ($previous.source.app_name -cne $app_name -or $previous.source.name -cne 'bitrouter' -or
        $previous.source.owner -cne 'bitrouter' -or $previous.source.release_type -cne 'github' -or
        $previous.provider.source -cne 'cargo-dist' -or ($previous.binaries -join ',') -cne $exe -or
        $null -eq $previous.binary_aliases -or @($previous.binary_aliases.psobject.Properties).Count -ne 0 -or
        $previous.install_prefix -ne $receipt_dest_dir -or $previous.install_layout -ne $install_layout -or
        $previous.binary_sha256 -ne $hash) {
      throw "Receipt, product, prefix or binary digest mismatch. Preserve the previous installation and receipt before migrating."
    }
  }
  $binaryHash = (Get-FileHash -LiteralPath $artifacts.bin_paths[0] -Algorithm SHA256).Hash.ToLowerInvariant()
