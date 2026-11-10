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
        require!(
            !handle.is_empty() && handle.len() <= MAX_HANDLE_LEN,
            VeiledError::InvalidHandleLength
        );
        require!(is_valid_handle(&handle), VeiledError::InvalidHandleCharacters);
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
        require!(
            !new_handle.is_empty() && new_handle.len() <= MAX_HANDLE_LEN,
            VeiledError::InvalidHandleLength
        );
        require!(is_valid_handle(&new_handle), VeiledError::InvalidHandleCharacters);
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
        constraint = profile.owner == owner.key() @ VeiledError::Unauthorized
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
