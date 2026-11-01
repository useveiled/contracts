use anchor_lang::prelude::*;

/// Longest handle accepted, in bytes, not counting the `.veiled` suffix.
pub const MAX_HANDLE_LEN: usize = 32;
/// Longest agent label accepted, in bytes.
pub const MAX_LABEL_LEN: usize = 40;
/// Room for a base58 transaction signature (up to 88 characters) in a
/// disclosure receipt.
pub const MAX_TX_REFERENCE_LEN: usize = 88;
/// Cap on an agent's recipient allowlist. Kept small because every entry is
/// paid for in account space up front.
pub const MAX_ALLOWED_RECIPIENTS: usize = 10;
/// Cap on the number of fee discount tiers.
pub const MAX_FEE_TIERS: usize = 8;
/// Denominator for every basis point value: 10_000 bps is 100%.
pub const BPS: u64 = 10_000;
/// Length of an agent's daily spend window, in seconds (24 hours).
pub const WINDOW_SECONDS: i64 = 86_400;

// Seed prefixes for each PDA type. These bytes are part of every derived
// address, so changing one would orphan all existing accounts of that type.
pub const CONFIG_SEED: &[u8] = b"config";
pub const FEES_SEED: &[u8] = b"fees";
pub const PROFILE_SEED: &[u8] = b"profile";
pub const HANDLE_SEED: &[u8] = b"handle";
pub const AGENT_SEED: &[u8] = b"agent";
pub const VAULT_SEED: &[u8] = b"vault";
pub const PENDING_SEED: &[u8] = b"pending";
pub const REQUEST_SEED: &[u8] = b"request";
pub const DISCLOSURE_SEED: &[u8] = b"disclosure";
pub const STAKE_SEED: &[u8] = b"stake";
pub const STAKE_VAULT_SEED: &[u8] = b"stake_vault";

/// KYC level the compliance authority has attested for a profile. Every
/// profile starts at `Unverified`. The program only records the tier; it
/// does not gate any instruction on it, and identity stays public either way
/// since Veiled keeps amounts private, not who you are.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, InitSpace, Debug)]
pub enum KycTier {
    Unverified,
    Basic,
    Enhanced,
}

/// What kind of user a profile represents. Picked by the owner at
/// registration and used by the app for presentation; the program does not
/// branch on it.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, InitSpace, Debug)]
pub enum AccountKind {
    Personal,
    Business,
    AgentOperator,
}

/// Decides which spends an agent may settle on its own through `pay_invoice`
/// and which have to go through `queue_invoice` for the owner to approve.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, InitSpace, Debug)]
pub enum AutonomyTier {
    /// Cannot settle anything directly. Every spend goes into the queue.
    Supervised,
    /// Pays directly up to and including the HITL threshold. Anything above
    /// it has to be queued, and only amounts above it may be queued.
    SemiAutonomous,
    /// Pays directly whenever the spend policy allows, and can still choose
    /// to queue a spend for a human to look at.
    FullyAutonomous,
}

/// Lifecycle of an agent. `Active` and `Paused` can be toggled freely by the
/// owner; `Revoked` is reachable only through `revoke_agent` and is final.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, InitSpace, Debug)]
pub enum AgentStatus {
    Active,
    Paused,
    Revoked,
}

/// Lifecycle of a payment request. Only an `Open` request can be paid or
/// cancelled.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, InitSpace, Debug)]
pub enum RequestStatus {
    Open,
    Fulfilled,
    Cancelled,
}

/// Singleton holding the admin keys, fee routing and the global pause flag.
/// It holds no tokens. User balances live in user wallets or in per-agent
/// vaults, so this account controls policy rather than money.
#[account]
#[derive(InitSpace)]
pub struct ProtocolConfig {
    /// Admin key. Can rotate keys, toggle the pause and set fee routing. The
    /// plan is to hand it to a Squads multisig after beta.
    pub authority: Pubkey,
    /// Key nominated to take over as admin. Nothing changes until that key
    /// signs `accept_authority`, which protects against handing control to a
    /// typo.
    pub pending_authority: Option<Pubkey>,
    /// The only key that can call `set_kyc_tier`.
    pub compliance_authority: Pubkey,
    /// Global kill switch for instructions that move funds (agent payments,
    /// approvals, request payments). Owner side actions like pausing,
    /// revoking or withdrawing from an agent still work, so users are never
    /// locked out of their own controls during an incident.
    pub paused: bool,
    /// Owner of the token accounts that collect protocol fees.
    pub treasury: Pubkey,
    /// Master switch for fee collection. Starts false and can only be turned
    /// on together with a real treasury.
    pub fees_enabled: bool,
    pub bump: u8,
}

/// One step on the discount curve.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, InitSpace, Debug)]
pub struct FeeTier {
    /// Loyalty weight needed to reach this step, in base units of the
    /// $VEILED mint.
    pub min_weight: u64,
    /// Discount applied to the base fee at this step, in basis points.
    pub discount_bps: u16,
}

/// Fee parameters and the $VEILED staking totals. `quote_fee` reads this
/// account at settlement time, and clients read the same fields to preview
/// a user's rate, so the two always agree.
#[account]
#[derive(InitSpace)]
pub struct FeeConfig {
    /// Key that can change the fee schedule and tiers.
    pub authority: Pubkey,
    /// Nominee for a two-step handoff of `authority`, same as on the
    /// protocol config.
    pub pending_authority: Option<Pubkey>,
    /// Mint of the $VEILED token used for loyalty weight and staking.
    pub veiled_mint: Pubkey,
    /// Fee charged before discounts, in basis points of the amount.
    pub base_fee_bps: u16,
    /// Ceiling on a single fee, in base units of the token being paid.
    /// Zero means no ceiling.
    pub fee_cap: u64,
    /// Fraction of a wallet's unstaked $VEILED balance that counts toward
    /// its weight, in basis points. Staked tokens always count in full.
    pub held_weight_bps: u16,
    /// Discount steps, sorted by strictly increasing `min_weight`.
    #[max_len(MAX_FEE_TIERS)]
    pub tiers: Vec<FeeTier>,
    /// Running sum of every open stake position, which should match the
    /// stake vault balance.
    pub total_staked: u64,
    pub bump: u8,
}

/// A wallet's Veiled identity. Each wallet gets at most one, and it owns
/// exactly one `.veiled` handle at a time so others can pay `john.veiled`
/// instead of pasting an address. The counters here also feed the PDA seeds
/// of the agents, requests and receipts the profile creates.
#[account]
#[derive(InitSpace)]
pub struct Profile {
    pub owner: Pubkey,
    /// The bare name, stored lowercase, with no `@` and no `.veiled`.
    #[max_len(MAX_HANDLE_LEN)]
    pub handle: String,
    pub kyc_tier: KycTier,
    pub account_kind: AccountKind,
    /// Agents created so far. Also the index the next agent will receive.
    pub agent_count: u64,
    /// Payment requests created so far. Also the next request's index.
    pub request_count: u64,
    /// Disclosure receipts filed so far. Also the next receipt's index.
    pub disclosure_count: u64,
    pub created_at: i64,
    pub updated_at: i64,
    pub bump: u8,
}

/// Reservation for one handle. The address is derived from the handle text,
/// so checking availability or looking up who owns a handle takes a single
/// read with no index to maintain. Closed when its owner moves to a new
/// handle, which frees the name.
#[account]
#[derive(InitSpace)]
pub struct HandleClaim {
    pub owner: Pubkey,
    pub bump: u8,
}

/// An AI agent acting for a profile, plus the rules it spends under. The
/// agent signs with its own key, but it can only spend from its own vault
/// (a token account owned by this PDA) and only inside these limits. If the
/// agent key leaks, the owner pauses, rotates or revokes it, and the
/// attacker is still bound by the same policy in the meantime.
#[account]
#[derive(InitSpace)]
pub struct Agent {
    /// Wallet of the profile that created the agent and controls it.
    pub owner: Pubkey,
    /// Key the agent signs `pay_invoice` and `queue_invoice` with.
    pub agent_signer: Pubkey,
    /// Human readable name, for example "coding-assistant".
    #[max_len(MAX_LABEL_LEN)]
    pub label: String,
    pub autonomy_tier: AutonomyTier,
    pub status: AgentStatus,
    /// Token mint the vault holds, fixed when the agent is created. Every
    /// transfer in or out of the vault must use it.
    pub mint: Pubkey,
    /// Largest single spend allowed, in base units of `mint`.
    pub per_tx_limit: u64,
    /// Total that can settle within one window of `WINDOW_SECONDS`.
    pub daily_limit: u64,
    /// For `SemiAutonomous` agents, the largest amount payable without
    /// approval.
    pub hitl_threshold: u64,
    /// Timestamp the current spend window opened. A new window opens on the
    /// first spend at least 24 hours after this.
    pub window_start: i64,
    /// Amount settled since `window_start`.
    pub window_spent: u64,
    /// When set, payments may only go to wallets in `allowed_recipients`.
    pub allowlist_enabled: bool,
    #[max_len(MAX_ALLOWED_RECIPIENTS)]
    pub allowed_recipients: Vec<Pubkey>,
    /// Spends queued so far. Also the seed index for the next one.
    pub pending_count: u64,
    /// Position of this agent among its owner's agents. Used in the PDA seed.
    pub index: u64,
    pub created_at: i64,
    pub updated_at: i64,
    pub bump: u8,
    pub vault_bump: u8,
}

/// A spend the agent has asked to make, parked until the owner approves or
/// rejects it. Creating one moves no tokens. Approving re-checks the policy
/// as it stands then; either outcome closes the account.
#[account]
#[derive(InitSpace)]
pub struct PendingApproval {
    pub agent: Pubkey,
    pub owner: Pubkey,
    pub recipient: Pubkey,
    pub mint: Pubkey,
    pub amount: u64,
    /// Identifier of the x402 invoice being paid. All zeros when the spend is
    /// not tied to an invoice.
    pub invoice_id: [u8; 32],
    /// Hash of an encrypted memo kept off chain, so the approver can see why
    /// the agent wants to pay.
    pub memo_hash: [u8; 32],
    pub index: u64,
    pub created_at: i64,
    pub bump: u8,
}

/// The on-chain side of a Veiled pay link or QR code. A plain request stores
/// the amount openly. A confidential one stores only a hash commitment to
/// it, and the payer has to reveal a matching amount and blinding factor
/// when paying.
#[account]
#[derive(InitSpace)]
pub struct PaymentRequest {
    /// Profile owner who opened the request. Only they can cancel or close it.
    pub requester: Pubkey,
    /// Wallet that gets paid. Can differ from the requester, for example a
    /// company's treasury wallet.
    pub receiver: Pubkey,
    pub mint: Pubkey,
    pub is_confidential: bool,
    /// Amount owed for a plain request. Always zero on confidential ones.
    pub amount: u64,
    /// keccak256 of the amount's little endian bytes followed by the blinding
    /// factor. All zeros on plain requests.
    pub amount_commitment: [u8; 32],
    /// Hash of an encrypted memo kept off chain.
    pub memo_hash: [u8; 32],
    pub status: RequestStatus,
    pub index: u64,
    pub created_at: i64,
    /// After this time the request can no longer be paid, and the requester
    /// may close it even if it was never paid.
    pub expires_at: i64,
    pub bump: u8,
}

/// Proof of record that a profile produced a selective disclosure for one
/// of its confidential transfers. The disclosure itself is built off chain
/// from the view key; only hashes and a timestamp are stored here.
#[account]
#[derive(InitSpace)]
pub struct DisclosureReceipt {
    pub profile: Pubkey,
    /// Points at the confidential transfer, usually by its base58 signature.
    #[max_len(MAX_TX_REFERENCE_LEN)]
    pub tx_reference: String,
    /// Hash of whoever received the disclosure. Storing the hash instead of
    /// an identity keeps the counterparty private.
    pub viewer_hash: [u8; 32],
    /// Hash of the disclosed payload, so the shared copy can be checked
    /// against this receipt later.
    pub disclosure_commitment: [u8; 32],
    pub index: u64,
    pub filed_at: i64,
    pub bump: u8,
}

/// How much $VEILED one wallet has locked in the stake vault. There is no
/// lockup period; staking simply makes the tokens count fully toward the
/// fee discount instead of at the reduced held rate.
#[account]
#[derive(InitSpace)]
pub struct StakePosition {
    pub owner: Pubkey,
    pub amount: u64,
    /// When the wallet went from nothing staked to something staked. Adding
    /// more keeps the original date; unstaking everything resets it to zero.
    pub staked_since: i64,
    pub bump: u8,
}

/// Checks the allowed character set for handles: `a-z`, `0-9` and `_`.
/// Keeping it this narrow means a handle never needs escaping inside a URL
/// or QR payload, and cannot be spoofed with lookalike characters.
pub fn is_valid_handle(handle: &str) -> bool {
    handle
        .bytes()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
}
