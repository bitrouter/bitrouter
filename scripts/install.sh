#!/usr/bin/env sh
set -eu

OWNER="bitrouter"
REPO="bitrouter"

say() {
    printf '%s\n' "$*"
}

fail() {
    printf 'bitrouter install: %s\n' "$*" >&2
    exit 1
}

have_cmd() {
    command -v "$1" >/dev/null 2>&1
}

need_cmd() {
    have_cmd "$1" || fail "required command not found: $1"
}

download_text() {
    url=$1

    if have_cmd curl; then
        curl --fail --silent --show-error --location \
            --header "Accept: application/vnd.github+json" \
            --header "User-Agent: bitrouter-install-script" \
            "$url"
        return
    fi

    if have_cmd wget; then
        wget -qO- \
            --header="Accept: application/vnd.github+json" \
            --header="User-Agent: bitrouter-install-script" \
            "$url"
        return
    fi

    fail "either curl or wget is required"
}

download_file() {
    url=$1
    output=$2

    if have_cmd curl; then
        curl --fail --silent --show-error --location \
            --output "$output" \
            "$url"
        return
    fi

    if have_cmd wget; then
        wget -qO "$output" "$url"
        return
    fi

    fail "either curl or wget is required"
}

extract_tag_name() {
    if have_cmd jq; then
        jq -r '.tag_name // empty'
        return
    fi

    if have_cmd python3; then
        python3 -c 'import json, sys; print(json.load(sys.stdin).get("tag_name", ""))'
        return
    fi

    if have_cmd python; then
        python -c 'import json, sys; print(json.load(sys.stdin).get("tag_name", ""))'
        return
    fi

    tr -d '\n' | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p'
}

resolve_tag() {
    if [ -n "${BITROUTER_VERSION:-}" ]; then
        case "$BITROUTER_VERSION" in
            v*)
                printf '%s\n' "$BITROUTER_VERSION"
                ;;
            *)
                printf 'v%s\n' "$BITROUTER_VERSION"
                ;;
        esac
        return
    fi

    latest_api_url=${BITROUTER_INSTALL_LATEST_API_URL:-"https://api.github.com/repos/${OWNER}/${REPO}/releases/latest"}
    release_json=$(download_text "$latest_api_url")
    tag=$(printf '%s' "$release_json" | extract_tag_name)

    [ -n "$tag" ] || fail "failed to resolve the latest release tag"
    printf '%s\n' "$tag"
}

detect_target() {
    if [ -n "${BITROUTER_TARGET:-}" ]; then
        printf '%s\n' "$BITROUTER_TARGET"
        return
    fi

    os=$(uname -s 2>/dev/null || true)
    arch=$(uname -m 2>/dev/null || true)

    case "$arch" in
        x86_64 | amd64)
            arch="x86_64"
            ;;
        aarch64 | arm64)
            arch="aarch64"
            ;;
        *)
            fail "unsupported architecture: $arch"
            ;;
    esac

    case "$os" in
        Linux)
            libc="gnu"
            if have_cmd ldd && ldd --version 2>&1 | grep -qi musl; then
                libc="musl"
            elif [ -f /etc/alpine-release ]; then
                libc="musl"
            else
                for loader in /lib/ld-musl-*.so.1 /lib64/ld-musl-*.so.1 /usr/lib/ld-musl-*.so.1; do
                    if [ -e "$loader" ]; then
                        libc="musl"
                        break
                    fi
                done
            fi
            printf '%s-unknown-linux-%s\n' "$arch" "$libc"
            ;;
        Darwin)
            printf '%s-apple-darwin\n' "$arch"
            ;;
        *)
            fail "unsupported operating system: $os"
            ;;
    esac
}

sha256_file() {
    file=$1

    if have_cmd sha256sum; then
        sha256sum "$file" | awk '{ print $1 }'
        return
    fi

    if have_cmd shasum; then
        shasum -a 256 "$file" | awk '{ print $1 }'
        return
    fi

    fail "either sha256sum or shasum is required"
}

extract_expected_checksum() {
    checksum_file=$1
    sed -n '1{s/^\([0-9a-fA-F][0-9a-fA-F]*\).*/\1/p;q;}' "$checksum_file"
}

need_cmd uname
need_cmd tar
need_cmd mktemp
need_cmd sed
need_cmd tr
need_cmd awk
need_cmd grep
need_cmd cp
need_cmd chmod
need_cmd ln
need_cmd rm
need_cmd mkdir

tag=$(resolve_tag)
target=$(detect_target)
bitrouter_home=${BITROUTER_HOME:-"${HOME}/.bitrouter"}
bin_dir="${bitrouter_home}/bin"
archive_name="bitrouter-${target}.tar.gz"
checksum_name="${archive_name}.sha256"
download_base_url=${BITROUTER_INSTALL_RELEASES_BASE_URL:-"https://github.com/${OWNER}/${REPO}/releases/download"}
archive_url="${download_base_url}/${tag}/${archive_name}"
checksum_url="${download_base_url}/${tag}/${checksum_name}"
installed_name="bitrouter-${tag}-${target}"
install_path="${bin_dir}/${installed_name}"

tmpdir=$(mktemp -d)
trap 'rm -rf "$tmpdir"' EXIT HUP INT TERM

say "==> Installing BitRouter ${tag} (${target})"
say "==> Downloading ${archive_name}"
download_file "$archive_url" "${tmpdir}/${archive_name}"
say "==> Downloading ${checksum_name}"
download_file "$checksum_url" "${tmpdir}/${checksum_name}"

expected_checksum=$(extract_expected_checksum "${tmpdir}/${checksum_name}")
[ -n "$expected_checksum" ] || fail "failed to parse checksum from ${checksum_name}"

actual_checksum=$(sha256_file "${tmpdir}/${archive_name}")
[ "$expected_checksum" = "$actual_checksum" ] || fail "checksum verification failed for ${archive_name}"

say "==> Extracting archive"
tar -xzf "${tmpdir}/${archive_name}" -C "$tmpdir"
[ -f "${tmpdir}/bitrouter" ] || fail "archive did not contain the bitrouter binary"

mkdir -p "$bin_dir"
cp "${tmpdir}/bitrouter" "$install_path"
chmod 755 "$install_path"

(
    cd "$bin_dir"
    rm -f bitrouter
    ln -s "$installed_name" bitrouter
)

say "==> Installed ${install_path}"
say "==> Updated ${bin_dir}/bitrouter"

case ":$PATH:" in
    *:"$bin_dir":*)
        ;;
    *)
        say ""
        say "Add ${bin_dir} to your PATH to run bitrouter from any shell."
        ;;
esac
