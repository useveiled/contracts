use anchor_lang::prelude::*;
use anchor_spl::token_interface::TokenAccount;

use crate::codes::VeiledError;
use crate::model::{FeeConfig, ProtocolConfig, StakePosition, BPS};

/// Result of pricing one payment: the fee in token base units and the
/// wallet that should receive it.
pub struct FeeQuote {
    pub fee: u64,
    pub treasury: Pubkey,
}

/// Prices the protocol fee for a payment of `amount`. Returns `None` while
/// fees are switched off, and fails with `MissingFeeAccounts` if they are on
/// but no fee config was passed. Every instruction that charges a fee goes
/// through here, so on-chain pricing has a single source of truth for
/// clients to mirror.
///
/// With every ratio in basis points (10_000 = 100%):
///
///   weight   = staked + held * held_weight_bps / 10_000
///   discount = discount_bps of the last tier with min_weight <= weight
///   gross    = amount * base_fee_bps / 10_000
///   net      = gross * (10_000 - discount) / 10_000
///   fee      = min(net, fee_cap), or just net when fee_cap is 0
///
/// The $VEILED token account and stake position are optional. Leaving one
/// out only loses the discount it would earn, but passing one that belongs
/// to someone else, or the wrong mint, is rejected so a payer cannot borrow
/// another wallet's weight. Intermediate values are u128 so the products
/// cannot overflow, and every division rounds down in the payer's favor.
pub fn quote_fee(
    config: &ProtocolConfig,
    fee_config: &Option<Account<FeeConfig>>,
    payer: &Pubkey,
    payer_veiled_ata: &Option<InterfaceAccount<TokenAccount>>,
    payer_stake: &Option<Account<StakePosition>>,
    amount: u64,
) -> Result<Option<FeeQuote>> {
    if !config.fees_enabled {
        return Ok(None);
    }
    let fee_config = fee_config
        .as_ref()
        .ok_or(error!(VeiledError::MissingFeeAccounts))?;

    let mut held: u64 = 0;
    if let Some(ata) = payer_veiled_ata {
        require_keys_eq!(ata.mint, fee_config.veiled_mint, VeiledError::InvalidFeeAccount);
        require_keys_eq!(ata.owner, *payer, VeiledError::InvalidFeeAccount);
        held = ata.amount;
    }
    let mut staked: u64 = 0;
    if let Some(position) = payer_stake {
        require_keys_eq!(position.owner, *payer, VeiledError::InvalidFeeAccount);
        staked = position.amount;
    }

    Ok(Some(FeeQuote {
        fee: compute_fee(fee_config, amount, staked, held),
        treasury: config.treasury,
    }))
}

/// The pure part of `quote_fee`: turns an amount and a wallet's staked and
/// held $VEILED into a fee, following the formula documented above. Split
/// out so the math can be tested without building any accounts.
pub fn compute_fee(fee_config: &FeeConfig, amount: u64, staked: u64, held: u64) -> u64 {
    // Staked tokens count fully, held ones at the configured fraction.
    let weight = staked as u128 + held as u128 * fee_config.held_weight_bps as u128 / BPS as u128;

    let mut discount_bps: u128 = 0;
    for tier in fee_config.tiers.iter() {
        if weight >= tier.min_weight as u128 {
            discount_bps = tier.discount_bps as u128;
        } else {
            // Sorted ascending, so once one tier is out of reach the rest are too.
            break;
        }
    }

    let gross = amount as u128 * fee_config.base_fee_bps as u128 / BPS as u128;
    let net = gross * (BPS as u128 - discount_bps) / BPS as u128;
    let capped = if fee_config.fee_cap != 0 && net > fee_config.fee_cap as u128 {
        fee_config.fee_cap as u128
    } else {
        net
    };
    // gross is at most amount, so this always fits back into a u64.
    capped as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::FeeTier;

    const UNIT: u64 = 1_000_000_000; // $VEILED has 9 decimals

    /// Launch pricing, matching what `init_fees` writes.
    fn launch_config() -> FeeConfig {
        FeeConfig {
            authority: Pubkey::default(),
            pending_authority: None,
            veiled_mint: Pubkey::default(),
            base_fee_bps: 10,
            fee_cap: 5_000_000,
            held_weight_bps: 5_000,
            tiers: vec![
                FeeTier { min_weight: 1_000 * UNIT, discount_bps: 1_000 },
                FeeTier { min_weight: 10_000 * UNIT, discount_bps: 2_500 },
                FeeTier { min_weight: 100_000 * UNIT, discount_bps: 5_000 },
                FeeTier { min_weight: 1_000_000 * UNIT, discount_bps: 7_500 },
            ],
            total_staked: 0,
            bump: 0,
        }
    }

    #[test]
    fn charges_base_rate_with_no_veiled() {
        // 100 USDG at 10 bps is 0.10 USDG.
        assert_eq!(compute_fee(&launch_config(), 100_000_000, 0, 0), 100_000);
    }

    #[test]
    fn applies_the_highest_reached_tier() {
        let cfg = launch_config();
        // Exactly on the 10k boundary gets 25% off.
        assert_eq!(compute_fee(&cfg, 100_000_000, 10_000 * UNIT, 0), 75_000);
        // Just below it stays on the 10% tier.
        assert_eq!(compute_fee(&cfg, 100_000_000, 10_000 * UNIT - 1, 0), 90_000);
    }

    #[test]
    fn counts_held_tokens_at_the_reduced_weight() {
        let cfg = launch_config();
        // 2k held counts as 1k, which reaches the first tier.
        assert_eq!(compute_fee(&cfg, 100_000_000, 0, 2_000 * UNIT), 90_000);
        // 1.9k held counts as 950, which does not.
        assert_eq!(compute_fee(&cfg, 100_000_000, 0, 1_900 * UNIT), 100_000);
    }

    #[test]
    fn caps_large_fees() {
        // 1M USDG would be 1,000 USDG in fees before the 5 USDG cap.
        assert_eq!(compute_fee(&launch_config(), 1_000_000_000_000, 0, 0), 5_000_000);
    }

    #[test]
    fn zero_cap_means_uncapped() {
        let mut cfg = launch_config();
        cfg.fee_cap = 0;
        assert_eq!(compute_fee(&cfg, 1_000_000_000_000, 0, 0), 1_000_000_000);
    }

    #[test]
    fn rounds_down_for_tiny_amounts() {
        assert_eq!(compute_fee(&launch_config(), 999, 0, 0), 0);
    }

    #[test]
    fn handles_max_values_without_overflow() {
        let mut cfg = launch_config();
        cfg.base_fee_bps = BPS as u16;
        cfg.fee_cap = 0;
        cfg.tiers.clear();
        assert_eq!(compute_fee(&cfg, u64::MAX, u64::MAX, u64::MAX), u64::MAX);
    }
}
