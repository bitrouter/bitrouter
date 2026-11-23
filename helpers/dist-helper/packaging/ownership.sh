# Inserted into cargo-dist's install function after it resolves the layout.
# $1 and $2 are the extraction directory and the list of selected executables.
case "$APP_NAME:$2" in
    bitrouter:bitrouter|bitrouter:bitrouter.exe|bitrouter-bro:bro|bitrouter-bro:bro.exe) ;;
    *) err "Unexpected executable for $APP_NAME: $2" ;;
esac
owned_bin=$_install_dir/$2
owned_receipt=$RECEIPT_HOME/$APP_NAME-receipt.json
case "$2" in *.exe) ext=.exe ;; *) ext='' ;; esac

# Keep mkdir locks for older installers. New writers additionally hold kernel
# leases on stable files. A marked directory can be reused only under its lease.
lease_supported=1
if is_windows_posix || ! { command -v flock >/dev/null || command -v perl >/dev/null; }; then
    lease_supported=0
fi
take_dir() {
    local path="$1" fd="$2"
    if [ "$lease_supported" = 1 ]; then
        if [ -L "$path.lease" ] || { [ -e "$path.lease" ] && [ ! -f "$path.lease" ]; }; then
            err "Invalid installer lease at $path.lease"
        fi
        case "$fd" in 8) exec 8>>"$path.lease" ;; 9) exec 9>>"$path.lease" ;; esac
        if command -v flock >/dev/null; then
            flock -n "$fd" || err "Active installer at $path"
        else
            # macOS ships Perl. The parent shell retains the locked description.
            perl -e 'open(my $f, ">&=$ARGV[0]") or exit 1; flock($f, 6) or exit 1' "$fd" || err "Active installer at $path"
        fi
    fi
    if mkdir "$path" 2>/dev/null; then
        if [ "$lease_supported" = 1 ]; then ensure mkdir "$path/lease-v1"; fi
    elif [ "$lease_supported" != 1 ] || [ -L "$path" ] || [ -L "$path/lease-v1" ] || [ ! -d "$path/lease-v1" ]; then
        err "Unrecognized installer lock at $path. Stop product writers and preserve both lock directories, executable and receipts before manual recovery."
    fi
    # Never adopt an unknown journal.
    for entry in "$path"/* "$path"/.[!.]* "$path"/..?*; do
        [ -e "$entry" ] || [ -L "$entry" ] || continue
        case "$entry" in
            "$path/lease-v1") ;;
            "$path/receipt") [ "$fd" = 9 ] || err "Unexpected file at $entry" ;;
            *) err "Unexpected file at $entry. Preserve the installer lock for manual recovery." ;;
        esac
    done
}
release_dir() {
    [ -n "$1" ] || return 0
    # A staged receipt is the recovery journal. Keep its lease marker on error.
    if [ -e "$1/receipt" ] || [ -L "$1/receipt" ]; then return 0; fi
    if [ -d "$1/lease-v1" ]; then rmdir "$1/lease-v1" || return 1; fi
    rmdir "$1"
}
ensure mkdir -p "$_install_dir"
bin_lock=$_install_dir/.$APP_NAME-install-lock
take_dir "$bin_lock" 8
lock_dir=''
trap 'release_dir "$lock_dir"; release_dir "$bin_lock"' EXIT
trap 'exit 1' HUP INT TERM
if [ "$INSTALL_UPDATER" = 1 ]; then
    ensure mkdir -p "$RECEIPT_HOME"
    take_dir "$RECEIPT_HOME/.install-lock" 9
    lock_dir=$RECEIPT_HOME/.install-lock
fi

# A receipt is global to a user. Check its prefix even if this destination is
# empty so a second prefix cannot take over updates for the first.
if [ -L "$owned_receipt" ] || [ -L "$owned_bin" ]; then
    err "Refusing to replace linked installation at $owned_bin or $owned_receipt."
fi
matches_receipt() {
    printf '%s\n' "$RECEIPT" | jq -e --slurpfile previous "$1" --arg hash "$2" '
        ($previous | length) == 1 and ($previous[0] |
        .provider.source == "cargo-dist" and .binary_aliases == {} and
        (.version | type == "string") and (.binary_sha256 | test("^[0-9a-f]{64}$"))
        and ($hash == "" or .binary_sha256 == $hash))
        and (. as $next | $previous[0] | .source == $next.source and .binaries == $next.binaries
          and .install_prefix == $next.install_prefix and .install_layout == $next.install_layout)
    ' >/dev/null
}
if [ -f "$_install_dir/bro$ext" ] && [ "$_install_dir/bitrouter$ext" -ef "$_install_dir/bro$ext" ]; then
    err "Refusing to replace linked product executables in $_install_dir"
fi
if [ -n "$lock_dir" ] && { [ -e "$lock_dir/receipt" ] || [ -L "$lock_dir/receipt" ]; }; then
    need_cmd jq
    [ ! -L "$lock_dir/receipt" ] && [ -f "$lock_dir/receipt" ] && matches_receipt "$lock_dir/receipt" "" ||
        err "Interrupted receipt product or prefix mismatch at $lock_dir. Preserve the installation for manual recovery."
    if [ -e "$owned_receipt" ]; then
        [ -f "$owned_receipt" ] && matches_receipt "$owned_receipt" "" || err "Conflicting installed receipt at $owned_receipt"
    fi
    if [ -f "$owned_bin" ]; then
        owned_hash=$(binary_sha256 "$owned_bin") || err "Cannot hash $owned_bin"
        if matches_receipt "$lock_dir/receipt" "$owned_hash"; then
            if [ -f "$owned_receipt" ] && matches_receipt "$owned_receipt" "$owned_hash"; then
                jq -e --slurpfile prior "$owned_receipt" '.version == $prior[0].version' "$lock_dir/receipt" >/dev/null ||
                    err "Conflicting versions for the same executable. Preserve both receipts for manual recovery."
            fi
            ensure mv "$lock_dir/receipt" "$owned_receipt"
        elif [ -f "$owned_receipt" ] && matches_receipt "$owned_receipt" "$owned_hash"; then
            ensure rm "$lock_dir/receipt"
        else
            err "Executable matches neither interrupted receipt. Preserve $owned_bin, $owned_receipt and $lock_dir for manual rollback."
        fi
    elif [ ! -e "$owned_bin" ] && [ ! -e "$owned_receipt" ]; then
        ensure rm "$lock_dir/receipt" # The interrupted first install never placed its binary.
    else
        err "Incomplete interrupted installation. Preserve $owned_bin, $owned_receipt and $lock_dir for manual rollback."
    fi
fi
if [ -e "$owned_bin" ] || { [ "$INSTALL_UPDATER" = 1 ] && [ -e "$owned_receipt" ]; }; then
    [ "$INSTALL_UPDATER" = 1 ] && [ -f "$owned_bin" ] && [ -f "$owned_receipt" ] ||
        err "Existing $APP_NAME installation has no matching receipt and executable. Move the previous installation aside or update it with its original package manager."
    need_cmd jq
    owned_hash=$(binary_sha256 "$owned_bin") || err "Cannot hash $owned_bin"
    matches_receipt "$owned_receipt" "$owned_hash" ||
        err "Receipt, product, prefix or binary digest mismatch. Preserve the previous installation and receipt before migrating."
fi
if [ "$INSTALL_UPDATER" = 1 ]; then
    owned_hash=$(binary_sha256 "$1/$2") || err "Cannot hash the downloaded executable"
    # Build the final receipt before moving the binary. The generated installer
    # publishes it by rename only after a successful install.
    case "$owned_hash" in *[!0-9a-f]*|'') err "Invalid executable digest" ;; esac
    [ "${#owned_hash}" = 64 ] || err "Incomplete executable digest"
    staged=$(mktemp "$RECEIPT_HOME/.receipt.XXXXXXXX") || err "Cannot create temporary receipt"
    printf '{"binary_sha256":"%s",%s\n' "$owned_hash" "${RECEIPT#\{}" > "$staged" ||
        err "Could not stage the install receipt."
    ensure mv "$staged" "$lock_dir/receipt"
fi
