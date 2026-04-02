//! Minimal Solana transaction builder.
//!
//! Provides just enough functionality to build SOL transfer and SPL
//! `transferChecked` transactions without pulling in `solana-sdk`.

use sha2::{Digest, Sha256};

use mpp::error::MppError;

// ── Well-known program IDs ───────────────────────────────────────────

/// System Program (all zeros).
const SYSTEM_PROGRAM: Pubkey = Pubkey([0; 32]);

/// SPL Token Program (`TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA`).
const TOKEN_PROGRAM: Pubkey = Pubkey([
    0x06, 0xdd, 0xf6, 0xe1, 0xd7, 0x65, 0xa1, 0x93, 0xd9, 0xcb, 0xe1, 0x46, 0xce, 0xeb, 0x79, 0xac,
    0x1c, 0xb4, 0x85, 0xed, 0x5f, 0x5b, 0x37, 0x91, 0x3a, 0x8c, 0xf5, 0x85, 0x7e, 0xff, 0x00, 0xa9,
]);

/// Associated Token Account Program (`ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL`).
const ASSOCIATED_TOKEN_PROGRAM: Pubkey = Pubkey([
    0x8c, 0x97, 0x25, 0x8f, 0x4e, 0x24, 0x89, 0xf1, 0xbb, 0x3d, 0x10, 0x29, 0x14, 0x8e, 0x0d, 0x83,
    0x0b, 0x5a, 0x13, 0x99, 0xda, 0xff, 0x10, 0x84, 0x04, 0x8e, 0x7b, 0xd8, 0xdb, 0xe9, 0xf8, 0x59,
]);

// ── Public key ───────────────────────────────────────────────────────

/// A 32-byte Solana public key.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Pubkey(pub [u8; 32]);

impl std::fmt::Debug for Pubkey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", bs58::encode(&self.0).into_string())
    }
}

/// Parse a base58-encoded Solana public key.
pub fn pubkey_from_b58(s: &str) -> Result<Pubkey, String> {
    let bytes = bs58::decode(s)
        .into_vec()
        .map_err(|e| format!("invalid base58: {e}"))?;
    if bytes.len() != 32 {
        return Err(format!("expected 32 bytes, got {}", bytes.len()));
    }
    let mut key = Pubkey([0u8; 32]);
    key.0.copy_from_slice(&bytes);
    Ok(key)
}

// ── ATA derivation ───────────────────────────────────────────────────

/// Derive the associated token account address for a wallet + mint.
fn get_associated_token_address(wallet: &Pubkey, mint: &Pubkey) -> Result<Pubkey, MppError> {
    find_program_address(
        &[&wallet.0, &TOKEN_PROGRAM.0, &mint.0],
        &ASSOCIATED_TOKEN_PROGRAM,
    )
    .map(|(addr, _bump)| addr)
}

/// Find a program-derived address (PDA).
fn find_program_address(seeds: &[&[u8]], program_id: &Pubkey) -> Result<(Pubkey, u8), MppError> {
    for bump in (0u8..=255).rev() {
        if let Some(addr) = try_create_program_address(seeds, bump, program_id) {
            return Ok((addr, bump));
        }
    }
    Err(MppError::InvalidConfig(
        "failed to find PDA (no valid bump)".into(),
    ))
}

/// Try to create a program address with the given bump seed.
///
/// Returns `None` if the derived address is on the Ed25519 curve.
fn try_create_program_address(seeds: &[&[u8]], bump: u8, program_id: &Pubkey) -> Option<Pubkey> {
    let mut hasher = Sha256::new();
    for seed in seeds {
        hasher.update(seed);
    }
    hasher.update([bump]);
    hasher.update(program_id.0);
    hasher.update(b"ProgramDerivedAddress");
    let hash: [u8; 32] = hasher.finalize().into();

    // A valid PDA must NOT be on the Ed25519 curve.
    if is_on_ed25519_curve(&hash) {
        return None;
    }

    Some(Pubkey(hash))
}

/// Check if a 32-byte value represents a point on the Ed25519 curve.
fn is_on_ed25519_curve(bytes: &[u8; 32]) -> bool {
    use curve25519_dalek::edwards::CompressedEdwardsY;
    CompressedEdwardsY::from_slice(bytes)
        .ok()
        .and_then(|p| p.decompress())
        .is_some()
}

// ── Transaction builder ──────────────────────────────────────────────

/// Solana transaction builder.
pub struct SolanaTransaction;

impl SolanaTransaction {
    /// Build a native SOL transfer transaction.
    ///
    /// Returns `(message_bytes, tx_bytes_with_placeholder_sig)`.
    pub fn sol_transfer(
        payer: &Pubkey,
        recipient: &Pubkey,
        lamports: u64,
        blockhash: &Pubkey,
    ) -> Result<(Vec<u8>, Vec<u8>), MppError> {
        // System Transfer instruction data: u32 LE(2) + u64 LE amount
        let mut data = Vec::with_capacity(12);
        data.extend_from_slice(&2u32.to_le_bytes());
        data.extend_from_slice(&lamports.to_le_bytes());

        let accounts = vec![*payer, *recipient, SYSTEM_PROGRAM];
        let instructions = vec![Instruction {
            program_id_index: 2,
            account_indexes: vec![0, 1],
            data,
        }];

        // 1 signer (payer), 0 readonly signed, 1 readonly unsigned (system program)
        let header = MessageHeader {
            num_required_signatures: 1,
            num_readonly_signed: 0,
            num_readonly_unsigned: 1,
        };

        build_transaction(&header, &accounts, blockhash, &instructions)
    }

    /// Build an SPL token `transferChecked` transaction.
    ///
    /// Includes an idempotent ATA creation for the destination.
    /// Returns `(message_bytes, tx_bytes_with_placeholder_sig)`.
    pub fn spl_transfer_checked(
        payer: &Pubkey,
        recipient: &Pubkey,
        mint: &Pubkey,
        amount: u64,
        decimals: u8,
        blockhash: &Pubkey,
    ) -> Result<(Vec<u8>, Vec<u8>), MppError> {
        let source_ata = get_associated_token_address(payer, mint)?;
        let dest_ata = get_associated_token_address(recipient, mint)?;

        // Account keys ordered: signers+writable, writable, readonly.
        let accounts = vec![
            *payer,                   // 0: signer + writable
            source_ata,               // 1: writable
            dest_ata,                 // 2: writable
            *recipient,               // 3: readonly
            *mint,                    // 4: readonly
            SYSTEM_PROGRAM,           // 5: readonly
            TOKEN_PROGRAM,            // 6: readonly
            ASSOCIATED_TOKEN_PROGRAM, // 7: readonly
        ];

        // CreateAssociatedTokenAccountIdempotent
        let create_ata_ix = Instruction {
            program_id_index: 7,
            account_indexes: vec![0, 2, 3, 4, 5, 6],
            data: vec![1],
        };

        // TransferChecked
        let mut transfer_data = Vec::with_capacity(10);
        transfer_data.push(12); // discriminator
        transfer_data.extend_from_slice(&amount.to_le_bytes());
        transfer_data.push(decimals);

        let transfer_ix = Instruction {
            program_id_index: 6,
            account_indexes: vec![1, 4, 2, 0],
            data: transfer_data,
        };

        let header = MessageHeader {
            num_required_signatures: 1,
            num_readonly_signed: 0,
            num_readonly_unsigned: 5,
        };

        build_transaction(&header, &accounts, blockhash, &[create_ata_ix, transfer_ix])
    }

    /// Replace the placeholder signature in a transaction with a real one.
    pub fn attach_signature(tx_bytes: &[u8], signature: &[u8; 64]) -> Vec<u8> {
        let mut result = tx_bytes.to_vec();
        // compact_u16(1) = [1], so the signature starts at byte 1.
        result[1..65].copy_from_slice(signature);
        result
    }
}

// ── Internal types ───────────────────────────────────────────────────

struct MessageHeader {
    num_required_signatures: u8,
    num_readonly_signed: u8,
    num_readonly_unsigned: u8,
}

struct Instruction {
    program_id_index: u8,
    account_indexes: Vec<u8>,
    data: Vec<u8>,
}

/// Build a serialized transaction with a placeholder (zero) signature.
///
/// Returns `(message_bytes, full_tx_bytes)`.
fn build_transaction(
    header: &MessageHeader,
    accounts: &[Pubkey],
    blockhash: &Pubkey,
    instructions: &[Instruction],
) -> Result<(Vec<u8>, Vec<u8>), MppError> {
    let message = serialize_message(header, accounts, blockhash, instructions);

    // Transaction: compact_u16(num_sigs) + sig(s) + message
    let mut tx = Vec::with_capacity(1 + 64 + message.len());
    encode_compact_u16(&mut tx, 1);
    tx.extend_from_slice(&[0u8; 64]); // placeholder
    tx.extend_from_slice(&message);

    Ok((message, tx))
}

/// Serialize a Solana legacy transaction message.
fn serialize_message(
    header: &MessageHeader,
    accounts: &[Pubkey],
    blockhash: &Pubkey,
    instructions: &[Instruction],
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(256);

    buf.push(header.num_required_signatures);
    buf.push(header.num_readonly_signed);
    buf.push(header.num_readonly_unsigned);

    encode_compact_u16(&mut buf, accounts.len() as u16);
    for account in accounts {
        buf.extend_from_slice(&account.0);
    }

    buf.extend_from_slice(&blockhash.0);

    encode_compact_u16(&mut buf, instructions.len() as u16);
    for ix in instructions {
        buf.push(ix.program_id_index);
        encode_compact_u16(&mut buf, ix.account_indexes.len() as u16);
        buf.extend_from_slice(&ix.account_indexes);
        encode_compact_u16(&mut buf, ix.data.len() as u16);
        buf.extend_from_slice(&ix.data);
    }

    buf
}

/// Encode a u16 in Solana's compact-u16 format.
fn encode_compact_u16(buf: &mut Vec<u8>, value: u16) {
    let mut val = value;
    loop {
        let mut byte = (val & 0x7F) as u8;
        val >>= 7;
        if val > 0 {
            byte |= 0x80;
        }
        buf.push(byte);
        if val == 0 {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_u16_encoding() {
        let mut buf = Vec::new();
        encode_compact_u16(&mut buf, 0);
        assert_eq!(buf, [0]);

        buf.clear();
        encode_compact_u16(&mut buf, 1);
        assert_eq!(buf, [1]);

        buf.clear();
        encode_compact_u16(&mut buf, 127);
        assert_eq!(buf, [127]);

        buf.clear();
        encode_compact_u16(&mut buf, 128);
        assert_eq!(buf, [0x80, 0x01]);

        buf.clear();
        encode_compact_u16(&mut buf, 256);
        assert_eq!(buf, [0x80, 0x02]);
    }

    #[test]
    fn pubkey_from_b58_roundtrip() {
        let b58 = "11111111111111111111111111111111";
        let key = pubkey_from_b58(b58).expect("valid");
        assert_eq!(key.0, [0u8; 32]);
    }

    #[test]
    fn sol_transfer_message_structure() {
        let payer = Pubkey([1u8; 32]);
        let recipient = Pubkey([2u8; 32]);
        let blockhash = Pubkey([3u8; 32]);

        let (message, tx) =
            SolanaTransaction::sol_transfer(&payer, &recipient, 1_000_000, &blockhash)
                .expect("build");

        assert_eq!(message[0], 1); // num_required_signatures
        assert_eq!(message[1], 0); // num_readonly_signed
        assert_eq!(message[2], 1); // num_readonly_unsigned

        assert_eq!(tx[0], 1); // compact_u16(1)
        assert_eq!(&tx[1..65], &[0u8; 64]); // placeholder sig
        assert_eq!(&tx[65..], &message[..]);
    }

    #[test]
    fn attach_signature_replaces_placeholder() {
        let payer = Pubkey([1u8; 32]);
        let recipient = Pubkey([2u8; 32]);
        let blockhash = Pubkey([3u8; 32]);

        let (_msg, tx) =
            SolanaTransaction::sol_transfer(&payer, &recipient, 1000, &blockhash).expect("build");

        let sig = [42u8; 64];
        let signed = SolanaTransaction::attach_signature(&tx, &sig);
        assert_eq!(&signed[1..65], &sig);
        assert_eq!(&signed[65..], &tx[65..]);
    }

    #[test]
    fn known_token_program_id() {
        // Verify our hardcoded Token Program matches the known base58.
        let expected = pubkey_from_b58("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA")
            .expect("parse token program");
        assert_eq!(TOKEN_PROGRAM, expected);
    }

    #[test]
    fn known_associated_token_program_id() {
        let expected = pubkey_from_b58("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL")
            .expect("parse ATA program");
        assert_eq!(ASSOCIATED_TOKEN_PROGRAM, expected);
    }
}
