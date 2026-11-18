#!/usr/bin/env bash
# Build the native archives with the production packagers and install from local files.
set -euo pipefail
[[ "$(git rev-parse HEAD)" == "$SOURCE_SHA" && -z "$(git status --porcelain)" ]]
[[ "$ROLE" == m0 || "$ROLE" == service ]]
[[ "$(dist --version)" == *' 0.31.0' ]]
target=x86_64-unknown-linux-gnu
[[ "$(rustc -vV | sed -n 's/^host: //p')" == "$target" ]]
out="$RUNNER_TEMP/m5-evidence"
assets="$PWD/target/distrib"
installs="$(mktemp -d "$RUNNER_TEMP/m5-installs.XXXXXX")"
helpers="$(cd "$(dirname "$0")" && pwd)"
export BITROUTER_BUILD_REVISION="$SOURCE_SHA"
mkdir -p "$out/archives" "$out/receipts"
dist plan -o json > "$out/plan.json"
mkdir -p "$assets"
cp "$out/plan.json" "$assets/plan-dist-manifest.json"
if [[ "$ROLE" == service ]]; then
  node helpers/dist-helper/packaging/release-plan.mjs "$out/plan.json" > "$out/product-matrix.json"
fi
dist build --artifacts=local --target "$target" -o json > "$out/local.json"
cp "$out/local.json" "$assets/local-dist-manifest.json"
if [[ "$ROLE" == service ]]; then
  node helpers/dist-helper/packaging/verify-archives.mjs "$out/local.json"
fi
dist build --artifacts=global -o json > "$out/global.json"
if [[ "$ROLE" == service ]]; then
  node helpers/dist-helper/packaging/guard-installers.mjs "$out/global.json"
fi
install_full() {
  local root="$installs/$1" pkg="$2"
  mkdir -p "$root"
  HOME="$root" CARGO_HOME="$root/cargo" XDG_CONFIG_HOME="$root/config" \
    XDG_DATA_HOME="$root/data" INSTALLER_NO_MODIFY_PATH=1 INSTALLER_DOWNLOAD_URL="file://$assets" \
    sh "$assets/$pkg-installer.sh"
  cp "$root/config/$pkg/$pkg-receipt.json" "$out/receipts/$1-$pkg.json"
}
products=(bitrouter)
if [[ "$ROLE" == m0 ]]; then
  install_full m0-full bitrouter
else
  products+=(bitrouter-bro)
  install_full service-default bitrouter
  install_full bro bitrouter-bro
  install_full both bitrouter
  install_full both bitrouter-bro
fi
for pkg in "${products[@]}"; do
  name="$pkg-$target.tar.xz"
  (cd "$assets" && sha256sum -c "$name.sha256")
  cp "$assets/$name" "$assets/$name.sha256" "$assets/$pkg-installer.sh" "$out/archives/"
done
for profile in gateway gateway-shared; do
  dir="$RUNNER_TEMP/m5-package-$profile"
  cargo run --locked -p dist-helper -- package --profile "$profile" --target "$target" \
    --cargo-profile dist --out "$dir"
  name="bitrouter-$profile-$target.tar.gz"
  (cd "$dir" && sha256sum -c "$name.sha256")
  variant=minimum
  if [[ "$profile" == gateway-shared ]]; then variant=shared; fi
  root="$installs/$ROLE-$variant"
  mkdir "$root"
  tar -xzf "$dir/$name" -C "$root"
  cp "$dir/$name" "$dir/$name.sha256" "$out/archives/"
done
node "$helpers/footprint.mjs" "$installs" "$out" "$target"
