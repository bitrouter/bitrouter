#!/usr/bin/env bash
set -euo pipefail

# Ubuntu 24.04 ships this scoped namespace policy separately from Bubblewrap.
# https://packages.ubuntu.com/noble-updates/all/apparmor-profiles/filelist
probe() {
  /usr/bin/bwrap --unshare-all --die-with-parent --new-session --cap-drop ALL \
    --ro-bind / / /usr/bin/true
}
if [[ -x /usr/bin/bwrap ]] && probe; then
  exit 0
fi
sudo apt-get update
sudo apt-get install --yes --no-install-recommends bubblewrap apparmor-profiles
if ! probe; then
  sudo apparmor_parser -r /usr/share/apparmor/extra-profiles/bwrap-userns-restrict
  probe
fi
