#!/usr/bin/env bash
set -e
source "$HOME/.cargo/env" 2>/dev/null || true

SRC="/mnt/c/Users/akam leinad/bitrouter"
DST="$HOME/bitrouter"

# Sync the files changed this session
cp "$SRC/plugins/bitrouter-pay/src/wallet/ows_signer.rs"   "$DST/plugins/bitrouter-pay/src/wallet/ows_signer.rs"
cp "$SRC/plugins/bitrouter-pay/src/payment/x402.rs"        "$DST/plugins/bitrouter-pay/src/payment/x402.rs"
cp "$SRC/plugins/bitrouter-pay/tests/integration_test.rs"  "$DST/plugins/bitrouter-pay/tests/integration_test.rs"

cd "$DST"
export OWS_VAULT_PATH=/home/maka/.ows/wallets
export OWS_WALLET_NAME=agent-treasury
export OWS_BIN=/home/maka/.ows/bin/ows
export CHAINLINK_ATTESTER_API_KEY=RLtYDAmBqQFXkxRpC6zhsQaVPA5qC4DC1gKNJVxn36qv

cargo test -p bitrouter-pay --test integration_test -- --nocapture --include-ignored 2>&1
