#!/usr/bin/env bash
set -e
source "$HOME/.cargo/env" 2>/dev/null || true

SRC="/mnt/c/Users/akam leinad/bitrouter"
DST="$HOME/bitrouter"

# Replace stale WSL copies with the current Windows workspace
cp "$SRC/Cargo.toml" "$DST/Cargo.toml"
cp "$SRC/Cargo.lock" "$DST/Cargo.lock"
rm -rf "$DST/plugins/bitrouter-pay"
rm -rf "$DST/crates/bitrouter-sdk"
rm -rf "$DST/crates/bitrouter-attestation"
rm -rf "$DST/crates/bitrouter-chainlink"
cp -r "$SRC/plugins/bitrouter-pay" "$DST/plugins/"
cp -r "$SRC/crates/bitrouter-sdk" "$DST/crates/"
cp -r "$SRC/crates/bitrouter-attestation" "$DST/crates/"
cp -r "$SRC/crates/bitrouter-chainlink" "$DST/crates/"

cd "$DST"
export OWS_VAULT_PATH=/home/maka/.ows/wallets
export OWS_WALLET_NAME=agent-treasury
export OWS_BIN=/home/maka/.ows/bin/ows
export CHAINLINK_ATTESTER_API_KEY=RLtYDAmBqQFXkxRpC6zhsQaVPA5qC4DC1gKNJVxn36qv

cargo run -p bitrouter-pay --example claude_code_demo 2>&1
