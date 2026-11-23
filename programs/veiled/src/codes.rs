use anchor_lang::prelude::*;

/// Errors the `veiled` program can return. Anchor numbers these by position
/// starting at 6000, and the SDK maps those numbers back to names, so new
/// variants go at the end and existing ones never move.
#[error_code]
pub enum VeiledError {
    #[msg("Handles need 1 to 32 characters")]
    InvalidHandleLength,
    #[msg("Handles can use only lowercase letters, numbers, and underscores")]
    InvalidHandleCharacters,
    #[msg("KYC tiers can only be set by the compliance authority")]
    UnauthorizedComplianceAuthority,
    #[msg("The protocol is paused, so instructions that move funds are disabled")]
    ProtocolPaused,
    #[msg("Signer is not the profile that owns this agent")]
    UnauthorizedAgentOwner,
    #[msg("Signer is not this agent's registered signing key")]
    UnauthorizedAgentSigner,
    #[msg("Agent labels need 1 to 40 characters")]
    InvalidLabelLength,
    #[msg("HITL threshold has to be above zero")]
    InvalidHitlThreshold,
    #[msg("Amounts and spend limits have to be above zero")]
    InvalidSpendAmount,
    #[msg("Per-transaction limit is larger than the daily limit")]
    PerTxLimitExceedsDailyLimit,
    #[msg("The allowlist holds no more than 10 recipients")]
    TooManyAllowedRecipients,
    #[msg("Agent is paused or revoked and cannot spend")]
    AgentNotActive,
    #[msg("Agent is revoked, and revocation only happens through revoke_agent")]
    AgentAlreadyRevoked,
    #[msg("New signing key is identical to the current one")]
    SameSigner,
    #[msg("Supervised agents cannot settle directly; queue the spend instead")]
    TierRequiresApproval,
    #[msg("Spend is above the HITL threshold and has to be queued for approval")]
    AmountExceedsHitlThreshold,
    #[msg("Spend is at or below the HITL threshold and should be paid directly")]
    AmountWithinHitlThreshold,
    #[msg("Spend is larger than the agent's per-transaction limit")]
    PerTransactionLimitExceeded,
    #[msg("Spend would push the agent past its 24 hour limit")]
    DailyLimitExceeded,
    #[msg("Recipient is not on this agent's allowlist")]
    RecipientNotAllowed,
    #[msg("Agent vault balance is too low for this amount and its fee")]
    InsufficientVaultBalance,
    #[msg("Expiry time has to be later than now")]
    InvalidExpiry,
    #[msg("Payment request is no longer open")]
    RequestNotOpen,
    #[msg("Payment request is past its expiry")]
    RequestExpired,
    #[msg("Amount and blinding factor do not open the request's commitment")]
    RequestCommitmentMismatch,
    #[msg("Amount differs from the amount on the request")]
    RequestAmountMismatch,
    #[msg("Signer or account owner is not permitted here")]
    Unauthorized,
    #[msg("Signer is not the nominated pending authority")]
    NotPendingAuthority,
    #[msg("Rejected setting: a required key is unset, or a fee value or tier order is out of range")]
    InvalidFeeConfig,
    #[msg("Transaction references need 1 to 88 characters")]
    InvalidTxReferenceLength,
    #[msg("Not enough staked to unstake that amount")]
    InsufficientStakedBalance,
    #[msg("Fees are switched on but a required fee account was not passed")]
    MissingFeeAccounts,
    #[msg("A fee account passed in does not belong to the expected mint or owner")]
    InvalidFeeAccount,
    #[msg("Token mint differs from the mint this agent or request settles in")]
    InvalidMint,
    #[msg("A token amount went out of range during arithmetic")]
    AmountOverflow,
    #[msg("An enabled allowlist needs at least one recipient")]
    EmptyAllowlist,
    #[msg("Open requests cannot be closed until they expire")]
    RequestStillOpen,
    #[msg("New handle is identical to the current one")]
    SameHandle,
}
