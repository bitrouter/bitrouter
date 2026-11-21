# Inserted after cargo-dist resolves the destination and before it writes files.
$exe = if ($app_name -eq 'bitrouter') { 'bitrouter.exe' } else { 'bro.exe' }
if (@($info.bins).Count -ne 1 -or $info.bins[0] -cne $exe -or $info.aliases_json -ne '{}') {
  throw "Unexpected executable or aliases for $app_name"
}
$binary = Join-Path $dest_dir $exe
$receiptFile = Join-Path $receipt_home "$app_name-receipt.json"
[IO.Directory]::CreateDirectory($dest_dir) | Out-Null
$binLock = Join-Path $dest_dir ".$app_name-install-lock"
# The streams stay open until the outer finally. Lock the first byte so a
# native Rust updater using LockFileEx and another installer exclude each other.
function Enter-InstallerLock($path, $allowReceipt) {
  $leasePath = "$path.lease"
  $leaseItem = Get-Item -LiteralPath $leasePath -Force -ErrorAction SilentlyContinue
  if ($leaseItem -and ($leaseItem.PSIsContainer -or $leaseItem.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
    throw "Invalid installer lease at $leasePath"
  }
  $stream = [IO.File]::Open($leasePath, 'OpenOrCreate', 'ReadWrite', 'ReadWrite')
  try {
    try { $stream.Lock(0, 1) } catch [IO.IOException] { throw "Active installer at $path" }
    $item = Get-Item -LiteralPath $path -Force -ErrorAction SilentlyContinue
    $marker = Join-Path $path 'lease-v1'
    if ($item) {
      $mark = Get-Item -LiteralPath $marker -Force -ErrorAction SilentlyContinue
      if (!$item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -or
          !$mark -or !$mark.PSIsContainer -or ($mark.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        throw "Unrecognized installer lock at $path. Stop product writers and preserve both lock directories, executable and receipts before manual recovery."
      }
    } else {
      $null = New-Item -Path $path -ItemType Directory -ErrorAction Stop
      $null = New-Item -Path $marker -ItemType Directory -ErrorAction Stop
    }
    foreach ($entry in [IO.Directory]::EnumerateFileSystemEntries($path)) {
      $name = [IO.Path]::GetFileName($entry)
      if ($name -cne 'lease-v1' -and !($allowReceipt -and $name -cin 'receipt','previous.exe')) {
        throw "Unexpected file at $entry. Preserve the installer lock for manual recovery."
      }
    }
    return $stream
  } catch {
    $stream.Dispose()
    throw
  }
}
function Exit-InstallerLock($path, $stream) {
  if (!$stream) { return }
  try {
    $entries = @([IO.Directory]::EnumerateFileSystemEntries($path))
    if ($entries.Count -eq 1 -and [IO.Path]::GetFileName($entries[0]) -ceq 'lease-v1') {
      [IO.Directory]::Delete((Join-Path $path 'lease-v1'))
      [IO.Directory]::Delete($path)
    }
  } finally { $stream.Dispose() }
}
$receiptLock = Join-Path $receipt_home '.install-lock'
$binLease = Enter-InstallerLock $binLock $false
$receiptLease = $null
try {
  if ($install_updater) {
    [IO.Directory]::CreateDirectory($receipt_home) | Out-Null
    $receiptLease = Enter-InstallerLock $receiptLock $true
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
  $pending = Join-Path $receiptLock 'receipt'
  function Read-OwnedReceipt($item) {
    if ($item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
      throw "Refusing to replace linked receipt at $($item.FullName)"
    }
    $r = Get-Content -Raw -LiteralPath $item.FullName | ConvertFrom-Json -ErrorAction Stop
    if ($r.source.app_name -cne $app_name -or $r.source.name -cne 'bitrouter' -or
        $r.source.owner -cne 'bitrouter' -or $r.source.release_type -cne 'github' -or
        $r.provider.source -cne 'cargo-dist' -or @($r.binaries).Count -ne 1 -or $r.binaries[0] -cne $exe -or
        $null -eq $r.binary_aliases -or @($r.binary_aliases.psobject.Properties).Count -ne 0 -or
        $r.install_prefix -ne $receipt_dest_dir -or $r.install_layout -cne $install_layout -or
        !$r.version -or $r.binary_sha256 -cnotmatch '^[a-f0-9]{64}$') {
      throw "Receipt, product, prefix or binary digest mismatch at $($item.FullName). Preserve the installation for manual rollback."
    }
    return $r
  }
  if ($existing -and ($existing.PSIsContainer -or $existing.LinkType)) {
    throw "Refusing to replace a linked or unowned installation at $binary"
  }
  $journal = if ($install_updater) { Get-Item -LiteralPath $pending -Force -ErrorAction SilentlyContinue }
  # A Windows self-update can stop after relocating its running image. Only
  # restore its recovery copy when the previous receipt still binds these bytes.
  $backup = if ($install_updater) { Get-Item -LiteralPath (Join-Path $receiptLock 'previous.exe') -Force -ErrorAction SilentlyContinue }
  if ($backup -and (!$record -or !$journal -or $backup.PSIsContainer -or $backup.LinkType)) {
    throw "Incomplete Windows recovery evidence at $receiptLock. Preserve the executable and receipts for manual rollback."
  }
  if ($journal) {
    $next = Read-OwnedReceipt $journal
    $prior = if ($record) { Read-OwnedReceipt $record }
    if ($backup) {
      if ((Get-FileHash -LiteralPath $backup.FullName -Algorithm SHA256).Hash -ne $prior.binary_sha256) {
        throw "Windows recovery executable conflicts with $receiptFile. Preserve $receiptLock for manual rollback."
      }
      if (!$existing) {
        $staged = Join-Path $dest_dir ([IO.Path]::GetRandomFileName())
        try {
          [IO.File]::Copy($backup.FullName, $staged)
          [IO.File]::Move($staged, $binary)
        } finally { if ([IO.File]::Exists($staged)) { [IO.File]::Delete($staged) } }
        $existing = Get-Item -LiteralPath $binary
      }
    }
    $hash = if ($existing) { (Get-FileHash -LiteralPath $binary -Algorithm SHA256).Hash }
    if ($prior -and $hash -eq $next.binary_sha256 -and $hash -eq $prior.binary_sha256 -and $next.version -cne $prior.version) {
      throw "Conflicting versions for the same executable. Preserve both receipts for manual recovery."
    }
    if ($backup -and ($hash -eq $next.binary_sha256 -or $hash -eq $prior.binary_sha256)) {
      [IO.File]::Delete($backup.FullName)
    }
    if ($hash -and $hash -eq $next.binary_sha256) {
      if ($record) { [IO.File]::Replace($pending, $receiptFile, [Management.Automation.Language.NullString]::Value) }
      else { [IO.File]::Move($pending, $receiptFile) }
      $record = Get-Item -LiteralPath $receiptFile
    } elseif ((!$existing -and !$record) -or ($hash -and $prior -and $hash -eq $prior.binary_sha256)) {
      [IO.File]::Delete($pending)
    } else {
      throw "Executable matches neither interrupted receipt. Preserve $binary, $receiptFile and $receiptLock for manual rollback."
    }
  }
  if ($existing -or ($install_updater -and $record)) {
    if (!$install_updater -or !$existing -or !$record) {
      throw "Refusing to replace an unowned installation at $binary or $receiptFile."
    }
    $previous = Read-OwnedReceipt $record
    if ($previous.binary_sha256 -ne (Get-FileHash -LiteralPath $binary -Algorithm SHA256).Hash) {
      throw "Receipt, product, prefix or binary digest mismatch. Preserve the installation for manual rollback."
    }
  }
