#!/usr/bin/env bash
set -e
source "$HOME/.cargo/env" 2>/dev/null || true

SRC="/mnt/c/Users/akam leinad/bitrouter"
DST="$HOME/bitrouter"

mkdir -p "$DST"

rm -rf "$DST/crates"
cp -r "$SRC/crates" "$DST/crates"

rm -rf "$DST/plugins/bitrouter-pay"
mkdir -p "$DST/plugins"
cp -r "$SRC/plugins/bitrouter-pay" "$DST/plugins/bitrouter-pay"

cp "$SRC/Cargo.toml" "$DST/Cargo.toml"
cp "$SRC/Cargo.lock" "$DST/Cargo.lock"

cd "$DST"
export OWS_VAULT_PATH=/home/maka/.ows/wallets
export OWS_WALLET_NAME=agent-treasury
export OWS_BIN=/home/maka/.ows/bin/ows
export CHAINLINK_ATTESTER_API_KEY=RLtYDAmBqQFXkxRpC6zhsQaVPA5qC4DC1gKNJVxn36qv

cargo run -p bitrouter-pay --example claude_code_demo 2>&1
