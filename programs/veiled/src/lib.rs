//! Veiled: an on-chain banking layer for people and their AI agents, built on Solana.
//!
//! Amount privacy does not live in this program. It comes from Token-2022 and
//! its confidential transfer extension, which stores balances and transfer
//! amounts as twisted ElGamal ciphertexts and has Solana's ZK proof program
//! verify them. What this program adds is everything needed to run an
//! account on top of that: a `.veiled` handle per user, agents that can only
//! spend inside limits the chain checks, a queue where a person signs off on
//! larger spends, payment request links, disclosure receipts, and the $VEILED
//! staking and fee logic.
//!
//! Keep in mind that wallets on both ends of a payment are visible on-chain.
//! Only the amounts are hidden.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::keccak;
use anchor_spl::token_interface::{
    self, Mint, TokenAccount, TokenInterface, TransferChecked,
};

pub mod codes;
pub mod logs;
pub mod pricing;
pub mod model;

use codes::VeiledError;
use logs::*;
use pricing::{quote_fee, FeeQuote};
use model::*;

declare_id!("BgJQoxfRfpQYhJLFxnq7f8GcxPoAce47MGPnq1qJiV7V");

#[program]
pub mod veiled {
    use super::*;

    // ---- Admin keys, pause switch and fee routing ----

    /// One-time setup of the global config account. Whoever signs is recorded
    /// as admin, the given key becomes the compliance authority (it may not be
    /// the default key), the protocol starts unpaused and no fees are taken.
    pub fn init_config(ctx: Context<InitConfig>, compliance_authority: Pubkey) -> Result<()> {
        require!(
            compliance_authority != Pubkey::default(),
            VeiledError::InvalidFeeConfig
        );
        let config = &mut ctx.accounts.config;
        config.authority = ctx.accounts.authority.key();
        config.pending_authority = None;
        config.compliance_authority = compliance_authority;
        config.paused = false;
        config.treasury = Pubkey::default();
        config.fees_enabled = false;
        config.bump = ctx.bumps.config;

        emit!(ConfigAuthorityUpdated {
            authority: config.authority,
            compliance_authority,
        });
        Ok(())
    }

    /// Admin-only. Replaces both the admin key and the compliance key in a
    /// single step. This is how control can be moved to a multisig such as
    /// Squads, or how a new KYC provider gets wired in, with no program upgrade.
    pub fn update_config(
        ctx: Context<AdminConfig>,
        new_authority: Pubkey,
        new_compliance_authority: Pubkey,
    ) -> Result<()> {
        require!(
            new_authority != Pubkey::default() && new_compliance_authority != Pubkey::default(),
            VeiledError::InvalidFeeConfig
        );
        let config = &mut ctx.accounts.config;
        config.authority = new_authority;
        config.compliance_authority = new_compliance_authority;
        // Any nominee from an unfinished two-step handoff loses their claim here.
        config.pending_authority = None;
        emit!(ConfigAuthorityUpdated {
            authority: new_authority,
            compliance_authority: new_compliance_authority,
        });
        Ok(())
    }

    /// First half of a two-step admin handoff: records who should take over,
    /// while the current admin keeps full control. Control only moves once
    /// that key signs `accept_authority`. Send `None` to withdraw a nomination.
    pub fn begin_authority_transfer(
        ctx: Context<AdminConfig>,
        new_authority: Option<Pubkey>,
    ) -> Result<()> {
        let config = &mut ctx.accounts.config;
        config.pending_authority = new_authority;
        emit!(AuthorityTransferStarted {
            current_authority: config.authority,
            pending_authority: new_authority,
        });
        Ok(())
    }

    /// Second half of the admin handoff. The signer has to be the exact key
    /// that was nominated. Requiring that key to sign proves someone holds it,
    /// which is why a typo can never strand admin rights (and the pause switch)
    /// on an unusable address.
    pub fn accept_authority(ctx: Context<AcceptAuthority>) -> Result<()> {
        let config = &mut ctx.accounts.config;
        require!(
            config.pending_authority == Some(ctx.accounts.pending_authority.key()),
            VeiledError::NotPendingAuthority
        );
        let previous = config.authority;
        config.authority = ctx.accounts.pending_authority.key();
        config.pending_authority = None;
        emit!(AuthorityTransferAccepted {
            previous_authority: previous,
            new_authority: config.authority,
        });
        Ok(())
    }

    /// Admin-only emergency switch. While it is on, agent payments, queuing
    /// and approving payments, and request fulfillment all fail. Profile changes,
    /// agent settings, revocation and withdrawals stay available, because
    /// owners need those most during an incident.
    pub fn set_pause(ctx: Context<AdminConfig>, paused: bool) -> Result<()> {
        ctx.accounts.config.paused = paused;
        emit!(ProtocolPauseToggled { paused });
        Ok(())
    }

    /// Admin-only. Chooses the wallet that collects protocol fees and whether
    /// fees are charged at all. Turning fees on requires a real treasury key;
    /// with fees off, the treasury may be left blank.
    pub fn set_fee_routing(
        ctx: Context<AdminConfig>,
        treasury: Pubkey,
        fees_enabled: bool,
    ) -> Result<()> {
        require!(
            !fees_enabled || treasury != Pubkey::default(),
            VeiledError::InvalidFeeConfig
        );
        let config = &mut ctx.accounts.config;
        config.treasury = treasury;
        config.fees_enabled = fees_enabled;
        emit!(FeeRoutingUpdated {
            treasury,
            fees_enabled,
        });
        Ok(())
    }

    // ---- Pricing and $VEILED staking ----

    /// Admin-only, run once. Creates the fee config and the vault that holds
    /// staked $VEILED, seeded with launch pricing: 10 bps per payment up to a
    /// maximum of 5 USDG, tokens sitting in a wallet counted at 50% of staked
    /// tokens, and discounts of 10%, 25%, 50% and 75% starting at 1k, 10k,
    /// 100k and 1M $VEILED.
    pub fn init_fees(ctx: Context<InitFees>) -> Result<()> {
        let unit = 10u64.pow(ctx.accounts.veiled_mint.decimals as u32);
        let fee_config = &mut ctx.accounts.fee_config;
        fee_config.authority = ctx.accounts.authority.key();
        fee_config.pending_authority = None;
        fee_config.veiled_mint = ctx.accounts.veiled_mint.key();
        fee_config.base_fee_bps = 10;
        fee_config.fee_cap = 5_000_000; // USDG uses 6 decimals, so this is 5.00
        fee_config.held_weight_bps = 5_000;
        fee_config.tiers = vec![
            FeeTier { min_weight: 1_000 * unit, discount_bps: 1_000 },
            FeeTier { min_weight: 10_000 * unit, discount_bps: 2_500 },
            FeeTier { min_weight: 100_000 * unit, discount_bps: 5_000 },
            FeeTier { min_weight: 1_000_000 * unit, discount_bps: 7_500 },
        ];
        fee_config.total_staked = 0;
        fee_config.bump = ctx.bumps.fee_config;

        emit!(FeeScheduleUpdated {
            base_fee_bps: fee_config.base_fee_bps,
            fee_cap: fee_config.fee_cap,
            held_weight_bps: fee_config.held_weight_bps,
        });
        Ok(())
    }

    /// Fee admin only. Sets the base rate in bps, the per-payment fee ceiling,
    /// and how much wallet-held $VEILED counts toward a discount compared with
    /// staked $VEILED. Both bps values must be 10,000 or less.
    pub fn set_fee_schedule(
        ctx: Context<AdminFees>,
        base_fee_bps: u16,
        fee_cap: u64,
        held_weight_bps: u16,
    ) -> Result<()> {
        require!(
            base_fee_bps as u64 <= BPS && held_weight_bps as u64 <= BPS,
            VeiledError::InvalidFeeConfig
        );
        let fee_config = &mut ctx.accounts.fee_config;
        fee_config.base_fee_bps = base_fee_bps;
        fee_config.fee_cap = fee_cap;
        fee_config.held_weight_bps = held_weight_bps;
        emit!(FeeScheduleUpdated {
            base_fee_bps,
            fee_cap,
            held_weight_bps,
        });
        Ok(())
    }

    /// Fee admin only. Swaps in a whole new discount table. Each entry needs a
    /// higher `min_weight` than the one before it, no discount can go past
    /// 10,000 bps, and the table is limited to `MAX_FEE_TIERS` entries.
    pub fn set_fee_tiers(ctx: Context<AdminFees>, tiers: Vec<FeeTier>) -> Result<()> {
        require!(tiers.len() <= MAX_FEE_TIERS, VeiledError::InvalidFeeConfig);
        for (i, tier) in tiers.iter().enumerate() {
            require!(tier.discount_bps as u64 <= BPS, VeiledError::InvalidFeeConfig);
            if i > 0 {
                require!(
                    tier.min_weight > tiers[i - 1].min_weight,
                    VeiledError::InvalidFeeConfig
                );
            }
        }
        let fee_config = &mut ctx.accounts.fee_config;
        fee_config.tiers = tiers;
        emit!(TiersUpdated {
            count: fee_config.tiers.len() as u64,
        });
        Ok(())
    }

    /// Starts handing the fee admin role to another key, or cancels that with
    /// `None`. It follows the same nominate-then-accept flow as the main admin
    /// role so there is only one handoff pattern to learn.
    pub fn begin_fee_authority_transfer(
        ctx: Context<AdminFees>,
        new_authority: Option<Pubkey>,
    ) -> Result<()> {
        let fee_config = &mut ctx.accounts.fee_config;
        fee_config.pending_authority = new_authority;
        emit!(AuthorityTransferStarted {
            current_authority: fee_config.authority,
            pending_authority: new_authority,
        });
        Ok(())
    }

    /// Finishes a fee admin handoff. Fails unless the signer is the nominee.
    pub fn accept_fee_authority(ctx: Context<AcceptFeeAuthority>) -> Result<()> {
        let fee_config = &mut ctx.accounts.fee_config;
        require!(
            fee_config.pending_authority == Some(ctx.accounts.pending_authority.key()),
            VeiledError::NotPendingAuthority
        );
        let previous = fee_config.authority;
        fee_config.authority = ctx.accounts.pending_authority.key();
        fee_config.pending_authority = None;
        emit!(AuthorityTransferAccepted {
            previous_authority: previous,
            new_authority: fee_config.authority,
        });
        Ok(())
    }

    /// Moves $VEILED from the signer's wallet into the shared stake vault and
    /// adds it to their stake position, creating the position on first use.
    /// Staked tokens get full weight when fee discounts are worked out.
    pub fn stake(ctx: Context<StakeTokens>, amount: u64) -> Result<()> {
        require!(amount > 0, VeiledError::InvalidSpendAmount);
        let now = Clock::get()?.unix_timestamp;

        token_interface::transfer_checked(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                TransferChecked {
                    from: ctx.accounts.staker_token_account.to_account_info(),
                    mint: ctx.accounts.veiled_mint.to_account_info(),
                    to: ctx.accounts.stake_vault.to_account_info(),
                    authority: ctx.accounts.staker.to_account_info(),
                },
            ),
            amount,
            ctx.accounts.veiled_mint.decimals,
        )?;

        let position = &mut ctx.accounts.stake_position;
        position.owner = ctx.accounts.staker.key();
        position.bump = ctx.bumps.stake_position;
        // Only an empty position gets a new start time. Adding to a stake keeps
        // the original date, so long-term stakers keep their history.
        if position.amount == 0 {
            position.staked_since = now;
        }
        position.amount = position
            .amount
            .checked_add(amount)
            .ok_or(VeiledError::AmountOverflow)?;

        let fee_config = &mut ctx.accounts.fee_config;
        fee_config.total_staked = fee_config
            .total_staked
            .checked_add(amount)
            .ok_or(VeiledError::AmountOverflow)?;

        emit!(Staked {
            account: position.owner,
            amount,
            new_balance: position.amount,
            timestamp: now,
        });
        Ok(())
    }

    /// Returns staked $VEILED to the signer's wallet. There is no lockup or
    /// cooldown, so tokens come back in the same transaction.
    pub fn unstake(ctx: Context<UnstakeTokens>, amount: u64) -> Result<()> {
        require!(amount > 0, VeiledError::InvalidSpendAmount);
        let position = &mut ctx.accounts.stake_position;
        require!(amount <= position.amount, VeiledError::InsufficientStakedBalance);

        position.amount -= amount;
        // Withdrawing everything resets the start time to zero.
        if position.amount == 0 {
            position.staked_since = 0;
        }
        let fee_config = &mut ctx.accounts.fee_config;
        fee_config.total_staked = fee_config
            .total_staked
            .checked_sub(amount)
            .ok_or(VeiledError::AmountOverflow)?;

        let seeds: &[&[u8]] = &[FEES_SEED, &[fee_config.bump]];
        token_interface::transfer_checked(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                TransferChecked {
                    from: ctx.accounts.stake_vault.to_account_info(),
                    mint: ctx.accounts.veiled_mint.to_account_info(),
                    to: ctx.accounts.staker_token_account.to_account_info(),
                    authority: fee_config.to_account_info(),
                },
                &[seeds],
            ),
            amount,
            ctx.accounts.veiled_mint.decimals,
        )?;

        emit!(Unstaked {
            account: ctx.accounts.staker.key(),
            amount,
            new_balance: ctx.accounts.stake_position.amount,
            timestamp: Clock::get()?.unix_timestamp,
        });
        Ok(())
    }

    // ---- Profiles and handles ----

    /// Opens a profile for the signer and claims the requested `.veiled`
    /// handle for it, failing if someone already holds that handle. New
    /// profiles have no KYC tier until the compliance key records one.
    pub fn create_profile(
        ctx: Context<CreateProfile>,
        handle: String,
        account_kind: AccountKind,
    ) -> Result<()> {
        validate_handle(&handle)?;
        let now = Clock::get()?.unix_timestamp;

        let profile = &mut ctx.accounts.profile;
        profile.owner = ctx.accounts.owner.key();
        profile.handle = handle.clone();
        profile.kyc_tier = KycTier::Unverified;
        profile.account_kind = account_kind;
        profile.agent_count = 0;
        profile.request_count = 0;
        profile.disclosure_count = 0;
        profile.created_at = now;
        profile.updated_at = now;
        profile.bump = ctx.bumps.profile;

        let claim = &mut ctx.accounts.handle_claim;
        claim.owner = ctx.accounts.owner.key();
        claim.bump = ctx.bumps.handle_claim;

        emit!(ProfileCreated {
            owner: profile.owner,
            handle,
            account_kind,
            timestamp: now,
        });
        Ok(())
    }

    /// Renames the signer's profile. The new handle is claimed, the old claim
    /// is closed so another user can take that name, and nothing else on the
    /// profile changes apart from its update time.
    pub fn change_handle(ctx: Context<ChangeHandle>, new_handle: String) -> Result<()> {
        validate_handle(&new_handle)?;
        let now = Clock::get()?.unix_timestamp;

        let profile = &mut ctx.accounts.profile;
        let previous = profile.handle.clone();
        profile.handle = new_handle.clone();
        profile.updated_at = now;

        let claim = &mut ctx.accounts.new_handle_claim;
        claim.owner = ctx.accounts.owner.key();
        claim.bump = ctx.bumps.new_handle_claim;

        emit!(HandleChanged {
            owner: profile.owner,
            previous_handle: previous,
            new_handle,
            timestamp: now,
        });
        Ok(())
    }

    /// Compliance key only. Saves the outcome of identity verification done
    /// off-chain onto a profile. No tokens are involved; the stored tier is
    /// there for other limits and features to read.
    pub fn set_kyc_tier(ctx: Context<SetKycTier>, kyc_tier: KycTier) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let profile = &mut ctx.accounts.profile;
        profile.kyc_tier = kyc_tier;
        profile.updated_at = now;
        emit!(KycTierUpdated {
            owner: profile.owner,
            kyc_tier,
            timestamp: now,
        });
        Ok(())
    }

    // ---- Agents, vaults and spending ----

    /// Sets up a new agent for the signer: the agent account with its limits,
    /// plus a vault for one settlement mint that only this program can move
    /// funds out of. `agent_signer` is the public key the agent will sign its
    /// payments with. The agent starts out active.
    #[allow(clippy::too_many_arguments)]
    pub fn create_agent(
        ctx: Context<CreateAgent>,
        agent_signer: Pubkey,
        label: String,
        autonomy_tier: AutonomyTier,
        per_tx_limit: u64,
        daily_limit: u64,
        hitl_threshold: u64,
        allowlist_enabled: bool,
        allowed_recipients: Vec<Pubkey>,
    ) -> Result<()> {
        require!(agent_signer != Pubkey::default(), VeiledError::InvalidFeeConfig);
        require!(
            !label.is_empty() && label.len() <= MAX_LABEL_LEN,
            VeiledError::InvalidLabelLength
        );
        validate_limits(
            per_tx_limit,
            daily_limit,
            hitl_threshold,
            allowlist_enabled,
            allowed_recipients.len(),
        )?;
        let now = Clock::get()?.unix_timestamp;

        let agent = &mut ctx.accounts.agent;
        agent.owner = ctx.accounts.owner.key();
        agent.agent_signer = agent_signer;
        agent.label = label.clone();
        agent.autonomy_tier = autonomy_tier;
        agent.status = AgentStatus::Active;
        agent.mint = ctx.accounts.mint.key();
        agent.per_tx_limit = per_tx_limit;
        agent.daily_limit = daily_limit;
        agent.hitl_threshold = hitl_threshold;
        agent.window_start = now;
        agent.window_spent = 0;
        agent.allowlist_enabled = allowlist_enabled;
        agent.allowed_recipients = allowed_recipients.clone();
        agent.pending_count = 0;
        agent.index = ctx.accounts.profile.agent_count;
        agent.created_at = now;
        agent.updated_at = now;
        agent.bump = ctx.bumps.agent;
        agent.vault_bump = ctx.bumps.vault;

        let profile = &mut ctx.accounts.profile;
        profile.agent_count += 1;

        emit!(AgentCreated {
            agent: agent.key(),
            owner: agent.owner,
            agent_signer,
            label,
            autonomy_tier,
            mint: agent.mint,
            timestamp: now,
        });
        emit!(SpendPolicyUpdated {
            agent: agent.key(),
            per_tx_limit,
            daily_limit,
            hitl_threshold,
            allowlist_enabled,
            allowed_recipients,
            timestamp: now,
        });
        Ok(())
    }

    /// Owner only. Replaces an agent's per-payment limit, daily limit, human
    /// approval threshold and allowlist. The agent key has no way to call
    /// this, so an agent can never raise its own limits.
    pub fn update_spend_policy(
        ctx: Context<OwnerAgent>,
        per_tx_limit: u64,
        daily_limit: u64,
        hitl_threshold: u64,
        allowlist_enabled: bool,
        allowed_recipients: Vec<Pubkey>,
    ) -> Result<()> {
        validate_limits(
            per_tx_limit,
            daily_limit,
            hitl_threshold,
            allowlist_enabled,
            allowed_recipients.len(),
        )?;
        let now = Clock::get()?.unix_timestamp;

        let agent = &mut ctx.accounts.agent;
        agent.per_tx_limit = per_tx_limit;
        agent.daily_limit = daily_limit;
        agent.hitl_threshold = hitl_threshold;
        agent.allowlist_enabled = allowlist_enabled;
        agent.allowed_recipients = allowed_recipients.clone();
        agent.updated_at = now;

        emit!(SpendPolicyUpdated {
            agent: agent.key(),
            per_tx_limit,
            daily_limit,
            hitl_threshold,
            allowlist_enabled,
            allowed_recipients,
            timestamp: now,
        });
        Ok(())
    }

    /// Owner only. Switches an agent between active and paused while leaving
    /// its funds and limits alone, which suits a temporary stop. It cannot set
    /// the revoked state or bring back a revoked agent; use `revoke_agent` for
    /// a permanent stop.
    pub fn set_agent_status(ctx: Context<OwnerAgent>, status: AgentStatus) -> Result<()> {
        require!(status != AgentStatus::Revoked, VeiledError::AgentAlreadyRevoked);
        let agent = &mut ctx.accounts.agent;
        require!(agent.status != AgentStatus::Revoked, VeiledError::AgentAlreadyRevoked);
        let now = Clock::get()?.unix_timestamp;
        agent.status = status;
        agent.updated_at = now;
        emit!(AgentStatusChanged {
            agent: agent.key(),
            status,
            timestamp: now,
        });
        Ok(())
    }

    /// Owner only. Moves an agent to a different autonomy tier, for example
    /// loosening a Supervised agent once it has earned some trust, or pulling
    /// a misbehaving one back to Supervised without pausing it. Spends already
    /// sitting in the queue are unaffected and still need the owner's call.
    pub fn set_autonomy_tier(ctx: Context<OwnerAgent>, autonomy_tier: AutonomyTier) -> Result<()> {
        let agent = &mut ctx.accounts.agent;
        require!(agent.status != AgentStatus::Revoked, VeiledError::AgentAlreadyRevoked);

        let now = Clock::get()?.unix_timestamp;
        let previous = agent.autonomy_tier;
        agent.autonomy_tier = autonomy_tier;
        agent.updated_at = now;
        emit!(AgentAutonomyChanged {
            agent: agent.key(),
            previous_tier: previous,
            new_tier: autonomy_tier,
            timestamp: now,
        });
        Ok(())
    }

    /// Owner only. Renames an agent. The label is display-only and not part of
    /// any seed, so this touches nothing but the label and the update time.
    /// Revoked agents keep the name they had when they were shut down.
    pub fn set_agent_label(ctx: Context<OwnerAgent>, label: String) -> Result<()> {
        require!(
            !label.is_empty() && label.len() <= MAX_LABEL_LEN,
            VeiledError::InvalidLabelLength
        );
        let agent = &mut ctx.accounts.agent;
        require!(agent.status != AgentStatus::Revoked, VeiledError::AgentAlreadyRevoked);

        let now = Clock::get()?.unix_timestamp;
        let previous = std::mem::replace(&mut agent.label, label.clone());
        agent.updated_at = now;
        emit!(AgentLabelUpdated {
            agent: agent.key(),
            previous_label: previous,
            new_label: label,
            timestamp: now,
        });
        Ok(())
    }

    /// Owner only. Points the agent at a new signing key. The vault, limits
    /// and today's spend total stay as they are, so a leaked key can be
    /// replaced without shutting the agent down for good.
    pub fn rotate_agent_signer(ctx: Context<OwnerAgent>, new_signer: Pubkey) -> Result<()> {
        require!(new_signer != Pubkey::default(), VeiledError::InvalidFeeConfig);
        let agent = &mut ctx.accounts.agent;
        require!(agent.status != AgentStatus::Revoked, VeiledError::AgentAlreadyRevoked);
        require!(agent.agent_signer != new_signer, VeiledError::SameSigner);

        let now = Clock::get()?.unix_timestamp;
        let previous = agent.agent_signer;
        agent.agent_signer = new_signer;
        agent.updated_at = now;
        emit!(AgentSignerRotated {
            agent: agent.key(),
            previous_signer: previous,
            new_signer,
            timestamp: now,
        });
        Ok(())
    }

    /// Owner only. Shuts an agent down for good and sends whatever is left in
    /// its vault to the owner's token account. There is no way back from this
    /// state. It works even when the protocol is paused, since owners must be
    /// able to get their money out during an incident.
    pub fn revoke_agent(ctx: Context<RevokeAgent>) -> Result<()> {
        let agent = &mut ctx.accounts.agent;
        require!(agent.status != AgentStatus::Revoked, VeiledError::AgentAlreadyRevoked);
        let now = Clock::get()?.unix_timestamp;
        agent.status = AgentStatus::Revoked;
        agent.updated_at = now;

        let remaining = ctx.accounts.vault.amount;
        if remaining > 0 {
            settle_from_vault(
                &ctx.accounts.token_program,
                &ctx.accounts.vault,
                &ctx.accounts.mint,
                &ctx.accounts.owner_token_account,
                &None,
                agent,
                remaining,
                None,
            )?;
        }

        emit!(AgentStatusChanged {
            agent: agent.key(),
            status: AgentStatus::Revoked,
            timestamp: now,
        });
        Ok(())
    }

    /// Tops up an agent's vault with its settlement mint. Any wallet may fund
    /// any agent that has not been revoked, although in practice it is
    /// usually the owner. Money only arrives when someone sends it.
    pub fn fund_agent(ctx: Context<FundAgent>, amount: u64) -> Result<()> {
        require!(amount > 0, VeiledError::InvalidSpendAmount);
        require!(
            ctx.accounts.agent.status != AgentStatus::Revoked,
            VeiledError::AgentAlreadyRevoked
        );

        token_interface::transfer_checked(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                TransferChecked {
                    from: ctx.accounts.funder_token_account.to_account_info(),
                    mint: ctx.accounts.mint.to_account_info(),
                    to: ctx.accounts.vault.to_account_info(),
                    authority: ctx.accounts.funder.to_account_info(),
                },
            ),
            amount,
            ctx.accounts.mint.decimals,
        )?;

        emit!(AgentFunded {
            agent: ctx.accounts.agent.key(),
            funder: ctx.accounts.funder.key(),
            amount,
            timestamp: Clock::get()?.unix_timestamp,
        });
        Ok(())
    }

    /// Owner only. Pulls some of the vault balance back to the owner while the
    /// agent carries on, which is useful when a vault holds more than it needs.
    /// It ignores the protocol pause and the agent's status on purpose, and the
    /// agent's signing key has no access to it.
    pub fn withdraw_from_agent(ctx: Context<WithdrawFromAgent>, amount: u64) -> Result<()> {
        require!(amount > 0, VeiledError::InvalidSpendAmount);
        require!(
            amount <= ctx.accounts.vault.amount,
            VeiledError::InsufficientVaultBalance
        );

        let agent = &ctx.accounts.agent;
        settle_from_vault(
            &ctx.accounts.token_program,
            &ctx.accounts.vault,
            &ctx.accounts.mint,
            &ctx.accounts.owner_token_account,
            &None,
            agent,
            amount,
            None,
        )?;

        emit!(AgentDefunded {
            agent: agent.key(),
            owner: agent.owner,
            amount,
            timestamp: Clock::get()?.unix_timestamp,
        });
        Ok(())
    }

    /// Signed by the agent key. Pays a recipient straight from the vault with
    /// no human step. What is allowed depends on the autonomy tier: Supervised
    /// agents are always refused, SemiAutonomous agents may pay amounts up to
    /// and including the HITL threshold, and FullyAutonomous agents may pay
    /// anything their limits permit. Every tier is still held to the
    /// per-payment limit, the rolling daily limit and the allowlist. Any
    /// protocol fee comes out of the vault as well as the payment itself.
    pub fn pay_invoice(
        ctx: Context<PayInvoice>,
        amount: u64,
        invoice_id: [u8; 32],
    ) -> Result<()> {
        require!(!ctx.accounts.config.paused, VeiledError::ProtocolPaused);
        require!(amount > 0, VeiledError::InvalidSpendAmount);

        let now = Clock::get()?.unix_timestamp;
        let agent = &mut ctx.accounts.agent;
        require!(agent.status == AgentStatus::Active, VeiledError::AgentNotActive);
        require!(
            agent.autonomy_tier != AutonomyTier::Supervised,
            VeiledError::TierRequiresApproval
        );
        require!(amount <= agent.per_tx_limit, VeiledError::PerTransactionLimitExceeded);
        if agent.autonomy_tier == AutonomyTier::SemiAutonomous {
            require!(
                amount <= agent.hitl_threshold,
                VeiledError::AmountExceedsHitlThreshold
            );
        }
        check_allowlist(agent, &ctx.accounts.recipient.key())?;

        roll_window(agent, now);
        let projected = agent
            .window_spent
            .checked_add(amount)
            .ok_or(VeiledError::DailyLimitExceeded)?;
        require!(projected <= agent.daily_limit, VeiledError::DailyLimitExceeded);

        let quote = quote_fee(
            &ctx.accounts.config,
            &ctx.accounts.fee_config,
            &agent.owner,
            &ctx.accounts.owner_veiled_token_account,
            &ctx.accounts.owner_stake_position,
            amount,
        )?;
        let fee = quote.as_ref().map_or(0, |q| q.fee);
        let total_debit = amount
            .checked_add(fee)
            .ok_or(VeiledError::AmountOverflow)?;
        require!(
            total_debit <= ctx.accounts.vault.amount,
            VeiledError::InsufficientVaultBalance
        );

        agent.window_spent = projected;
        agent.updated_at = now;

        settle_from_vault(
            &ctx.accounts.token_program,
            &ctx.accounts.vault,
            &ctx.accounts.mint,
            &ctx.accounts.recipient_token_account,
            &ctx.accounts.treasury_token_account,
            agent,
            amount,
            quote,
        )?;

        emit!(AgentPaymentExecuted {
            agent: agent.key(),
            recipient: ctx.accounts.recipient.key(),
            mint: agent.mint,
            amount,
            invoice_id,
            window_spent: projected,
            timestamp: now,
        });
        Ok(())
    }

    /// Signed by the agent key. Puts a payment in front of the owner instead of
    /// paying it; nothing leaves the vault yet. Supervised agents send every
    /// payment through here, and FullyAutonomous agents can choose to. A
    /// SemiAutonomous agent may only queue amounts above its HITL threshold,
    /// since smaller ones should go through `pay_invoice`. The allowlist and
    /// per-payment limit are enforced up front to keep junk out of the queue.
    /// The daily limit is left until the owner approves, because the total
    /// may look different by then. The agent key pays the rent for the record.
    pub fn queue_invoice(
        ctx: Context<QueueInvoice>,
        amount: u64,
        invoice_id: [u8; 32],
        memo_hash: [u8; 32],
    ) -> Result<()> {
        require!(!ctx.accounts.config.paused, VeiledError::ProtocolPaused);
        require!(amount > 0, VeiledError::InvalidSpendAmount);

        let now = Clock::get()?.unix_timestamp;
        let agent = &mut ctx.accounts.agent;
        require!(agent.status == AgentStatus::Active, VeiledError::AgentNotActive);
        require!(amount <= agent.per_tx_limit, VeiledError::PerTransactionLimitExceeded);
        if agent.autonomy_tier == AutonomyTier::SemiAutonomous {
            require!(
                amount > agent.hitl_threshold,
                VeiledError::AmountWithinHitlThreshold
            );
        }
        check_allowlist(agent, &ctx.accounts.recipient.key())?;

        let pending = &mut ctx.accounts.pending;
        pending.agent = agent.key();
        pending.owner = agent.owner;
        pending.recipient = ctx.accounts.recipient.key();
        pending.mint = agent.mint;
        pending.amount = amount;
        pending.invoice_id = invoice_id;
        pending.memo_hash = memo_hash;
        pending.index = agent.pending_count;
        pending.created_at = now;
        pending.bump = ctx.bumps.pending;

        agent.pending_count += 1;

        emit!(ApprovalRequired {
            agent: agent.key(),
            pending_index: pending.index,
            recipient: pending.recipient,
            mint: pending.mint,
            amount,
            invoice_id,
            timestamp: now,
        });
        Ok(())
    }

    /// Owner only. Pays out a queued payment from the vault and closes the
    /// queue record, with its rent going to the owner. Status, allowlist,
    /// per-payment limit and daily limit are all checked again using today's
    /// values, because any of them could have changed since it was queued.
    pub fn approve_pending(ctx: Context<ApprovePending>) -> Result<()> {
        require!(!ctx.accounts.config.paused, VeiledError::ProtocolPaused);

        let now = Clock::get()?.unix_timestamp;
        let amount = ctx.accounts.pending.amount;
        let invoice_id = ctx.accounts.pending.invoice_id;
        let pending_index = ctx.accounts.pending.index;
        let recipient = ctx.accounts.pending.recipient;

        let agent = &mut ctx.accounts.agent;
        require!(agent.status == AgentStatus::Active, VeiledError::AgentNotActive);
        check_allowlist(agent, &recipient)?;
        require!(amount <= agent.per_tx_limit, VeiledError::PerTransactionLimitExceeded);

        roll_window(agent, now);
        let projected = agent
            .window_spent
            .checked_add(amount)
            .ok_or(VeiledError::DailyLimitExceeded)?;
        require!(projected <= agent.daily_limit, VeiledError::DailyLimitExceeded);

        let quote = quote_fee(
            &ctx.accounts.config,
            &ctx.accounts.fee_config,
            &agent.owner,
            &ctx.accounts.owner_veiled_token_account,
            &ctx.accounts.owner_stake_position,
            amount,
        )?;
        let fee = quote.as_ref().map_or(0, |q| q.fee);
        let total_debit = amount
            .checked_add(fee)
            .ok_or(VeiledError::AmountOverflow)?;
        require!(
            total_debit <= ctx.accounts.vault.amount,
            VeiledError::InsufficientVaultBalance
        );

        agent.window_spent = projected;
        agent.updated_at = now;

        settle_from_vault(
            &ctx.accounts.token_program,
            &ctx.accounts.vault,
            &ctx.accounts.mint,
            &ctx.accounts.recipient_token_account,
            &ctx.accounts.treasury_token_account,
            agent,
            amount,
            quote,
        )?;

        emit!(AgentPaymentExecuted {
            agent: agent.key(),
            recipient,
            mint: agent.mint,
            amount,
            invoice_id,
            window_spent: projected,
            timestamp: now,
        });
        emit!(PendingPaymentApproved {
            agent: agent.key(),
            pending_index,
            recipient,
            amount,
            timestamp: now,
        });
        Ok(())
    }

    /// Owner only. Turns down a queued payment and closes its record, sending
    /// the rent to the owner. Queuing never moved any tokens, so there is no
    /// refund to make.
    pub fn reject_pending(ctx: Context<RejectPending>) -> Result<()> {
        emit!(PendingPaymentRejected {
            agent: ctx.accounts.agent.key(),
            pending_index: ctx.accounts.pending.index,
            timestamp: Clock::get()?.unix_timestamp,
        });
        Ok(())
    }

    /// Signed by the agent key. Pulls back a spend the agent queued, for
    /// example because the invoice was paid some other way or was wrong. The
    /// record is closed and its rent goes back to the current agent key, which
    /// is the key that paid for it unless the signer was rotated in between.
    /// Works while the protocol is paused, since it moves no tokens.
    pub fn cancel_pending(ctx: Context<CancelPending>) -> Result<()> {
        emit!(PendingPaymentCancelled {
            agent: ctx.accounts.agent.key(),
            pending_index: ctx.accounts.pending.index,
            timestamp: Clock::get()?.unix_timestamp,
        });
        Ok(())
    }

    // ---- Request-to-pay ----

    /// Opens a request-to-pay, the record behind a shareable Veiled link or QR
    /// code, with an expiry that must be in the future and no more than
    /// `MAX_REQUEST_LIFETIME` away. A public request
    /// stores a nonzero `amount`. A confidential one stores only
    /// `amount_commitment`, a keccak256 hash of the amount and a blinding
    /// value; any `amount` passed with it is ignored and saved as zero. The
    /// payer gets the real amount and blinding value through another channel,
    /// for example the request's encrypted memo.
    pub fn create_request(
        ctx: Context<CreateRequest>,
        is_confidential: bool,
        amount: u64,
        amount_commitment: [u8; 32],
        memo_hash: [u8; 32],
        expires_at: i64,
    ) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        require!(expires_at > now, VeiledError::InvalidExpiry);
        require!(
            expires_at - now <= MAX_REQUEST_LIFETIME,
            VeiledError::InvalidExpiry
        );
        require!(is_confidential || amount > 0, VeiledError::InvalidSpendAmount);
        // An all-zero commitment is what plain requests store, and nobody can
        // find an amount and blinding value that hash to it, so a confidential
        // request created with one could never be paid.
        require!(
            !is_confidential || amount_commitment != [0u8; 32],
            VeiledError::MissingAmountCommitment
        );

        let request = &mut ctx.accounts.request;
        request.requester = ctx.accounts.requester.key();
        request.receiver = ctx.accounts.receiver.key();
        request.mint = ctx.accounts.mint.key();
        request.is_confidential = is_confidential;
        request.amount = if is_confidential { 0 } else { amount };
        request.amount_commitment = if is_confidential {
            amount_commitment
        } else {
            [0u8; 32]
        };
        request.memo_hash = memo_hash;
        request.status = RequestStatus::Open;
        request.index = ctx.accounts.profile.request_count;
        request.created_at = now;
        request.expires_at = expires_at;
        request.bump = ctx.bumps.request;

        let profile = &mut ctx.accounts.profile;
        profile.request_count += 1;

        emit!(PaymentRequestCreated {
            request: request.key(),
            requester: request.requester,
            mint: request.mint,
            is_confidential,
            amount: request.amount,
            expires_at,
            timestamp: now,
        });
        Ok(())
    }

    /// Pays an open, unexpired request and marks it fulfilled. For a public
    /// request, `amount` has to equal the stored amount. For a confidential
    /// request, the program hashes the little-endian `amount` together with
    /// `blinding` using keccak256 and compares the result with the stored
    /// commitment, so a wrong amount is caught without the right one ever
    /// being written on-chain. Any protocol fee is taken from the payer as an
    /// extra transfer, and the receiver gets the full amount.
    pub fn fulfill_request(
        ctx: Context<FulfillRequest>,
        amount: u64,
        blinding: [u8; 32],
    ) -> Result<()> {
        require!(!ctx.accounts.config.paused, VeiledError::ProtocolPaused);
        // Without this, someone could commit to zero and then mark a request
        // as paid while sending nothing.
        require!(amount > 0, VeiledError::InvalidSpendAmount);

        let now = Clock::get()?.unix_timestamp;
        let request = &mut ctx.accounts.request;
        require!(request.status == RequestStatus::Open, VeiledError::RequestNotOpen);
        require!(now < request.expires_at, VeiledError::RequestExpired);

        if request.is_confidential {
            let digest = keccak::hashv(&[&amount.to_le_bytes(), &blinding]).to_bytes();
            require!(
                digest == request.amount_commitment,
                VeiledError::RequestCommitmentMismatch
            );
        } else {
            require!(amount == request.amount, VeiledError::RequestAmountMismatch);
        }

        request.status = RequestStatus::Fulfilled;

        let quote = quote_fee(
            &ctx.accounts.config,
            &ctx.accounts.fee_config,
            &ctx.accounts.payer.key(),
            &ctx.accounts.payer_veiled_token_account,
            &ctx.accounts.payer_stake_position,
            amount,
        )?;

        token_interface::transfer_checked(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                TransferChecked {
                    from: ctx.accounts.payer_token_account.to_account_info(),
                    mint: ctx.accounts.mint.to_account_info(),
                    to: ctx.accounts.receiver_token_account.to_account_info(),
                    authority: ctx.accounts.payer.to_account_info(),
                },
            ),
            amount,
            ctx.accounts.mint.decimals,
        )?;

        if let Some(quote) = quote {
            if quote.fee > 0 {
                let treasury_ata = ctx
                    .accounts
                    .treasury_token_account
                    .as_ref()
                    .ok_or(error!(VeiledError::MissingFeeAccounts))?;
                require_keys_eq!(
                    treasury_ata.owner,
                    quote.treasury,
                    VeiledError::InvalidFeeAccount
                );
                token_interface::transfer_checked(
                    CpiContext::new(
                        ctx.accounts.token_program.to_account_info(),
                        TransferChecked {
                            from: ctx.accounts.payer_token_account.to_account_info(),
                            mint: ctx.accounts.mint.to_account_info(),
                            to: treasury_ata.to_account_info(),
                            authority: ctx.accounts.payer.to_account_info(),
                        },
                    ),
                    quote.fee,
                    ctx.accounts.mint.decimals,
                )?;
                emit!(ProtocolFeeCharged {
                    payer: ctx.accounts.payer.key(),
                    treasury: quote.treasury,
                    fee: quote.fee,
                    timestamp: now,
                });
            }
        }

        emit!(PaymentRequestFulfilled {
            request: request.key(),
            payer: ctx.accounts.payer.key(),
            amount,
            timestamp: now,
        });
        Ok(())
    }

    /// Requester only. Marks an open request as cancelled so it can no longer
    /// be paid.
    pub fn cancel_request(ctx: Context<CancelRequest>) -> Result<()> {
        let request = &mut ctx.accounts.request;
        require!(request.status == RequestStatus::Open, VeiledError::RequestNotOpen);
        request.status = RequestStatus::Cancelled;
        emit!(PaymentRequestCancelled {
            request: request.key(),
            timestamp: Clock::get()?.unix_timestamp,
        });
        Ok(())
    }

    /// Requester only. Deletes a request account and refunds its rent. Paid or
    /// cancelled requests can be closed whenever the requester likes. An open
    /// request has to reach its expiry first, so nobody paying from a valid
    /// link finds the record gone halfway through.
    pub fn close_request(ctx: Context<CloseRequest>) -> Result<()> {
        let request = &ctx.accounts.request;
        let now = Clock::get()?.unix_timestamp;
        require!(
            request.status != RequestStatus::Open || now >= request.expires_at,
            VeiledError::RequestStillOpen
        );
        emit!(PaymentRequestClosed {
            request: request.key(),
            timestamp: now,
        });
        Ok(())
    }

    // ---- Disclosure receipts ----

    /// Writes a receipt saying the signer revealed details of a confidential
    /// transfer to a particular viewer, identified only by `viewer_hash`. The
    /// proof is never stored here. The receipt holds the transaction
    /// reference, a commitment to what was disclosed and the time it was filed.
    pub fn file_disclosure(
        ctx: Context<FileDisclosure>,
        tx_reference: String,
        viewer_hash: [u8; 32],
        disclosure_commitment: [u8; 32],
    ) -> Result<()> {
        require!(
            !tx_reference.is_empty() && tx_reference.len() <= MAX_TX_REFERENCE_LEN,
            VeiledError::InvalidTxReferenceLength
        );
        let now = Clock::get()?.unix_timestamp;

        let receipt = &mut ctx.accounts.receipt;
        receipt.profile = ctx.accounts.owner.key();
        receipt.tx_reference = tx_reference.clone();
        receipt.viewer_hash = viewer_hash;
        receipt.disclosure_commitment = disclosure_commitment;
        receipt.index = ctx.accounts.profile.disclosure_count;
        receipt.filed_at = now;
        receipt.bump = ctx.bumps.receipt;

        let profile = &mut ctx.accounts.profile;
        profile.disclosure_count += 1;

        emit!(DisclosureFiled {
            receipt: receipt.key(),
            profile: receipt.profile,
            tx_reference,
            timestamp: now,
        });
        Ok(())
    }
}

// ---- Helpers shared by the instructions above ----

/// Checks a proposed set of agent limits before it is stored.
fn validate_limits(
    per_tx_limit: u64,
    daily_limit: u64,
    hitl_threshold: u64,
    allowlist_enabled: bool,
    allowlist_len: usize,
) -> Result<()> {
    require!(hitl_threshold > 0, VeiledError::InvalidHitlThreshold);
    require!(per_tx_limit > 0 && daily_limit > 0, VeiledError::InvalidSpendAmount);
    require!(per_tx_limit <= daily_limit, VeiledError::PerTxLimitExceedsDailyLimit);
    require!(
        allowlist_len <= MAX_ALLOWED_RECIPIENTS,
        VeiledError::TooManyAllowedRecipients
    );
    // An allowlist that is switched on but empty blocks every payment, which
    // is easy to set by accident and hard to spot. Pausing the agent is the
    // explicit way to stop it spending.
    require!(
        !allowlist_enabled || allowlist_len > 0,
        VeiledError::EmptyAllowlist
    );
    Ok(())
}

/// Passes when the allowlist is off or contains `recipient`.
fn check_allowlist(agent: &Agent, recipient: &Pubkey) -> Result<()> {
    if !agent.allowlist_enabled {
        return Ok(());
    }
    require!(
        agent.allowed_recipients.contains(recipient),
        VeiledError::RecipientNotAllowed
    );
    Ok(())
}

/// Starts a new daily window, with the spent total back at zero, once
/// `WINDOW_SECONDS` have gone by since the current one began. Doing this
/// lazily inside each spend means nobody has to send a reset transaction.
fn roll_window(agent: &mut Agent, now: i64) {
    if now - agent.window_start >= WINDOW_SECONDS {
        agent.window_start = now;
        agent.window_spent = 0;
    }
}

/// The single place where tokens leave an agent vault. The agent PDA signs
/// for the main transfer and, if the quote carries a nonzero fee, a second
/// transfer to the treasury token account. Keeping both in one function means
/// payments, approvals, withdrawals and revocations all use identical signer
/// seeds and fee handling.
#[allow(clippy::too_many_arguments)]
fn settle_from_vault<'info>(
    token_program: &Interface<'info, TokenInterface>,
    vault: &InterfaceAccount<'info, TokenAccount>,
    mint: &InterfaceAccount<'info, Mint>,
    recipient_token_account: &InterfaceAccount<'info, TokenAccount>,
    treasury_token_account: &Option<InterfaceAccount<'info, TokenAccount>>,
    agent: &Account<'info, Agent>,
    amount: u64,
    quote: Option<FeeQuote>,
) -> Result<()> {
    let seeds: &[&[u8]] = &[
        AGENT_SEED,
        agent.owner.as_ref(),
        &agent.index.to_le_bytes(),
        &[agent.bump],
    ];
    token_interface::transfer_checked(
        CpiContext::new_with_signer(
            token_program.to_account_info(),
            TransferChecked {
                from: vault.to_account_info(),
                mint: mint.to_account_info(),
                to: recipient_token_account.to_account_info(),
                authority: agent.to_account_info(),
            },
            &[seeds],
        ),
        amount,
        mint.decimals,
    )?;

    if let Some(quote) = quote {
        if quote.fee > 0 {
            let treasury_ata = treasury_token_account
                .as_ref()
                .ok_or(error!(VeiledError::MissingFeeAccounts))?;
            require_keys_eq!(
                treasury_ata.owner,
                quote.treasury,
                VeiledError::InvalidFeeAccount
            );
            token_interface::transfer_checked(
                CpiContext::new_with_signer(
                    token_program.to_account_info(),
                    TransferChecked {
                        from: vault.to_account_info(),
                        mint: mint.to_account_info(),
                        to: treasury_ata.to_account_info(),
                        authority: agent.to_account_info(),
                    },
                    &[seeds],
                ),
                quote.fee,
                mint.decimals,
            )?;
            emit!(ProtocolFeeCharged {
                payer: agent.owner,
                treasury: quote.treasury,
                fee: quote.fee,
                timestamp: Clock::get()?.unix_timestamp,
            });
        }
    }
    Ok(())
}

// ---- Account lists: admin and config ----

#[derive(Accounts)]
pub struct InitConfig<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,
    #[account(
        init,
        payer = authority,
        space = 8 + ProtocolConfig::INIT_SPACE,
        seeds = [CONFIG_SEED],
        bump
    )]
    pub config: Account<'info, ProtocolConfig>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct AdminConfig<'info> {
    pub authority: Signer<'info>,
    #[account(
        mut,
        seeds = [CONFIG_SEED],
        bump = config.bump,
        constraint = config.authority == authority.key() @ VeiledError::Unauthorized
    )]
    pub config: Account<'info, ProtocolConfig>,
}

#[derive(Accounts)]
pub struct AcceptAuthority<'info> {
    pub pending_authority: Signer<'info>,
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, ProtocolConfig>,
}

// ---- Account lists: pricing and staking ----

#[derive(Accounts)]
pub struct InitFees<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,
    #[account(
        seeds = [CONFIG_SEED],
        bump = config.bump,
        constraint = config.authority == authority.key() @ VeiledError::Unauthorized
    )]
    pub config: Account<'info, ProtocolConfig>,
    #[account(
        init,
        payer = authority,
        space = 8 + FeeConfig::INIT_SPACE,
        seeds = [FEES_SEED],
        bump
    )]
    pub fee_config: Account<'info, FeeConfig>,
    pub veiled_mint: InterfaceAccount<'info, Mint>,
    #[account(
        init,
        payer = authority,
        seeds = [STAKE_VAULT_SEED],
        bump,
        token::mint = veiled_mint,
        token::authority = fee_config,
        token::token_program = token_program
    )]
    pub stake_vault: InterfaceAccount<'info, TokenAccount>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct AdminFees<'info> {
    pub authority: Signer<'info>,
    #[account(
        mut,
        seeds = [FEES_SEED],
        bump = fee_config.bump,
        constraint = fee_config.authority == authority.key() @ VeiledError::Unauthorized
    )]
    pub fee_config: Account<'info, FeeConfig>,
}

#[derive(Accounts)]
pub struct AcceptFeeAuthority<'info> {
    pub pending_authority: Signer<'info>,
    #[account(mut, seeds = [FEES_SEED], bump = fee_config.bump)]
    pub fee_config: Account<'info, FeeConfig>,
}

#[derive(Accounts)]
pub struct StakeTokens<'info> {
    #[account(mut)]
    pub staker: Signer<'info>,
    #[account(mut, seeds = [FEES_SEED], bump = fee_config.bump)]
    pub fee_config: Account<'info, FeeConfig>,
    #[account(
        init_if_needed,
        payer = staker,
        space = 8 + StakePosition::INIT_SPACE,
        seeds = [STAKE_SEED, staker.key().as_ref()],
        bump
    )]
    pub stake_position: Account<'info, StakePosition>,
    #[account(
        constraint = veiled_mint.key() == fee_config.veiled_mint @ VeiledError::InvalidFeeAccount
    )]
    pub veiled_mint: InterfaceAccount<'info, Mint>,
    #[account(
        mut,
        constraint = staker_token_account.mint == fee_config.veiled_mint @ VeiledError::InvalidFeeAccount
    )]
    pub staker_token_account: InterfaceAccount<'info, TokenAccount>,
    #[account(mut, seeds = [STAKE_VAULT_SEED], bump)]
    pub stake_vault: InterfaceAccount<'info, TokenAccount>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct UnstakeTokens<'info> {
    pub staker: Signer<'info>,
    #[account(mut, seeds = [FEES_SEED], bump = fee_config.bump)]
    pub fee_config: Account<'info, FeeConfig>,
    #[account(
        mut,
        seeds = [STAKE_SEED, staker.key().as_ref()],
        bump = stake_position.bump,
        constraint = stake_position.owner == staker.key() @ VeiledError::Unauthorized
    )]
    pub stake_position: Account<'info, StakePosition>,
    #[account(
        constraint = veiled_mint.key() == fee_config.veiled_mint @ VeiledError::InvalidFeeAccount
    )]
    pub veiled_mint: InterfaceAccount<'info, Mint>,
    #[account(
        mut,
        constraint = staker_token_account.mint == fee_config.veiled_mint @ VeiledError::InvalidFeeAccount
    )]
    pub staker_token_account: InterfaceAccount<'info, TokenAccount>,
    #[account(mut, seeds = [STAKE_VAULT_SEED], bump)]
    pub stake_vault: InterfaceAccount<'info, TokenAccount>,
    pub token_program: Interface<'info, TokenInterface>,
}

// ---- Account lists: profiles ----

#[derive(Accounts)]
#[instruction(handle: String)]
pub struct CreateProfile<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    #[account(
        init,
        payer = owner,
        space = 8 + Profile::INIT_SPACE,
        seeds = [PROFILE_SEED, owner.key().as_ref()],
        bump
    )]
    pub profile: Account<'info, Profile>,
    /// One account per handle, keyed by the handle text. Because `init` fails
    /// when the account is already there, this enforces unique handles.
    #[account(
        init,
        payer = owner,
        space = 8 + HandleClaim::INIT_SPACE,
        seeds = [HANDLE_SEED, handle.as_bytes()],
        bump
    )]
    pub handle_claim: Account<'info, HandleClaim>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(new_handle: String)]
pub struct ChangeHandle<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    #[account(
        mut,
        seeds = [PROFILE_SEED, owner.key().as_ref()],
        bump = profile.bump,
        constraint = profile.owner == owner.key() @ VeiledError::Unauthorized,
        // Checked here rather than in the handler because the claim accounts
        // below are validated first, and renaming to the same handle would
        // otherwise surface as an unhelpful "already in use" init failure.
        constraint = profile.handle != new_handle @ VeiledError::SameHandle
    )]
    pub profile: Account<'info, Profile>,
    /// Claim account for the handle being given up. It is closed and its rent
    /// returned to the owner.
    #[account(
        mut,
        close = owner,
        seeds = [HANDLE_SEED, profile.handle.as_bytes()],
        bump = old_handle_claim.bump,
        constraint = old_handle_claim.owner == owner.key() @ VeiledError::Unauthorized
    )]
    pub old_handle_claim: Account<'info, HandleClaim>,
    #[account(
        init,
        payer = owner,
        space = 8 + HandleClaim::INIT_SPACE,
        seeds = [HANDLE_SEED, new_handle.as_bytes()],
        bump
    )]
    pub new_handle_claim: Account<'info, HandleClaim>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct SetKycTier<'info> {
    pub compliance_authority: Signer<'info>,
    #[account(
        seeds = [CONFIG_SEED],
        bump = config.bump,
        constraint = config.compliance_authority == compliance_authority.key()
            @ VeiledError::UnauthorizedComplianceAuthority
    )]
    pub config: Account<'info, ProtocolConfig>,
    #[account(
        mut,
        seeds = [PROFILE_SEED, profile.owner.as_ref()],
        bump = profile.bump
    )]
    pub profile: Account<'info, Profile>,
}

// ---- Account lists: agents ----

#[derive(Accounts)]
pub struct CreateAgent<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    #[account(
        mut,
        seeds = [PROFILE_SEED, owner.key().as_ref()],
        bump = profile.bump
    )]
    pub profile: Account<'info, Profile>,
    #[account(
        init,
        payer = owner,
        space = 8 + Agent::INIT_SPACE,
        seeds = [AGENT_SEED, owner.key().as_ref(), &profile.agent_count.to_le_bytes()],
        bump
    )]
    pub agent: Account<'info, Agent>,
    /// The only mint this agent can spend, normally USDG.
    pub mint: InterfaceAccount<'info, Mint>,
    /// Token account owned by the agent PDA. Funds can only leave through
    /// this program's checks, so a stolen agent key cannot spend beyond the
    /// agent's limits.
    #[account(
        init,
        payer = owner,
        seeds = [VAULT_SEED, agent.key().as_ref()],
        bump,
        token::mint = mint,
        token::authority = agent,
        token::token_program = token_program
    )]
    pub vault: InterfaceAccount<'info, TokenAccount>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct OwnerAgent<'info> {
    pub owner: Signer<'info>,
    #[account(
        mut,
        seeds = [AGENT_SEED, owner.key().as_ref(), &agent.index.to_le_bytes()],
        bump = agent.bump,
        constraint = agent.owner == owner.key() @ VeiledError::UnauthorizedAgentOwner
    )]
    pub agent: Account<'info, Agent>,
}

#[derive(Accounts)]
pub struct RevokeAgent<'info> {
    pub owner: Signer<'info>,
    #[account(
        mut,
        seeds = [AGENT_SEED, owner.key().as_ref(), &agent.index.to_le_bytes()],
        bump = agent.bump,
        constraint = agent.owner == owner.key() @ VeiledError::UnauthorizedAgentOwner
    )]
    pub agent: Account<'info, Agent>,
    #[account(
        mut,
        seeds = [VAULT_SEED, agent.key().as_ref()],
        bump = agent.vault_bump
    )]
    pub vault: InterfaceAccount<'info, TokenAccount>,
    #[account(constraint = mint.key() == agent.mint @ VeiledError::InvalidMint)]
    pub mint: InterfaceAccount<'info, Mint>,
    #[account(
        mut,
        constraint = owner_token_account.owner == owner.key() @ VeiledError::Unauthorized,
        constraint = owner_token_account.mint == agent.mint @ VeiledError::InvalidMint
    )]
    pub owner_token_account: InterfaceAccount<'info, TokenAccount>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct FundAgent<'info> {
    pub funder: Signer<'info>,
    #[account(
        seeds = [AGENT_SEED, agent.owner.as_ref(), &agent.index.to_le_bytes()],
        bump = agent.bump
    )]
    pub agent: Account<'info, Agent>,
    #[account(
        mut,
        seeds = [VAULT_SEED, agent.key().as_ref()],
        bump = agent.vault_bump
    )]
    pub vault: InterfaceAccount<'info, TokenAccount>,
    #[account(constraint = mint.key() == agent.mint @ VeiledError::InvalidMint)]
    pub mint: InterfaceAccount<'info, Mint>,
    #[account(
        mut,
        constraint = funder_token_account.owner == funder.key() @ VeiledError::Unauthorized,
        constraint = funder_token_account.mint == agent.mint @ VeiledError::InvalidMint
    )]
    pub funder_token_account: InterfaceAccount<'info, TokenAccount>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct WithdrawFromAgent<'info> {
    pub owner: Signer<'info>,
    #[account(
        seeds = [AGENT_SEED, owner.key().as_ref(), &agent.index.to_le_bytes()],
        bump = agent.bump,
        constraint = agent.owner == owner.key() @ VeiledError::UnauthorizedAgentOwner
    )]
    pub agent: Account<'info, Agent>,
    #[account(
        mut,
        seeds = [VAULT_SEED, agent.key().as_ref()],
        bump = agent.vault_bump
    )]
    pub vault: InterfaceAccount<'info, TokenAccount>,
    #[account(constraint = mint.key() == agent.mint @ VeiledError::InvalidMint)]
    pub mint: InterfaceAccount<'info, Mint>,
    #[account(
        mut,
        constraint = owner_token_account.owner == owner.key() @ VeiledError::Unauthorized,
        constraint = owner_token_account.mint == agent.mint @ VeiledError::InvalidMint
    )]
    pub owner_token_account: InterfaceAccount<'info, TokenAccount>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct PayInvoice<'info> {
    pub agent_signer: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, ProtocolConfig>,
    #[account(
        mut,
        seeds = [AGENT_SEED, agent.owner.as_ref(), &agent.index.to_le_bytes()],
        bump = agent.bump,
        constraint = agent.agent_signer == agent_signer.key() @ VeiledError::UnauthorizedAgentSigner
    )]
    pub agent: Account<'info, Agent>,
    #[account(
        mut,
        seeds = [VAULT_SEED, agent.key().as_ref()],
        bump = agent.vault_bump
    )]
    pub vault: InterfaceAccount<'info, TokenAccount>,
    #[account(constraint = mint.key() == agent.mint @ VeiledError::InvalidMint)]
    pub mint: InterfaceAccount<'info, Mint>,
    /// CHECK: Wallet being paid. The handler tests it against the allowlist,
    /// and `recipient_token_account` must belong to it.
    pub recipient: UncheckedAccount<'info>,
    #[account(
        mut,
        constraint = recipient_token_account.owner == recipient.key() @ VeiledError::Unauthorized,
        constraint = recipient_token_account.mint == agent.mint @ VeiledError::InvalidMint
    )]
    pub recipient_token_account: InterfaceAccount<'info, TokenAccount>,
    #[account(seeds = [FEES_SEED], bump = fee_config.bump)]
    pub fee_config: Option<Account<'info, FeeConfig>>,
    pub owner_veiled_token_account: Option<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        seeds = [STAKE_SEED, agent.owner.as_ref()],
        bump = owner_stake_position.bump
    )]
    pub owner_stake_position: Option<Account<'info, StakePosition>>,
    #[account(mut)]
    pub treasury_token_account: Option<InterfaceAccount<'info, TokenAccount>>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct QueueInvoice<'info> {
    #[account(mut)]
    pub agent_signer: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, ProtocolConfig>,
    #[account(
        mut,
        seeds = [AGENT_SEED, agent.owner.as_ref(), &agent.index.to_le_bytes()],
        bump = agent.bump,
        constraint = agent.agent_signer == agent_signer.key() @ VeiledError::UnauthorizedAgentSigner
    )]
    pub agent: Account<'info, Agent>,
    /// CHECK: Wallet that would be paid. Tested against the allowlist and
    /// saved on the queue record so the owner can see it before approving.
    pub recipient: UncheckedAccount<'info>,
    #[account(
        init,
        payer = agent_signer,
        space = 8 + PendingApproval::INIT_SPACE,
        seeds = [PENDING_SEED, agent.key().as_ref(), &agent.pending_count.to_le_bytes()],
        bump
    )]
    pub pending: Account<'info, PendingApproval>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct ApprovePending<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, ProtocolConfig>>,
    #[account(
        mut,
        seeds = [AGENT_SEED, owner.key().as_ref(), &agent.index.to_le_bytes()],
        bump = agent.bump,
        constraint = agent.owner == owner.key() @ VeiledError::UnauthorizedAgentOwner
    )]
    pub agent: Box<Account<'info, Agent>>,
    #[account(
        mut,
        close = owner,
        seeds = [PENDING_SEED, agent.key().as_ref(), &pending.index.to_le_bytes()],
        bump = pending.bump,
        constraint = pending.agent == agent.key() @ VeiledError::Unauthorized
    )]
    pub pending: Box<Account<'info, PendingApproval>>,
    #[account(
        mut,
        seeds = [VAULT_SEED, agent.key().as_ref()],
        bump = agent.vault_bump
    )]
    pub vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(constraint = mint.key() == agent.mint @ VeiledError::InvalidMint)]
    pub mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(
        mut,
        constraint = recipient_token_account.owner == pending.recipient @ VeiledError::Unauthorized,
        constraint = recipient_token_account.mint == agent.mint @ VeiledError::InvalidMint
    )]
    pub recipient_token_account: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(seeds = [FEES_SEED], bump = fee_config.bump)]
    pub fee_config: Option<Account<'info, FeeConfig>>,
    pub owner_veiled_token_account: Option<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        seeds = [STAKE_SEED, agent.owner.as_ref()],
        bump = owner_stake_position.bump
    )]
    pub owner_stake_position: Option<Account<'info, StakePosition>>,
    #[account(mut)]
    pub treasury_token_account: Option<InterfaceAccount<'info, TokenAccount>>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct RejectPending<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    #[account(
        seeds = [AGENT_SEED, owner.key().as_ref(), &agent.index.to_le_bytes()],
        bump = agent.bump,
        constraint = agent.owner == owner.key() @ VeiledError::UnauthorizedAgentOwner
    )]
    pub agent: Account<'info, Agent>,
    #[account(
        mut,
        close = owner,
        seeds = [PENDING_SEED, agent.key().as_ref(), &pending.index.to_le_bytes()],
        bump = pending.bump,
        constraint = pending.agent == agent.key() @ VeiledError::Unauthorized
    )]
    pub pending: Account<'info, PendingApproval>,
}

#[derive(Accounts)]
pub struct CancelPending<'info> {
    #[account(mut)]
    pub agent_signer: Signer<'info>,
    #[account(
        seeds = [AGENT_SEED, agent.owner.as_ref(), &agent.index.to_le_bytes()],
        bump = agent.bump,
        constraint = agent.agent_signer == agent_signer.key() @ VeiledError::UnauthorizedAgentSigner
    )]
    pub agent: Account<'info, Agent>,
    #[account(
        mut,
        close = agent_signer,
        seeds = [PENDING_SEED, agent.key().as_ref(), &pending.index.to_le_bytes()],
        bump = pending.bump,
        constraint = pending.agent == agent.key() @ VeiledError::Unauthorized
    )]
    pub pending: Account<'info, PendingApproval>,
}

// ---- Account lists: request-to-pay ----

#[derive(Accounts)]
pub struct CreateRequest<'info> {
    #[account(mut)]
    pub requester: Signer<'info>,
    #[account(
        mut,
        seeds = [PROFILE_SEED, requester.key().as_ref()],
        bump = profile.bump
    )]
    pub profile: Account<'info, Profile>,
    #[account(
        init,
        payer = requester,
        space = 8 + PaymentRequest::INIT_SPACE,
        seeds = [REQUEST_SEED, requester.key().as_ref(), &profile.request_count.to_le_bytes()],
        bump
    )]
    pub request: Account<'info, PaymentRequest>,
    /// CHECK: Wallet that gets paid when the request is fulfilled. It can
    /// differ from the requester, for example a company treasury.
    pub receiver: UncheckedAccount<'info>,
    pub mint: InterfaceAccount<'info, Mint>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct FulfillRequest<'info> {
    pub payer: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, ProtocolConfig>,
    #[account(
        mut,
        seeds = [REQUEST_SEED, request.requester.as_ref(), &request.index.to_le_bytes()],
        bump = request.bump
    )]
    pub request: Account<'info, PaymentRequest>,
    #[account(constraint = mint.key() == request.mint @ VeiledError::InvalidMint)]
    pub mint: InterfaceAccount<'info, Mint>,
    #[account(
        mut,
        constraint = payer_token_account.owner == payer.key() @ VeiledError::Unauthorized,
        constraint = payer_token_account.mint == request.mint @ VeiledError::InvalidMint
    )]
    pub payer_token_account: InterfaceAccount<'info, TokenAccount>,
    #[account(
        mut,
        constraint = receiver_token_account.owner == request.receiver @ VeiledError::Unauthorized,
        constraint = receiver_token_account.mint == request.mint @ VeiledError::InvalidMint
    )]
    pub receiver_token_account: InterfaceAccount<'info, TokenAccount>,
    #[account(seeds = [FEES_SEED], bump = fee_config.bump)]
    pub fee_config: Option<Account<'info, FeeConfig>>,
    pub payer_veiled_token_account: Option<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        seeds = [STAKE_SEED, payer.key().as_ref()],
        bump = payer_stake_position.bump
    )]
    pub payer_stake_position: Option<Account<'info, StakePosition>>,
    #[account(mut)]
    pub treasury_token_account: Option<InterfaceAccount<'info, TokenAccount>>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct CancelRequest<'info> {
    pub requester: Signer<'info>,
    #[account(
        mut,
        seeds = [REQUEST_SEED, requester.key().as_ref(), &request.index.to_le_bytes()],
        bump = request.bump,
        constraint = request.requester == requester.key() @ VeiledError::Unauthorized
    )]
    pub request: Account<'info, PaymentRequest>,
}

#[derive(Accounts)]
pub struct CloseRequest<'info> {
    #[account(mut)]
    pub requester: Signer<'info>,
    #[account(
        mut,
        close = requester,
        seeds = [REQUEST_SEED, requester.key().as_ref(), &request.index.to_le_bytes()],
        bump = request.bump,
        constraint = request.requester == requester.key() @ VeiledError::Unauthorized
    )]
    pub request: Account<'info, PaymentRequest>,
}

// ---- Account lists: disclosure receipts ----

#[derive(Accounts)]
pub struct FileDisclosure<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    #[account(
        mut,
        seeds = [PROFILE_SEED, owner.key().as_ref()],
        bump = profile.bump
    )]
    pub profile: Account<'info, Profile>,
    #[account(
        init,
        payer = owner,
        space = 8 + DisclosureReceipt::INIT_SPACE,
        seeds = [DISCLOSURE_SEED, owner.key().as_ref(), &profile.disclosure_count.to_le_bytes()],
        bump
    )]
    pub receipt: Account<'info, DisclosureReceipt>,
    pub system_program: Program<'info, System>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handle_rules() {
        assert!(validate_handle("john").is_ok());
        assert!(validate_handle("gwen_stacy_99").is_ok());
        assert!(validate_handle(&"a".repeat(MAX_HANDLE_LEN)).is_ok());

        assert!(validate_handle("").is_err());
        assert!(validate_handle(&"a".repeat(MAX_HANDLE_LEN + 1)).is_err());
        assert!(validate_handle("John").is_err());
        assert!(validate_handle("john.veiled").is_err());
        assert!(validate_handle("j\u{f6}hn").is_err());
    }

    #[test]
    fn limit_rules() {
        assert!(validate_limits(100, 1_000, 50, false, 0).is_ok());
        assert!(validate_limits(1_000, 1_000, 50, true, 1).is_ok());

        // Zero values.
        assert!(validate_limits(100, 1_000, 0, false, 0).is_err());
        assert!(validate_limits(0, 1_000, 50, false, 0).is_err());
        assert!(validate_limits(100, 0, 50, false, 0).is_err());
        // Per-transaction limit above the daily one.
        assert!(validate_limits(1_001, 1_000, 50, false, 0).is_err());
        // Allowlist too long, or switched on while empty.
        assert!(validate_limits(100, 1_000, 50, true, MAX_ALLOWED_RECIPIENTS + 1).is_err());
        assert!(validate_limits(100, 1_000, 50, true, 0).is_err());
    }

    fn test_agent(window_start: i64, window_spent: u64, allowlist: Vec<Pubkey>) -> Agent {
        Agent {
            owner: Pubkey::default(),
            agent_signer: Pubkey::default(),
            label: "test".to_string(),
            autonomy_tier: AutonomyTier::FullyAutonomous,
            status: AgentStatus::Active,
            mint: Pubkey::default(),
            per_tx_limit: 100,
            daily_limit: 1_000,
            hitl_threshold: 50,
            window_start,
            window_spent,
            allowlist_enabled: !allowlist.is_empty(),
            allowed_recipients: allowlist,
            pending_count: 0,
            index: 0,
            created_at: 0,
            updated_at: 0,
            bump: 0,
            vault_bump: 0,
        }
    }

    #[test]
    fn window_rolls_only_after_a_full_day() {
        let mut agent = test_agent(1_000, 400, vec![]);
        roll_window(&mut agent, 1_000 + WINDOW_SECONDS - 1);
        assert_eq!(agent.window_spent, 400);
        assert_eq!(agent.window_start, 1_000);

        let later = 1_000 + WINDOW_SECONDS;
        roll_window(&mut agent, later);
        assert_eq!(agent.window_spent, 0);
        assert_eq!(agent.window_start, later);
    }

    #[test]
    fn allowlist_checks() {
        let allowed = Pubkey::new_unique();
        let stranger = Pubkey::new_unique();

        let open = test_agent(0, 0, vec![]);
        assert!(check_allowlist(&open, &stranger).is_ok());

        let locked = test_agent(0, 0, vec![allowed]);
        assert!(check_allowlist(&locked, &allowed).is_ok());
        assert!(check_allowlist(&locked, &stranger).is_err());
    }
}
