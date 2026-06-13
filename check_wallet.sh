#!/usr/bin/env bash
set -e
export OWS_VAULT_PATH=/home/maka/.ows/wallets
OWS=/home/maka/.ows/bin/ows

TD='{"types":{"EIP712Domain":[{"name":"name","type":"string"},{"name":"version","type":"string"},{"name":"chainId","type":"uint256"},{"name":"verifyingContract","type":"address"}],"TransferWithAuthorization":[{"name":"from","type":"address"},{"name":"to","type":"address"},{"name":"value","type":"uint256"},{"name":"validAfter","type":"uint256"},{"name":"validBefore","type":"uint256"},{"name":"nonce","type":"bytes32"}]},"primaryType":"TransferWithAuthorization","domain":{"name":"USDC","version":"2","chainId":5042002,"verifyingContract":"0x3600000000000000000000000000000000000000"},"message":{"from":"0xBB4CB05dA6ED0780cFDd0F088EaEEd420381DE38","to":"0xec56f2790840676a82ac11cbebb463eb28c9799a","value":"1000","validAfter":"0","validBefore":"1781313023","nonce":"0x1d39c364f6e0eeff1513761a8a6afbdddb98acf02f08fe11cfe4ee86d99d022f"}}'

echo "=== raw hex output ==="
"$OWS" sign message --chain eip155:5042002 --wallet agent-treasury --message "" --typed-data "$TD"
echo ""
echo "=== json output ==="
"$OWS" sign message --chain eip155:5042002 --wallet agent-treasury --message "" --typed-data "$TD" --json
