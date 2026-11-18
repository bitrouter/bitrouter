# Inserted into cargo-dist's install function after it resolves the layout.
# $1 and $2 are the extraction directory and the list of selected executables.
case "$APP_NAME:$2" in
    bitrouter:bitrouter|bitrouter:bitrouter.exe|bitrouter-bro:bro|bitrouter-bro:bro.exe) ;;
    *) err "Unexpected executable for $APP_NAME: $2" ;;
esac
owned_bin=$_install_dir/$2
owned_receipt=$RECEIPT_HOME/$APP_NAME-receipt.json
old_receipt=${RECEIPT_HOME%/*}/bitrouter/bitrouter-receipt.json
case "$2" in *.exe) ext=.exe ;; *) ext='' ;; esac

# Keep the receipt and binary replacement under one lock for this product.
# An unmanaged install owns only a fresh binary in the requested directory.
ensure mkdir -p "$_install_dir"
bin_lock=$_install_dir/.$APP_NAME-install-lock
mkdir "$bin_lock" || err "Install in progress. Check for an interrupted installer at $bin_lock."
lock_dir=''
trap 'if [ -n "$lock_dir" ]; then rm -f "$lock_dir/receipt"; rmdir "$lock_dir"; fi; rmdir "$bin_lock"' EXIT
trap 'exit 1' HUP INT TERM
if [ "$INSTALL_UPDATER" = 1 ]; then
    ensure mkdir -p "$RECEIPT_HOME"
    mkdir "$RECEIPT_HOME/.install-lock" || err "Install in progress. Check for an interrupted installer at $RECEIPT_HOME/.install-lock."
    lock_dir=$RECEIPT_HOME/.install-lock
fi

# The M0 shell installer used a symlink. Git Bash can encounter a hard link
# from the PowerShell installer. Confirm both receipt ownership and file identity.
if [ -f "$old_receipt" ] && [ -f "$_install_dir/bro$ext" ] &&
    [ "$_install_dir/bitrouter$ext" -ef "$_install_dir/bro$ext" ]; then
    need_cmd jq
    old_prefix=$(jq -er --arg bro "bro$ext" --arg alias "bitrouter$ext" '
        select(.provider.source == "cargo-dist" and .source.owner == "bitrouter"
          and .source.name == "bitrouter" and .source.app_name == "bitrouter"
          and .source.release_type == "github" and .binaries == [$bro]
          and .binary_aliases == {($bro): [$alias]})
        | if .install_layout == "cargo-home" then .install_prefix + "/bin"
          elif .install_layout == "flat" then .install_prefix else empty end
    ' "$old_receipt") || old_prefix=''
    if is_windows_posix && [ -n "$old_prefix" ]; then old_prefix=$(cygpath -u "$old_prefix"); fi
    if [ -d "$old_prefix" ] && [ "$old_prefix" -ef "$_install_dir" ]; then
        err "Verified legacy BitRouter installation. Stop M0, snapshot its data, and move bro$ext, its bitrouter$ext alias and $old_receipt together to a backup. Then run each desired product installer."
    fi
fi

# A receipt is global to a user. Check its prefix even if this destination is
# empty so a second prefix cannot take over updates for the first.
if [ -L "$owned_receipt" ] || [ -L "$owned_bin" ]; then
    err "Refusing to replace linked installation at $owned_bin or $owned_receipt."
fi
if [ -e "$owned_bin" ] || { [ "$INSTALL_UPDATER" = 1 ] && [ -e "$owned_receipt" ]; }; then
    [ "$INSTALL_UPDATER" = 1 ] && [ -f "$owned_bin" ] && [ -f "$owned_receipt" ] ||
        err "Existing $APP_NAME installation has no matching receipt and executable. Move the previous installation aside or update it with its original package manager."
    need_cmd jq
    owned_hash=$(binary_sha256 "$owned_bin") || err "Cannot hash $owned_bin"
    printf '%s\n' "$RECEIPT" | jq -e --slurpfile previous "$owned_receipt" --arg hash "$owned_hash" '
        ($previous | length) == 1 and ($previous[0] |
        .provider.source == "cargo-dist" and .binary_aliases == {} and .binary_sha256 == $hash)
        and (. as $next | $previous[0] | .source == $next.source and .binaries == $next.binaries
          and .install_prefix == $next.install_prefix and .install_layout == $next.install_layout)
    ' >/dev/null || err "Receipt, product, prefix or binary digest mismatch. Preserve the previous installation and receipt before migrating."
fi
if [ "$INSTALL_UPDATER" = 1 ]; then
    owned_hash=$(binary_sha256 "$1/$2") || err "Cannot hash the downloaded executable"
    # Build the final receipt before moving the binary. The generated installer
    # publishes it by rename only after a successful install.
    case "$owned_hash" in *[!0-9a-f]*|'') err "Invalid executable digest" ;; esac
    [ "${#owned_hash}" = 64 ] || err "Incomplete executable digest"
    printf '{"binary_sha256":"%s",%s\n' "$owned_hash" "${RECEIPT#\{}" > "$lock_dir/receipt" ||
        err "Could not stage the install receipt."
fi
