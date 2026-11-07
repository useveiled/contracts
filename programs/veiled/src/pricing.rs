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

    let mut held: u128 = 0;
    if let Some(ata) = payer_veiled_ata {
        require_keys_eq!(ata.mint, fee_config.veiled_mint, VeiledError::InvalidFeeAccount);
        require_keys_eq!(ata.owner, *payer, VeiledError::InvalidFeeAccount);
        held = ata.amount as u128;
    }
    let mut staked: u128 = 0;
    if let Some(position) = payer_stake {
        require_keys_eq!(position.owner, *payer, VeiledError::InvalidFeeAccount);
        staked = position.amount as u128;
    }

    // Staked tokens count fully, held ones at the configured fraction.
    let weight = staked + held * fee_config.held_weight_bps as u128 / BPS as u128;

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

    Ok(Some(FeeQuote {
        fee: capped as u64,
        treasury: config.treasury,
    }))
}
