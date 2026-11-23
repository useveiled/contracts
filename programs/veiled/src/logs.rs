use anchor_lang::prelude::*;

use crate::model::{AccountKind, AgentStatus, AutonomyTier, KycTier};

/// A wallet registered a profile and claimed its first handle.
#[event]
pub struct ProfileCreated {
    pub owner: Pubkey,
    pub handle: String,
    pub account_kind: AccountKind,
    pub timestamp: i64,
}

/// A profile moved to a new handle; the previous one is free to claim again.
#[event]
pub struct HandleChanged {
    pub owner: Pubkey,
    pub previous_handle: String,
    pub new_handle: String,
    pub timestamp: i64,
}

/// The compliance authority set a new KYC tier on a profile.
#[event]
pub struct KycTierUpdated {
    pub owner: Pubkey,
    pub kyc_tier: KycTier,
    pub timestamp: i64,
}

/// A profile created an agent and its vault. A `SpendPolicyUpdated` with the
/// starting limits is emitted in the same instruction.
#[event]
pub struct AgentCreated {
    pub agent: Pubkey,
    pub owner: Pubkey,
    pub agent_signer: Pubkey,
    pub label: String,
    pub autonomy_tier: AutonomyTier,
    pub mint: Pubkey,
    pub timestamp: i64,
}

/// An agent's limits, HITL threshold or allowlist were set, either at creation
/// or by a later owner update. Carries the full policy, not a diff.
#[event]
pub struct SpendPolicyUpdated {
    pub agent: Pubkey,
    pub per_tx_limit: u64,
    pub daily_limit: u64,
    pub hitl_threshold: u64,
    pub allowlist_enabled: bool,
    pub allowed_recipients: Vec<Pubkey>,
    pub timestamp: i64,
}

/// An agent was paused, resumed or permanently revoked.
#[event]
pub struct AgentStatusChanged {
    pub agent: Pubkey,
    pub status: AgentStatus,
    pub timestamp: i64,
}

/// The owner gave an agent a new label.
#[event]
pub struct AgentLabelUpdated {
    pub agent: Pubkey,
    pub previous_label: String,
    pub new_label: String,
    pub timestamp: i64,
}

/// The owner swapped the key an agent signs with.
#[event]
pub struct AgentSignerRotated {
    pub agent: Pubkey,
    pub previous_signer: Pubkey,
    pub new_signer: Pubkey,
    pub timestamp: i64,
}

/// Tokens were deposited into an agent's vault.
#[event]
pub struct AgentFunded {
    pub agent: Pubkey,
    pub funder: Pubkey,
    pub amount: u64,
    pub timestamp: i64,
}

/// The owner withdrew part of an agent's vault balance. Not emitted for the
/// sweep that happens during revocation.
#[event]
pub struct AgentDefunded {
    pub agent: Pubkey,
    pub owner: Pubkey,
    pub amount: u64,
    pub timestamp: i64,
}

/// Tokens left an agent's vault for a recipient, either paid directly or via
/// an approved queue entry. `window_spent` is the window total after this
/// payment.
#[event]
pub struct AgentPaymentExecuted {
    pub agent: Pubkey,
    pub recipient: Pubkey,
    pub mint: Pubkey,
    pub amount: u64,
    pub invoice_id: [u8; 32],
    pub window_spent: u64,
    pub timestamp: i64,
}

/// An agent queued a spend that now waits for the owner's decision.
#[event]
pub struct ApprovalRequired {
    pub agent: Pubkey,
    pub pending_index: u64,
    pub recipient: Pubkey,
    pub mint: Pubkey,
    pub amount: u64,
    pub invoice_id: [u8; 32],
    pub timestamp: i64,
}

/// The owner approved a queued spend and it settled.
#[event]
pub struct PendingPaymentApproved {
    pub agent: Pubkey,
    pub pending_index: u64,
    pub recipient: Pubkey,
    pub amount: u64,
    pub timestamp: i64,
}

/// The owner turned down a queued spend; nothing was paid.
#[event]
pub struct PendingPaymentRejected {
    pub agent: Pubkey,
    pub pending_index: u64,
    pub timestamp: i64,
}

/// A protocol fee reached the treasury. For agent spends `payer` is the
/// agent's owner, even though the tokens came from the agent vault.
#[event]
pub struct ProtocolFeeCharged {
    pub payer: Pubkey,
    pub treasury: Pubkey,
    pub fee: u64,
    pub timestamp: i64,
}

/// A payment request was opened. `amount` is zero when the request is
/// confidential.
#[event]
pub struct PaymentRequestCreated {
    pub request: Pubkey,
    pub requester: Pubkey,
    pub mint: Pubkey,
    pub is_confidential: bool,
    pub amount: u64,
    pub expires_at: i64,
    pub timestamp: i64,
}

/// A payment request was paid in full and is now marked fulfilled.
#[event]
pub struct PaymentRequestFulfilled {
    pub request: Pubkey,
    pub payer: Pubkey,
    pub amount: u64,
    pub timestamp: i64,
}

/// The requester withdrew an open payment request.
#[event]
pub struct PaymentRequestCancelled {
    pub request: Pubkey,
    pub timestamp: i64,
}

/// A payment request account was deleted and its rent returned to the
/// requester.
#[event]
pub struct PaymentRequestClosed {
    pub request: Pubkey,
    pub timestamp: i64,
}

/// A profile recorded that it shared a selective disclosure for a transfer.
#[event]
pub struct DisclosureFiled {
    pub receipt: Pubkey,
    pub profile: Pubkey,
    pub tx_reference: String,
    pub timestamp: i64,
}

/// A wallet staked $VEILED. `new_balance` is its position after the deposit.
#[event]
pub struct Staked {
    pub account: Pubkey,
    pub amount: u64,
    pub new_balance: u64,
    pub timestamp: i64,
}

/// A wallet took $VEILED out of staking. `new_balance` is what stays staked.
#[event]
pub struct Unstaked {
    pub account: Pubkey,
    pub amount: u64,
    pub new_balance: u64,
    pub timestamp: i64,
}

/// The protocol admin and compliance keys were set, at init or by a direct
/// rotation.
#[event]
pub struct ConfigAuthorityUpdated {
    pub authority: Pubkey,
    pub compliance_authority: Pubkey,
}

/// An admin nominated a successor for either the protocol config or the fee
/// config. A `None` nominee means a pending handoff was cancelled.
#[event]
pub struct AuthorityTransferStarted {
    pub current_authority: Pubkey,
    pub pending_authority: Option<Pubkey>,
}

/// A nominated key accepted control and is now the authority.
#[event]
pub struct AuthorityTransferAccepted {
    pub previous_authority: Pubkey,
    pub new_authority: Pubkey,
}

/// The global pause on fund-moving instructions was turned on or off.
#[event]
pub struct ProtocolPauseToggled {
    pub paused: bool,
}

/// The fee treasury or the fee on/off switch changed.
#[event]
pub struct FeeRoutingUpdated {
    pub treasury: Pubkey,
    pub fees_enabled: bool,
}

/// Base fee, fee cap or held weight was set, at init or by a later update.
#[event]
pub struct FeeScheduleUpdated {
    pub base_fee_bps: u16,
    pub fee_cap: u64,
    pub held_weight_bps: u16,
}

/// The discount tier table was replaced. `count` is the number of tiers now
/// in effect.
#[event]
pub struct TiersUpdated {
    pub count: u64,
}
