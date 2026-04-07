# BitRouter Open JWT Auth Protocol

This document specifies the JWT authentication format defined in `bitrouter-core`.

## 1. Token format

A BitRouter token is a JWT with three base64url-encoded segments and no padding:

```text
BASE64URL(header) + "." + BASE64URL(payload) + "." + BASE64URL(signature)
```

The signed message is the ASCII byte sequence:

```text
BASE64URL(header) + "." + BASE64URL(payload)
```

## 2. Header JSON

Every token header is plain JSON with the following shape:

```json
{
  "alg": "SOL_EDDSA",
  "typ": "JWT"
}
```

or:

```json
{
  "alg": "EIP191K",
  "typ": "JWT"
}
```

### `alg` values

- `SOL_EDDSA`: Solana-style Ed25519 signing over the raw signed message bytes.
- `EIP191K`: EVM-style EIP-191 prefixed secp256k1 signing.

`typ` is always `JWT`.

## 3. Payload JSON

The payload is a JSON object with this shape:

```json
{
  "iss": "solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp:DRpbCBMxVnDK7maPM5tGv6MvB3v1sRMC86PZ8okm21hy",
  "chain": "solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp",
  "iat": 1700000000,
  "exp": 1700000300,
  "scope": "admin",
  "models": ["openai/*", "anthropic/claude-*"],
  "budget": 2500000,
  "budget_scope": "session",
  "budget_range": {
    "type": "rounds",
    "count": 10
  }
}
```

### Field definitions

#### Required fields

- `iss`: CAIP-10 account identifier of the signer.
  - Solana example:
    ```json
    "solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp:DRpbCBMxVnDK7maPM5tGv6MvB3v1sRMC86PZ8okm21hy"
    ```
  - EVM example:
    ```json
    "eip155:8453:0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045"
    ```
- `chain`: CAIP-2 chain identifier used for signing.
  - Solana example:
    ```json
    "solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp"
    ```
  - EVM example:
    ```json
    "eip155:8453"
    ```
- `scope`: Authorization scope.
  - Allowed values:
    ```json
    "admin"
    ```
    ```json
    "api"
    ```

#### Optional fields

- `iat`: Issued-at UNIX timestamp in seconds.
- `exp`: Expiration UNIX timestamp in seconds.
  - Admin tokens are expected to include `exp`.
  - API tokens may omit `exp`.
  - When `exp` is omitted, `bitrouter-core` does not apply a default expiration timestamp.
  - A token without `exp` remains valid until some external policy rejects it, such as key rotation, budget exhaustion, or application-specific rules.
  - For interoperable and safer deployments, issuers should prefer setting `exp` on API tokens instead of relying on indefinite validity.
  - Short-lived API tokens are recommended.
- `models`: Array of model-name patterns allowed for this token.
- `budget`: Unsigned integer budget in micro-USD.
  - `1000000` means 1.000000 USD.
- `budget_scope`: Budget application scope.
  - Allowed values:
    ```json
    "session"
    ```
    ```json
    "account"
    ```
- `budget_range`: Budget measurement window.

When an optional field is absent, it is omitted from the JSON payload instead of being serialized as `null`.

## 4. `budget_range` JSON variants

Round-based budget window:

```json
{
  "type": "rounds",
  "count": 10
}
```

Time-based budget window:

```json
{
  "type": "duration",
  "seconds": 3600
}
```

## 5. Signing algorithm selection

The algorithm is selected from the CAIP-10 namespace embedded in `iss`.

### Solana namespace

If `iss` starts with:

```json
"solana:<reference>:<address>"
```

then:

- `chain` must equal:
  ```json
  "solana:<reference>"
  ```
- `alg` must be:
  ```json
  "SOL_EDDSA"
  ```
- The signer signs the raw `BASE64URL(header) + "." + BASE64URL(payload)` bytes with Ed25519.
- The signature is the 64-byte Ed25519 signature encoded with base64url and no padding.
- Verification uses the Solana base58 public key from the CAIP-10 address portion of `iss`.

### EVM namespace

If `iss` starts with:

```json
"eip155:<reference>:<address>"
```

then:

- `chain` must equal:
  ```json
  "eip155:<reference>"
  ```
- `alg` must be:
  ```json
  "EIP191K"
  ```
- The signer applies the standard EIP-191 Ethereum signed-message prefix to the raw `BASE64URL(header) + "." + BASE64URL(payload)` bytes and signs with secp256k1 ECDSA.
- The signature is the 65-byte `r || s || v` encoding, base64url-encoded with no padding.
- Verification recovers the EVM address from the signature and compares it with the address in `iss`.

## 6. Validation rules

A verifier should reject the token if any of the following checks fail:

1. The token does not contain exactly three dot-separated segments.
2. The header, payload, or signature is not valid base64url without padding.
3. The header JSON does not contain a supported `alg` value.
4. `iss` is not a valid CAIP-10 identifier.
5. `chain` does not equal the CAIP-2 chain implied by `iss`.
6. `alg` does not match the chain namespace implied by `iss`.
7. The signature does not verify against the identity embedded in `iss`.
8. If `exp` is present, the token is valid only while `current_unix_time < exp`; once the clock reaches `exp`, the token must be rejected. This is the current `bitrouter-core` expiration rule.

## 7. Minimal examples

### Solana API token payload

```json
{
  "iss": "solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp:DRpbCBMxVnDK7maPM5tGv6MvB3v1sRMC86PZ8okm21hy",
  "chain": "solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp",
  "iat": 1700000000,
  "scope": "api"
}
```

### EVM API token payload

```json
{
  "iss": "eip155:8453:0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045",
  "chain": "eip155:8453",
  "iat": 1700000000,
  "scope": "api"
}
```
