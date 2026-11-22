# veiled

Solana program for Veiled (useveiled.com), written with Anchor 0.31.

Veiled is a private money account for people and for the AI agents that act on their behalf. Amount privacy is not implemented here. It comes from Token-2022 mints with the confidential transfer extension, where balances and transfer amounts are encrypted and checked by Solana's zero-knowledge proof program. This program adds what an encrypted token balance is missing before it can behave like an account: a name you can send to, spending rules for agents that the chain enforces, a queue where a person signs off on larger agent payments, payment requests, a record of who was shown what, and the $VEILED fee discount.

Wallet addresses on both sides of a payment remain visible on chain. Only the amounts are hidden.

**Program ID** (localnet and devnet): `BgJQoxfRfpQYhJLFxnq7f8GcxPoAce47MGPnq1qJiV7V`

## Contents

- [What the program does](#what-the-program-does)
- [On-chain accounts](#on-chain-accounts)
- [Instruction reference](#instruction-reference)
- [Source layout](#source-layout)
- [Building and testing](#building-and-testing)
- [Deploying](#deploying)
- [Devnet sample data](#devnet-sample-data)

## What the program does

### Names

A wallet registers one profile and picks a handle for it, so people can pay `john.veiled` instead of pasting an address. A handle is 1 to 32 characters drawn from `a-z`, `0-9` and `_`, and it is stored without the `.veiled` ending. Each handle gets its own account whose address is derived from the handle text. That makes two things cheap: a lookup from name to wallet is one account read, and a second wallet trying to register a taken name simply fails when the account already exists. Renaming releases the old handle immediately.

Profiles also carry a KYC tier (`Unverified`, `Basic`, `Enhanced`) and an account kind (`Personal`, `Business`, `AgentOperator`). Only the compliance key named in the protocol config can change the tier. The tier is informational on chain; no instruction moves money because of it.

### Agents

An agent belongs to a profile. It has its own signing key, but that key can only spend from the agent's vault, a token account owned by a program address, and only inside the policy the owner set:

- a maximum per payment,
- a maximum per rolling 24 hour window,
- an optional list of up to 10 permitted recipients,
- a human-in-the-loop (HITL) threshold.

How the threshold is used depends on the autonomy tier. `Supervised` agents cannot pay directly at all and must queue everything. `SemiAutonomous` agents pay directly up to the threshold and must queue anything above it. `FullyAutonomous` agents may pay any amount the limits allow, and may still choose to queue. Queued payments hold no funds. When the owner approves, the limits and allowlist are checked again against their current values, because the policy or the day's spend may have changed in the meantime.

`pay_invoice` and `queue_invoice` accept a 32 byte invoice id, so an x402 payment reference from an HTTP 402 response ends up in the emitted events where a backend can match it.

If an agent key leaks, the owner can pause the agent, swap in a new signer, or revoke it. Revoking is final and sends whatever is left in the vault back to the owner. Revoking and withdrawing from a vault both keep working while the protocol is paused, so owners can always get their money out during an incident.

### Payment requests

A requester creates a request with a mint, an amount, a receiving wallet (which can differ from the requester) and an expiry. The first payer to send the exact amount before expiry settles it. In confidential mode the request stores `keccak256(amount as little-endian u64 || 32 byte blinding)` in place of the amount. The payer gets the amount and blinding off chain, submits both, and the program refuses the payment if they do not hash to the stored value. Pairing this with a Token-2022 confidential transfer keeps the amount hidden end to end; the client handles that transfer.

The requester can cancel an open request. Requests that were paid or cancelled can be closed at any time to recover rent. An open request can only be closed after it expires, so someone paying from a live link never races its deletion.

### Disclosure receipts

When a user reveals the details of a private transfer to an auditor or counterparty, they can file a receipt. It stores the transaction reference (up to 88 characters, usually a signature), a hash identifying the viewer, a commitment to what was disclosed and a timestamp. The actual proof and the viewer's identity stay off chain.

### Fees and $VEILED

No fees are charged until the admin sets a treasury and turns fee routing on. After that, every instruction that moves funds computes the fee in one function (`quote_fee` in `pricing.rs`):

```
weight   = staked + held * held_weight_bps / 10_000
discount = discount_bps of the highest tier with min_weight <= weight
fee      = amount * base_fee_bps / 10_000 * (10_000 - discount) / 10_000
fee      = min(fee, fee_cap)      (a cap of 0 means no cap)
```

`init_fees` sets these starting values:

| Setting | Default |
|---|---|
| Base rate | 10 bps (0.10%) |
| Fee cap | 5,000,000 base units (5 USDG at 6 decimals) |
| Weight of held, unstaked $VEILED | 50% |
| Discount tiers | 10% at 1,000, 25% at 10,000, 50% at 100,000, 75% at 1,000,000 $VEILED |

For agent payments the fee comes out of the vault on top of the payment, and the owner's $VEILED decides the discount. For payment requests the payer covers the fee on top, so the receiver gets the full amount. Staking has no lockup and unstaking is instant.

### Admin keys and the pause switch

The protocol config holds an admin key and a compliance key. The fee config has its own admin key. Both admin roles can be handed over in two steps (nominate, then the nominee accepts), which protects against sending control to a wrong address. The protocol admin can also rotate both protocol keys in a single call with `update_config`, which cancels any handoff in progress.

`set_pause` stops the instructions that pay out (`pay_invoice`, `queue_invoice`, `approve_pending`, `fulfill_request`). Profile, agent configuration and exit paths are unaffected.

## On-chain accounts

Every account lives at a program derived address. Counters are kept per profile or per agent rather than globally, so unrelated users never write to the same account.

| Account type | Seeds | Purpose |
|---|---|---|
| `ProtocolConfig` | `"config"` | Admin and compliance keys, pause flag, treasury, fees on or off |
| `FeeConfig` | `"fees"` | Rate, cap, held weight, tiers, $VEILED mint, total staked |
| `Profile` | `"profile"`, owner | Handle, KYC tier, account kind, agent/request/disclosure counters |
| `HandleClaim` | `"handle"`, handle bytes | Owner of a handle |
| `Agent` | `"agent"`, owner, index (u64 LE) | Signer, tier, status, policy, rolling window |
| Agent vault (token account) | `"vault"`, agent | Funds the agent can spend |
| `PendingApproval` | `"pending"`, agent, index (u64 LE) | One queued payment |
| `PaymentRequest` | `"request"`, requester, index (u64 LE) | Request state, plain or committed amount |
| `DisclosureReceipt` | `"disclosure"`, profile owner, index (u64 LE) | One filed disclosure |
| `StakePosition` | `"stake"`, owner | Staked amount and the time the position opened |
| Stake vault (token account) | `"stake_vault"` | All staked $VEILED, controlled by the fee config |

Tokens move through the Anchor token interface, so both the classic SPL Token program and Token-2022 mints work. Amounts are always in the mint's base units.

## Instruction reference

| Area | Instructions |
|---|---|
| Protocol admin | `init_config`, `update_config`, `begin_authority_transfer`, `accept_authority`, `set_pause`, `set_fee_routing` |
| Fee admin | `init_fees`, `set_fee_schedule`, `set_fee_tiers`, `begin_fee_authority_transfer`, `accept_fee_authority` |
| Staking | `stake`, `unstake` |
| Profiles | `create_profile`, `change_handle`, `set_kyc_tier` |
| Agent setup | `create_agent`, `update_spend_policy`, `set_agent_status`, `rotate_agent_signer`, `revoke_agent` |
| Agent funds | `fund_agent`, `withdraw_from_agent`, `pay_invoice`, `queue_invoice`, `approve_pending`, `reject_pending` |
| Requests | `create_request`, `fulfill_request`, `cancel_request`, `close_request` |
| Disclosure | `file_disclosure` |

The generated IDL is written to `target/idl/veiled.json` by `anchor build`.

## Source layout

```
Anchor.toml                  cluster, program IDs, test script
programs/veiled/
  Cargo.toml
  src/
    lib.rs                   entrypoint, every instruction and its account context
    model.rs                 account structs, enums, seed constants, size limits
    pricing.rs               quote_fee, the single fee calculation
    logs.rs                  events emitted for indexers and the SDK
    codes.rs                 VeiledError, the program's error codes
tests/
  protocol.spec.ts           end to end suite run by `anchor test`
scripts/devnet/
  populate.js                devnet sample data, round one (24 transactions)
  populate-round-two.js      round two (10 transactions)
  populate-round-three.js    round three (3 transactions)
```

## Building and testing

You need the Solana CLI (Agave), Anchor CLI 0.31 and Node 18 or newer.

```bash
npm install
anchor build
```

Run the tests against a throwaway local validator:

```bash
anchor test --provider.cluster localnet
```

`npm test` runs plain `anchor test`, which uses the cluster from `Anchor.toml` (devnet), so pass the localnet override when you want a local run. The suite in `tests/protocol.spec.ts` walks through admin setup and handoff, handle rules and KYC, the agent lifecycle including each policy rejection, plain and confidential requests, cancellation and rent refunds, disclosure receipts, staking, fee discounts, the fee cap and the pause switch.

## Deploying

`Anchor.toml` points at devnet by default and uses `~/.config/solana/id.json` as the wallet. Make sure that wallet has enough devnet SOL to pay for the program account (`solana airdrop 5 --url devnet`, repeated if needed).

```bash
anchor build
anchor deploy --provider.cluster devnet
```

The program keypair is `target/deploy/veiled-keypair.json`. Its public key must match both `declare_id!` in `programs/veiled/src/lib.rs` and the entries in `Anchor.toml`.

A fresh deployment does nothing until it is initialized. Run these in order:

1. `init_config` with the compliance key. The signer becomes the protocol admin. Fees start off and the protocol starts unpaused.
2. `init_fees` with the $VEILED mint. Creates the fee schedule with the defaults above and the stake vault.
3. `set_fee_routing` with a treasury and `fees_enabled = true`, once you actually want to collect fees. Skipping this step leaves the program fee free.

For devnet testing, use your own mints: 6 decimals as a USDG stand-in and 9 decimals for $VEILED. The devnet scripts below create both for you.

## Devnet sample data

`scripts/devnet/` holds three Node scripts that fill a fresh devnet deployment with realistic activity. They use the IDL from `anchor build` and expect the deployer keypair at `deploy-wallet-devnet.json` in this folder. Set `RPC_URL` to use an endpoint other than `https://api.devnet.solana.com`.

```bash
node scripts/devnet/populate.js
node scripts/devnet/populate-round-two.js
DEPLOYER_PUBKEY=<deployer address> node scripts/devnet/populate-round-three.js
```

| Script | Transactions | What it covers |
|---|---|---|
| `populate.js` | 24 | Config and fee setup, two profiles, KYC, a handle change, an agent with direct, approved and rejected payments, policy changes, signer rotation, pause and resume, a withdrawal, staking, turning fees on, a paid request |
| `populate-round-two.js` | 10 | A disclosure receipt, a confidential request, a cancelled request, agent payments that pay fees, a top-up stake, a new fee rate |
| `populate-round-three.js` | 3 | One more fee-paying agent payment, then a request where the payer's stake earns the discount |

Before its first step, round one generates the test wallets and both mock mints and funds them. All three rounds share `scripts/devnet/.devnet-progress.json`, which stores those keys, the mint addresses and the signature of each confirmed step. If a run fails partway, start it again and it continues from the first unconfirmed step. Round three signs with one of the test wallets because it needs no admin rights and does not use the deployer key. Protocol fees in that round go to the deployer's USDG token account, so set `DEPLOYER_PUBKEY` to the deployer's public address. If you leave it unset, the script falls back to an older deployer address and fails with `AccountNotInitialized` on any fresh deployment.

The progress file and the deployer wallet hold private keys and are listed in `.gitignore`. Keep them out of commits.
