use std::str::FromStr;

use base64::{Engine, prelude::BASE64_STANDARD};
use serde::Deserialize;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::{Keypair, Signer};
use solana_message::Message;
use solana_pubkey::Pubkey;
use solana_transaction::Transaction;

use bitrouter_swig_sdk::auth::ClientRole;
use bitrouter_swig_sdk::auth::ed25519::Ed25519ClientRole;
use bitrouter_swig_sdk::pda;

use x402_core::transport::{PaymentPayload, PaymentRequirements, PaymentResource};
use x402_core::types::{Extension, Record, X402V2};
use x402_networks::svm::SvmAddress;
use x402_networks::svm::exact::ExplicitSvmPayload;
use x402_signer::PaymentSigner;
use x402_signer::svm::SvmRpc;

/// SWIG wallet x402 payment signer.
///
/// Wraps x402 transfer instructions in SWIG `sign_v2` delegation calls,
/// allowing a SWIG wallet PDA to pay for LLM requests via the x402 protocol.
pub struct SwigPaymentSigner<R> {
    /// The SWIG account PDA (the on-chain account storing roles/permissions).
    swig_account: Pubkey,
    /// The SWIG wallet address PDA (the address that holds tokens).
    swig_wallet_address: Pubkey,
    /// The Ed25519 authority keypair for signing transactions.
    authority: Keypair,
    /// The SWIG role ID that grants token transfer permissions.
    role_id: u32,
    /// Solana RPC client for fetching blockhash and mint info.
    rpc: R,
}

impl<R: SvmRpc> SwigPaymentSigner<R> {
    pub fn new(swig_account: Pubkey, authority: Keypair, role_id: u32, rpc: R) -> Self {
        let (swig_wallet_address, _bump) = pda::swig_wallet_address(&swig_account);
        Self {
            swig_account,
            swig_wallet_address,
            authority,
            role_id,
            rpc,
        }
    }
}

/// SWIG-specific signing errors.
#[derive(Debug, thiserror::Error)]
pub enum SwigSigningError {
    #[error("wallet error: {0}")]
    Wallet(String),

    #[error("RPC error: {0}")]
    Rpc(String),

    #[error("cannot parse address '{0}': {1}")]
    AddressParse(String, String),

    #[error("feePayer missing from requirements extra")]
    MissingFeePayer,

    #[error("payload serialization: {0}")]
    Serialization(String),

    #[error("swig SDK error: {0}")]
    Swig(String),
}

#[derive(Deserialize)]
struct SvmExtra {
    #[serde(rename = "feePayer")]
    fee_payer: Option<String>,
}

impl<R> PaymentSigner for SwigPaymentSigner<R>
where
    R: SvmRpc + Sync,
{
    type Error = SwigSigningError;

    fn matches(&self, requirements: &PaymentRequirements) -> bool {
        requirements.scheme == "exact" && requirements.network.starts_with("solana:")
    }

    async fn sign_payment(
        &self,
        requirements: &PaymentRequirements,
        resource: &PaymentResource,
        extensions: &Record<Extension>,
    ) -> Result<PaymentPayload, SwigSigningError> {
        // 1. Parse requirements
        let extra: SvmExtra = requirements
            .extra
            .as_ref()
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or(SvmExtra { fee_payer: None });

        let fee_payer_str = extra.fee_payer.ok_or(SwigSigningError::MissingFeePayer)?;
        let fee_payer = parse_pubkey(&fee_payer_str)?;

        let mint = parse_pubkey(&requirements.asset)?;
        let destination_owner = parse_pubkey(&requirements.pay_to)?;
        let amount = requirements.amount.0 as u64;

        // 2. Fetch mint info and blockhash from RPC
        let mint_info = self
            .rpc
            .fetch_mint_info(SvmAddress(mint))
            .await
            .map_err(|e| SwigSigningError::Rpc(e.to_string()))?;

        let recent_blockhash = self
            .rpc
            .get_latest_blockhash()
            .await
            .map_err(|e| SwigSigningError::Rpc(e.to_string()))?;

        // 3. Build inner transfer instruction.
        //    The swig_wallet_address is the token authority (payer).
        let transfer_ix = build_transfer_checked(
            &self.swig_wallet_address,
            &mint,
            &destination_owner,
            amount,
            mint_info.decimals,
            &mint_info.program_address.0,
        );

        // 4. Wrap transfer in SWIG sign_v2 delegation.
        let client_role = Ed25519ClientRole::new(self.authority.pubkey());
        let swig_instructions = client_role
            .sign_v2(
                self.swig_account,
                self.swig_wallet_address,
                self.role_id,
                vec![transfer_ix],
                None, // Ed25519 doesn't use current_slot
                &[fee_payer],
            )
            .map_err(|e| SwigSigningError::Swig(e.to_string()))?;

        // 5. Assemble full instruction list: compute budget + swig + memo.
        let mut instructions = Vec::with_capacity(swig_instructions.len() + 3);
        instructions.push(build_set_compute_unit_limit(DEFAULT_COMPUTE_UNIT_LIMIT));
        instructions.push(build_set_compute_unit_price(DEFAULT_COMPUTE_UNIT_PRICE));
        instructions.extend(swig_instructions);
        instructions.push(build_memo_instruction());

        // 6. Build and sign the transaction.
        let message =
            Message::new_with_blockhash(&instructions, Some(&fee_payer), &recent_blockhash);
        let mut tx = Transaction::new_unsigned(message);

        let message_bytes = tx.message_data();
        let signature = solana_keypair::Signer::try_sign_message(&self.authority, &message_bytes)
            .map_err(|e| SwigSigningError::Wallet(e.to_string()))?;

        // Place signature at the authority's index in account_keys.
        let authority_pubkey = self.authority.pubkey();
        let signer_index = tx
            .message
            .account_keys
            .iter()
            .position(|k| k == &authority_pubkey)
            .ok_or_else(|| {
                SwigSigningError::Serialization("authority not found in account keys".into())
            })?;
        tx.signatures[signer_index] = signature;

        // 7. Serialize to base64.
        let tx_bytes = bincode::serde::encode_to_vec(&tx, bincode::config::legacy())
            .map_err(|e| SwigSigningError::Serialization(e.to_string()))?;
        let transaction_b64 = BASE64_STANDARD.encode(&tx_bytes);

        let payload_json = serde_json::to_value(ExplicitSvmPayload {
            transaction: transaction_b64,
        })
        .map_err(|e| SwigSigningError::Serialization(e.to_string()))?;

        Ok(PaymentPayload {
            x402_version: X402V2,
            resource: resource.clone(),
            accepted: requirements.clone(),
            payload: payload_json,
            extensions: extensions.clone(),
        })
    }
}

// ---------------------------------------------------------------------------
// Instruction builders (matching x402-signer's internal implementation)
// ---------------------------------------------------------------------------

const DEFAULT_COMPUTE_UNIT_LIMIT: u32 = 400_000;
const DEFAULT_COMPUTE_UNIT_PRICE: u64 = 10_000;

const COMPUTE_BUDGET_PROGRAM: Pubkey =
    solana_pubkey::pubkey!("ComputeBudget111111111111111111111111111111");
const MEMO_PROGRAM: Pubkey = solana_pubkey::pubkey!("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr");
const ASSOCIATED_TOKEN_PROGRAM: Pubkey =
    solana_pubkey::pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");

fn build_set_compute_unit_limit(units: u32) -> Instruction {
    let mut data = Vec::with_capacity(5);
    data.push(2u8);
    data.extend_from_slice(&units.to_le_bytes());
    Instruction {
        program_id: COMPUTE_BUDGET_PROGRAM,
        accounts: vec![],
        data,
    }
}

fn build_set_compute_unit_price(micro_lamports: u64) -> Instruction {
    let mut data = Vec::with_capacity(9);
    data.push(3u8);
    data.extend_from_slice(&micro_lamports.to_le_bytes());
    Instruction {
        program_id: COMPUTE_BUDGET_PROGRAM,
        accounts: vec![],
        data,
    }
}

fn build_transfer_checked(
    payer: &Pubkey,
    mint: &Pubkey,
    destination_owner: &Pubkey,
    amount: u64,
    decimals: u8,
    token_program: &Pubkey,
) -> Instruction {
    let source_ata = derive_ata(payer, mint, token_program);
    let destination_ata = derive_ata(destination_owner, mint, token_program);

    let mut data = Vec::with_capacity(10);
    data.push(12u8); // SPL Token TransferChecked discriminator
    data.extend_from_slice(&amount.to_le_bytes());
    data.push(decimals);

    Instruction {
        program_id: *token_program,
        accounts: vec![
            AccountMeta::new(source_ata, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new(destination_ata, false),
            AccountMeta::new_readonly(*payer, true),
        ],
        data,
    }
}

fn build_memo_instruction() -> Instruction {
    let nonce: [u8; 16] = rand::random();
    let hex_str = hex::encode(nonce);
    Instruction {
        program_id: MEMO_PROGRAM,
        accounts: vec![],
        data: hex_str.into_bytes(),
    }
}

fn derive_ata(owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
    let (ata, _bump) = Pubkey::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &ASSOCIATED_TOKEN_PROGRAM,
    );
    ata
}

fn parse_pubkey(s: &str) -> Result<Pubkey, SwigSigningError> {
    Pubkey::from_str(s).map_err(|e| SwigSigningError::AddressParse(s.to_string(), e.to_string()))
}
