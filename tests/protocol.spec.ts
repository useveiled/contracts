import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import {
  Keypair,
  LAMPORTS_PER_SOL,
  PublicKey,
  SystemProgram,
} from "@solana/web3.js";
import {
  TOKEN_PROGRAM_ID,
  createMint,
  createAssociatedTokenAccount,
  getAssociatedTokenAddressSync,
  mintTo,
  getAccount,
} from "@solana/spl-token";
import { keccak_256 } from "js-sha3";
import { assert } from "chai";
import { Veiled } from "../target/types/veiled";

describe("veiled program end to end", () => {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);
  const program = anchor.workspace.Veiled as Program<Veiled>;
  const connection = provider.connection;
  const payer = (provider.wallet as anchor.Wallet).payer;

  // Keypairs for everyone who signs or receives in this suite
  const compliance = Keypair.generate();
  const john = Keypair.generate(); // main user; owns the agent under test
  const felix = Keypair.generate(); // counterparty who pays and gets paid
  const agentSigner = Keypair.generate();
  const newAgentSigner = Keypair.generate();
  const vendor = Keypair.generate();
  const treasury = Keypair.generate();

  // Token mints, created in the before hook
  let usdg: PublicKey; // stablecoin that payments settle in (6 decimals)
  let veiledMint: PublicKey; // $VEILED, used for staking (9 decimals)

  // Singleton program accounts, plus derivation helpers for per-user ones
  const [configPda] = PublicKey.findProgramAddressSync(
    [Buffer.from("config")],
    program.programId
  );
  const [feesPda] = PublicKey.findProgramAddressSync(
    [Buffer.from("fees")],
    program.programId
  );
  const [stakeVaultPda] = PublicKey.findProgramAddressSync(
    [Buffer.from("stake_vault")],
    program.programId
  );

  const profilePda = (owner: PublicKey) =>
    PublicKey.findProgramAddressSync(
      [Buffer.from("profile"), owner.toBuffer()],
      program.programId
    )[0];
  const handlePda = (handle: string) =>
    PublicKey.findProgramAddressSync(
      [Buffer.from("handle"), Buffer.from(handle)],
      program.programId
    )[0];
  const agentPda = (owner: PublicKey, index: number) =>
    PublicKey.findProgramAddressSync(
      [
        Buffer.from("agent"),
        owner.toBuffer(),
        new anchor.BN(index).toArrayLike(Buffer, "le", 8),
      ],
      program.programId
    )[0];
  const vaultPda = (agent: PublicKey) =>
    PublicKey.findProgramAddressSync(
      [Buffer.from("vault"), agent.toBuffer()],
      program.programId
    )[0];
  const pendingPda = (agent: PublicKey, index: number) =>
    PublicKey.findProgramAddressSync(
      [
        Buffer.from("pending"),
        agent.toBuffer(),
        new anchor.BN(index).toArrayLike(Buffer, "le", 8),
      ],
      program.programId
    )[0];
  const requestPda = (requester: PublicKey, index: number) =>
    PublicKey.findProgramAddressSync(
      [
        Buffer.from("request"),
        requester.toBuffer(),
        new anchor.BN(index).toArrayLike(Buffer, "le", 8),
      ],
      program.programId
    )[0];
  const disclosurePda = (owner: PublicKey, index: number) =>
    PublicKey.findProgramAddressSync(
      [
        Buffer.from("disclosure"),
        owner.toBuffer(),
        new anchor.BN(index).toArrayLike(Buffer, "le", 8),
      ],
      program.programId
    )[0];
  const stakePda = (owner: PublicKey) =>
    PublicKey.findProgramAddressSync(
      [Buffer.from("stake"), owner.toBuffer()],
      program.programId
    )[0];

  const USDG = (n: number) => new anchor.BN(n * 1_000_000);
  const VEIL = (n: number) => new anchor.BN(n).mul(new anchor.BN(1_000_000_000));
  const ZERO32 = Array(32).fill(0);

  const expectError = async (p: Promise<unknown>, code: string) => {
    try {
      await p;
      assert.fail(`expected ${code}`);
    } catch (err: any) {
      const got = err?.error?.errorCode?.code ?? err?.message ?? String(err);
      assert.include(String(got), code, `expected ${code}, got: ${got}`);
    }
  };

  before(async () => {
    // Airdrop SOL to every keypair so each can pay rent and fees.
    for (const kp of [compliance, john, felix, agentSigner, newAgentSigner, vendor, treasury]) {
      const sig = await connection.requestAirdrop(kp.publicKey, 10 * LAMPORTS_PER_SOL);
      await connection.confirmTransaction(sig);
    }
    usdg = await createMint(connection, payer, payer.publicKey, null, 6);
    veiledMint = await createMint(connection, payer, payer.publicKey, null, 9);
  });

  // Global config and admin controls

  it("sets up the config account with fees off and the protocol unpaused", async () => {
    await program.methods
      .initConfig(compliance.publicKey)
      .accountsPartial({
        authority: provider.wallet.publicKey,
        config: configPda,
        systemProgram: SystemProgram.programId,
      })
      .rpc();

    const config = await program.account.protocolConfig.fetch(configPda);
    assert.ok(config.authority.equals(provider.wallet.publicKey));
    assert.ok(config.complianceAuthority.equals(compliance.publicKey));
    assert.isFalse(config.paused);
    assert.isFalse(config.feesEnabled);
  });

  it("moves the admin role to a nominee via propose and accept, then returns it", async () => {
    await program.methods
      .beginAuthorityTransfer(felix.publicKey)
      .accountsPartial({ authority: provider.wallet.publicKey, config: configPda })
      .rpc();

    // Only the nominated key may accept; john was never proposed.
    await expectError(
      program.methods
        .acceptAuthority()
        .accountsPartial({ pendingAuthority: john.publicKey, config: configPda })
        .signers([john])
        .rpc(),
      "NotPendingAuthority"
    );

    await program.methods
      .acceptAuthority()
      .accountsPartial({ pendingAuthority: felix.publicKey, config: configPda })
      .signers([felix])
      .rpc();

    let config = await program.account.protocolConfig.fetch(configPda);
    assert.ok(config.authority.equals(felix.publicKey));

    // Later tests sign admin calls with the provider wallet, so restore it now.
    await program.methods
      .updateConfig(provider.wallet.publicKey, compliance.publicKey)
      .accountsPartial({ authority: felix.publicKey, config: configPda })
      .signers([felix])
      .rpc();
    config = await program.account.protocolConfig.fetch(configPda);
    assert.ok(config.authority.equals(provider.wallet.publicKey));
  });

  it("refuses a pause toggle signed by someone other than the admin", async () => {
    await expectError(
      program.methods
        .setPause(true)
        .accountsPartial({ authority: john.publicKey, config: configPda })
        .signers([john])
        .rpc(),
      "Unauthorized"
    );
  });

  // Profiles, handles and KYC tiers

  it("registers a profile for john that starts out unverified", async () => {
    await program.methods
      .createProfile("john", { personal: {} })
      .accountsPartial({
        owner: john.publicKey,
        profile: profilePda(john.publicKey),
        handleClaim: handlePda("john"),
        systemProgram: SystemProgram.programId,
      })
      .signers([john])
      .rpc();

    const profile = await program.account.profile.fetch(profilePda(john.publicKey));
    assert.equal(profile.handle, "john");
    assert.deepEqual(profile.kycTier, { unverified: {} });
  });

  it("will not let a second user claim a handle someone holds", async () => {
    await expectError(
      program.methods
        .createProfile("john", { personal: {} })
        .accountsPartial({
          owner: felix.publicKey,
          profile: profilePda(felix.publicKey),
          handleClaim: handlePda("john"),
          systemProgram: SystemProgram.programId,
        })
        .signers([felix])
        .rpc(),
      "already in use"
    );
  });

  it("refuses handles containing uppercase or punctuation", async () => {
    await expectError(
      program.methods
        .createProfile("Felix!", { personal: {} })
        .accountsPartial({
          owner: felix.publicKey,
          profile: profilePda(felix.publicKey),
          handleClaim: handlePda("Felix!"),
          systemProgram: SystemProgram.programId,
        })
        .signers([felix])
        .rpc(),
      "InvalidHandleCharacters"
    );
  });

  it("renames a handle and frees the previous one for reuse", async () => {
    await program.methods
      .changeHandle("gwen_stacy")
      .accountsPartial({
        owner: john.publicKey,
        profile: profilePda(john.publicKey),
        oldHandleClaim: handlePda("john"),
        newHandleClaim: handlePda("gwen_stacy"),
        systemProgram: SystemProgram.programId,
      })
      .signers([john])
      .rpc();

    const profile = await program.account.profile.fetch(profilePda(john.publicKey));
    assert.equal(profile.handle, "gwen_stacy");

    // Prove the old name was freed by having felix register it.
    await program.methods
      .createProfile("john", { business: {} })
      .accountsPartial({
        owner: felix.publicKey,
        profile: profilePda(felix.publicKey),
        handleClaim: handlePda("john"),
        systemProgram: SystemProgram.programId,
      })
      .signers([felix])
      .rpc();
  });

  it("accepts KYC tier changes only from the compliance key", async () => {
    await expectError(
      program.methods
        .setKycTier({ enhanced: {} })
        .accountsPartial({
          complianceAuthority: john.publicKey,
          config: configPda,
          profile: profilePda(john.publicKey),
        })
        .signers([john])
        .rpc(),
      "UnauthorizedComplianceAuthority"
    );

    await program.methods
      .setKycTier({ enhanced: {} })
      .accountsPartial({
        complianceAuthority: compliance.publicKey,
        config: configPda,
        profile: profilePda(john.publicKey),
      })
      .signers([compliance])
      .rpc();

    const profile = await program.account.profile.fetch(profilePda(john.publicKey));
    assert.deepEqual(profile.kycTier, { enhanced: {} });
  });

  // Agents: spend policy, vault, approvals and lifecycle.
  // These tests share state and must run in the order written.

  let agent: PublicKey;
  let vault: PublicKey;
  let gwenUsdg: PublicKey;
  let vendorUsdg: PublicKey;
  let felixUsdg: PublicKey;

  const noFeeAccounts = {
    feeConfig: null,
    ownerVeiledTokenAccount: null,
    ownerStakePosition: null,
    treasuryTokenAccount: null,
  };

  it("opens a semi-autonomous agent with limits and its own token vault", async () => {
    agent = agentPda(john.publicKey, 0);
    vault = vaultPda(agent);

    await program.methods
      .createAgent(
        agentSigner.publicKey,
        "coding-assistant",
        { semiAutonomous: {} },
        USDG(50), // max per single payment
        USDG(100), // max across the rolling day
        USDG(20), // payments above this need owner sign-off
        false,
        []
      )
      .accountsPartial({
        owner: john.publicKey,
        profile: profilePda(john.publicKey),
        agent,
        mint: usdg,
        vault,
        tokenProgram: TOKEN_PROGRAM_ID,
        systemProgram: SystemProgram.programId,
      })
      .signers([john])
      .rpc();

    const a = await program.account.agent.fetch(agent);
    assert.equal(a.label, "coding-assistant");
    assert.deepEqual(a.status, { active: {} });
    assert.ok(a.agentSigner.equals(agentSigner.publicKey));
  });

  it("refuses a policy where one payment could exceed the daily cap", async () => {
    await expectError(
      program.methods
        .createAgent(
          agentSigner.publicKey,
          "bad-policy",
          { fullyAutonomous: {} },
          USDG(200),
          USDG(100),
          USDG(20),
          false,
          []
        )
        .accountsPartial({
          owner: john.publicKey,
          profile: profilePda(john.publicKey),
          agent: agentPda(john.publicKey, 1),
          mint: usdg,
          vault: vaultPda(agentPda(john.publicKey, 1)),
          tokenProgram: TOKEN_PROGRAM_ID,
          systemProgram: SystemProgram.programId,
        })
        .signers([john])
        .rpc(),
      "PerTxLimitExceedsDailyLimit"
    );
  });

  it("deposits 200 USDG from john into the agent vault", async () => {
    gwenUsdg = await createAssociatedTokenAccount(connection, payer, usdg, john.publicKey);
    vendorUsdg = await createAssociatedTokenAccount(connection, payer, usdg, vendor.publicKey);
    felixUsdg = await createAssociatedTokenAccount(connection, payer, usdg, felix.publicKey);
    await mintTo(connection, payer, usdg, gwenUsdg, payer, 1_000_000_000); // john starts with 1000 USDG

    await program.methods
      .fundAgent(USDG(200))
      .accountsPartial({
        funder: john.publicKey,
        agent,
        vault,
        mint: usdg,
        funderTokenAccount: gwenUsdg,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .signers([john])
      .rpc();

    const v = await getAccount(connection, vault);
    assert.equal(Number(v.amount), 200_000_000);
  });

  it("pays an x402 invoice under the threshold straight from the vault", async () => {
    const invoiceId = Array.from(Buffer.alloc(32, 7));
    await program.methods
      .payInvoice(USDG(10), invoiceId)
      .accountsPartial({
        agentSigner: agentSigner.publicKey,
        config: configPda,
        agent,
        vault,
        mint: usdg,
        recipient: vendor.publicKey,
        recipientTokenAccount: vendorUsdg,
        ...noFeeAccounts,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .signers([agentSigner])
      .rpc();

    const v = await getAccount(connection, vendorUsdg);
    assert.equal(Number(v.amount), 10_000_000);
    const a = await program.account.agent.fetch(agent);
    assert.equal(Number(a.windowSpent), 10_000_000);
  });

  it("refuses an immediate payment that needs human approval", async () => {
    await expectError(
      program.methods
        .payInvoice(USDG(30), ZERO32)
        .accountsPartial({
          agentSigner: agentSigner.publicKey,
          config: configPda,
          agent,
          vault,
          mint: usdg,
          recipient: vendor.publicKey,
          recipientTokenAccount: vendorUsdg,
          ...noFeeAccounts,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([agentSigner])
        .rpc(),
      "AmountExceedsHitlThreshold"
    );
  });

  it("refuses a payment larger than the single-payment cap", async () => {
    await expectError(
      program.methods
        .payInvoice(USDG(60), ZERO32)
        .accountsPartial({
          agentSigner: agentSigner.publicKey,
          config: configPda,
          agent,
          vault,
          mint: usdg,
          recipient: vendor.publicKey,
          recipientTokenAccount: vendorUsdg,
          ...noFeeAccounts,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([agentSigner])
        .rpc(),
      "PerTransactionLimitExceeded"
    );
  });

  it("parks a payment for review and pays it out once the owner approves", async () => {
    const pending = pendingPda(agent, 0);
    await program.methods
      .queueInvoice(USDG(30), ZERO32, ZERO32)
      .accountsPartial({
        agentSigner: agentSigner.publicKey,
        config: configPda,
        agent,
        recipient: vendor.publicKey,
        pending,
        systemProgram: SystemProgram.programId,
      })
      .signers([agentSigner])
      .rpc();

    await program.methods
      .approvePending()
      .accountsPartial({
        owner: john.publicKey,
        config: configPda,
        agent,
        pending,
        vault,
        mint: usdg,
        recipientTokenAccount: vendorUsdg,
        ...noFeeAccounts,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .signers([john])
      .rpc();

    const v = await getAccount(connection, vendorUsdg);
    assert.equal(Number(v.amount), 40_000_000);
    // Once paid out, the pending account should no longer exist.
    const closed = await connection.getAccountInfo(pending);
    assert.isNull(closed);
  });

  it("will not queue a payment small enough to pay directly", async () => {
    await expectError(
      program.methods
        .queueInvoice(USDG(5), ZERO32, ZERO32)
        .accountsPartial({
          agentSigner: agentSigner.publicKey,
          config: configPda,
          agent,
          recipient: vendor.publicKey,
          pending: pendingPda(agent, 1),
          systemProgram: SystemProgram.programId,
        })
        .signers([agentSigner])
        .rpc(),
      "AmountWithinHitlThreshold"
    );
  });

  it("discards a queued payment on owner rejection and leaves the vault untouched", async () => {
    const pending = pendingPda(agent, 1);
    await program.methods
      .queueInvoice(USDG(25), ZERO32, ZERO32)
      .accountsPartial({
        agentSigner: agentSigner.publicKey,
        config: configPda,
        agent,
        recipient: vendor.publicKey,
        pending,
        systemProgram: SystemProgram.programId,
      })
      .signers([agentSigner])
      .rpc();

    const vaultBefore = Number((await getAccount(connection, vault)).amount);
    await program.methods
      .rejectPending()
      .accountsPartial({ owner: john.publicKey, agent, pending })
      .signers([john])
      .rpc();

    assert.equal(Number((await getAccount(connection, vault)).amount), vaultBefore);
    assert.isNull(await connection.getAccountInfo(pending));
  });

  it("stops payments once the day's spend reaches the daily cap", async () => {
    // The window already holds 40 (the 10 direct payment plus the approved 30).
    // Three more payments of 20 bring it to exactly 100.
    await program.methods
      .payInvoice(USDG(20), ZERO32)
      .accountsPartial({
        agentSigner: agentSigner.publicKey,
        config: configPda,
        agent,
        vault,
        mint: usdg,
        recipient: vendor.publicKey,
        recipientTokenAccount: vendorUsdg,
        ...noFeeAccounts,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .signers([agentSigner])
      .rpc();

    await program.methods
      .payInvoice(USDG(20), ZERO32)
      .accountsPartial({
        agentSigner: agentSigner.publicKey,
        config: configPda,
        agent,
        vault,
        mint: usdg,
        recipient: vendor.publicKey,
        recipientTokenAccount: vendorUsdg,
        ...noFeeAccounts,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .signers([agentSigner])
      .rpc();

    // At 80 now. This last 20 fills the cap exactly, so even 1 more must fail.
    await program.methods
      .payInvoice(USDG(20), ZERO32)
      .accountsPartial({
        agentSigner: agentSigner.publicKey,
        config: configPda,
        agent,
        vault,
        mint: usdg,
        recipient: vendor.publicKey,
        recipientTokenAccount: vendorUsdg,
        ...noFeeAccounts,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .signers([agentSigner])
      .rpc();

    await expectError(
      program.methods
        .payInvoice(USDG(1), ZERO32)
        .accountsPartial({
          agentSigner: agentSigner.publicKey,
          config: configPda,
          agent,
          vault,
          mint: usdg,
          recipient: vendor.publicKey,
          recipientTokenAccount: vendorUsdg,
          ...noFeeAccounts,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([agentSigner])
        .rpc(),
      "DailyLimitExceeded"
    );
  });

  it("refuses to pay anyone missing from the allowlist once it is turned on", async () => {
    await program.methods
      .updateSpendPolicy(USDG(50), USDG(1000), USDG(20), true, [vendor.publicKey])
      .accountsPartial({ owner: john.publicKey, agent })
      .signers([john])
      .rpc();

    await expectError(
      program.methods
        .payInvoice(USDG(1), ZERO32)
        .accountsPartial({
          agentSigner: agentSigner.publicKey,
          config: configPda,
          agent,
          vault,
          mint: usdg,
          recipient: felix.publicKey,
          recipientTokenAccount: felixUsdg,
          ...noFeeAccounts,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([agentSigner])
        .rpc(),
      "RecipientNotAllowed"
    );
  });

  it("swaps the agent key so only the replacement can sign payments", async () => {
    await program.methods
      .rotateAgentSigner(newAgentSigner.publicKey)
      .accountsPartial({ owner: john.publicKey, agent })
      .signers([john])
      .rpc();

    await expectError(
      program.methods
        .payInvoice(USDG(1), ZERO32)
        .accountsPartial({
          agentSigner: agentSigner.publicKey,
          config: configPda,
          agent,
          vault,
          mint: usdg,
          recipient: vendor.publicKey,
          recipientTokenAccount: vendorUsdg,
          ...noFeeAccounts,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([agentSigner])
        .rpc(),
      "UnauthorizedAgentSigner"
    );

    await program.methods
      .payInvoice(USDG(1), ZERO32)
      .accountsPartial({
        agentSigner: newAgentSigner.publicKey,
        config: configPda,
        agent,
        vault,
        mint: usdg,
        recipient: vendor.publicKey,
        recipientTokenAccount: vendorUsdg,
        ...noFeeAccounts,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .signers([newAgentSigner])
      .rpc();
  });

  it("freezes payments while the agent is paused, then reactivates it", async () => {
    await program.methods
      .setAgentStatus({ paused: {} })
      .accountsPartial({ owner: john.publicKey, agent })
      .signers([john])
      .rpc();

    await expectError(
      program.methods
        .payInvoice(USDG(1), ZERO32)
        .accountsPartial({
          agentSigner: newAgentSigner.publicKey,
          config: configPda,
          agent,
          vault,
          mint: usdg,
          recipient: vendor.publicKey,
          recipientTokenAccount: vendorUsdg,
          ...noFeeAccounts,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([newAgentSigner])
        .rpc(),
      "AgentNotActive"
    );

    await program.methods
      .setAgentStatus({ active: {} })
      .accountsPartial({ owner: john.publicKey, agent })
      .signers([john])
      .rpc();
  });

  it("returns 10 USDG from the vault to the owner on request", async () => {
    const before = Number((await getAccount(connection, gwenUsdg)).amount);
    await program.methods
      .withdrawFromAgent(USDG(10))
      .accountsPartial({
        owner: john.publicKey,
        agent,
        vault,
        mint: usdg,
        ownerTokenAccount: gwenUsdg,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .signers([john])
      .rpc();
    const after = Number((await getAccount(connection, gwenUsdg)).amount);
    assert.equal(after - before, 10_000_000);
  });

  it("empties the vault to the owner on revoke and locks the agent for good", async () => {
    const vaultBalance = Number((await getAccount(connection, vault)).amount);
    const before = Number((await getAccount(connection, gwenUsdg)).amount);

    await program.methods
      .revokeAgent()
      .accountsPartial({
        owner: john.publicKey,
        agent,
        vault,
        mint: usdg,
        ownerTokenAccount: gwenUsdg,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .signers([john])
      .rpc();

    const after = Number((await getAccount(connection, gwenUsdg)).amount);
    assert.equal(after - before, vaultBalance);
    assert.equal(Number((await getAccount(connection, vault)).amount), 0);

    // Revocation is final: deposits and status changes both fail afterwards.
    await expectError(
      program.methods
        .fundAgent(USDG(1))
        .accountsPartial({
          funder: john.publicKey,
          agent,
          vault,
          mint: usdg,
          funderTokenAccount: gwenUsdg,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([john])
        .rpc(),
      "AgentAlreadyRevoked"
    );
    await expectError(
      program.methods
        .setAgentStatus({ active: {} })
        .accountsPartial({ owner: john.publicKey, agent })
        .signers([john])
        .rpc(),
      "AgentAlreadyRevoked"
    );
  });

  // Payment requests, public and confidential

  it("lets felix pay john's public request exactly once at the right amount", async () => {
    await mintTo(connection, payer, usdg, felixUsdg, payer, 500_000_000);
    const request = requestPda(john.publicKey, 0);

    const expiry = Math.floor(Date.now() / 1000) + 3600;
    await program.methods
      .createRequest(false, USDG(25), ZERO32, ZERO32, new anchor.BN(expiry))
      .accountsPartial({
        requester: john.publicKey,
        profile: profilePda(john.publicKey),
        request,
        receiver: john.publicKey,
        mint: usdg,
        systemProgram: SystemProgram.programId,
      })
      .signers([john])
      .rpc();

    // Paying 24 against a 25 request must fail with nothing transferred.
    await expectError(
      program.methods
        .fulfillRequest(USDG(24), ZERO32)
        .accountsPartial({
          payer: felix.publicKey,
          config: configPda,
          request,
          mint: usdg,
          payerTokenAccount: felixUsdg,
          receiverTokenAccount: gwenUsdg,
          feeConfig: null,
          payerVeiledTokenAccount: null,
          payerStakePosition: null,
          treasuryTokenAccount: null,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([felix])
        .rpc(),
      "RequestAmountMismatch"
    );

    const before = Number((await getAccount(connection, gwenUsdg)).amount);
    await program.methods
      .fulfillRequest(USDG(25), ZERO32)
      .accountsPartial({
        payer: felix.publicKey,
        config: configPda,
        request,
        mint: usdg,
        payerTokenAccount: felixUsdg,
        receiverTokenAccount: gwenUsdg,
        feeConfig: null,
        payerVeiledTokenAccount: null,
        payerStakePosition: null,
        treasuryTokenAccount: null,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .signers([felix])
      .rpc();
    const after = Number((await getAccount(connection, gwenUsdg)).amount);
    assert.equal(after - before, 25_000_000);

    // A second payment against the same request is refused.
    await expectError(
      program.methods
        .fulfillRequest(USDG(25), ZERO32)
        .accountsPartial({
          payer: felix.publicKey,
          config: configPda,
          request,
          mint: usdg,
          payerTokenAccount: felixUsdg,
          receiverTokenAccount: gwenUsdg,
          feeConfig: null,
          payerVeiledTokenAccount: null,
          payerStakePosition: null,
          treasuryTokenAccount: null,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([felix])
        .rpc(),
      "RequestNotOpen"
    );
  });

  it("settles a hidden-amount request only when amount and blinding match the hash", async () => {
    const request = requestPda(john.publicKey, 1);
    const amount = USDG(40);
    const blinding = Buffer.alloc(32, 9);
    const commitment = Buffer.from(
      keccak_256.arrayBuffer(
        Buffer.concat([amount.toArrayLike(Buffer, "le", 8), blinding])
      )
    );

    const expiry = Math.floor(Date.now() / 1000) + 3600;
    await program.methods
      .createRequest(true, new anchor.BN(0), Array.from(commitment), ZERO32, new anchor.BN(expiry))
      .accountsPartial({
        requester: john.publicKey,
        profile: profilePda(john.publicKey),
        request,
        receiver: john.publicKey,
        mint: usdg,
        systemProgram: SystemProgram.programId,
      })
      .signers([john])
      .rpc();

    // Off by one unit, the keccak hash no longer matches what john stored.
    await expectError(
      program.methods
        .fulfillRequest(USDG(39), Array.from(blinding))
        .accountsPartial({
          payer: felix.publicKey,
          config: configPda,
          request,
          mint: usdg,
          payerTokenAccount: felixUsdg,
          receiverTokenAccount: gwenUsdg,
          feeConfig: null,
          payerVeiledTokenAccount: null,
          payerStakePosition: null,
          treasuryTokenAccount: null,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([felix])
        .rpc(),
      "RequestCommitmentMismatch"
    );

    await program.methods
      .fulfillRequest(amount, Array.from(blinding))
      .accountsPartial({
        payer: felix.publicKey,
        config: configPda,
        request,
        mint: usdg,
        payerTokenAccount: felixUsdg,
        receiverTokenAccount: gwenUsdg,
        feeConfig: null,
        payerVeiledTokenAccount: null,
        payerStakePosition: null,
        treasuryTokenAccount: null,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .signers([felix])
      .rpc();

    const r = await program.account.paymentRequest.fetch(request);
    assert.deepEqual(r.status, { fulfilled: {} });
  });

  it("refuses payment on a request its creator has cancelled", async () => {
    const request = requestPda(john.publicKey, 2);
    const expiry = Math.floor(Date.now() / 1000) + 3600;
    await program.methods
      .createRequest(false, USDG(5), ZERO32, ZERO32, new anchor.BN(expiry))
      .accountsPartial({
        requester: john.publicKey,
        profile: profilePda(john.publicKey),
        request,
        receiver: john.publicKey,
        mint: usdg,
        systemProgram: SystemProgram.programId,
      })
      .signers([john])
      .rpc();

    await program.methods
      .cancelRequest()
      .accountsPartial({ requester: john.publicKey, request })
      .signers([john])
      .rpc();

    await expectError(
      program.methods
        .fulfillRequest(USDG(5), ZERO32)
        .accountsPartial({
          payer: felix.publicKey,
          config: configPda,
          request,
          mint: usdg,
          payerTokenAccount: felixUsdg,
          receiverTokenAccount: gwenUsdg,
          feeConfig: null,
          payerVeiledTokenAccount: null,
          payerStakePosition: null,
          treasuryTokenAccount: null,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([felix])
        .rpc(),
      "RequestNotOpen"
    );
  });

  // Disclosure receipts

  it("records a disclosure receipt against a transaction reference", async () => {
    const receipt = disclosurePda(john.publicKey, 0);
    const txRef = "5".repeat(88); // stand-in with the length of a base58 tx signature
    await program.methods
      .fileDisclosure(txRef, Array.from(Buffer.alloc(32, 3)), Array.from(Buffer.alloc(32, 4)))
      .accountsPartial({
        owner: john.publicKey,
        profile: profilePda(john.publicKey),
        receipt,
        systemProgram: SystemProgram.programId,
      })
      .signers([john])
      .rpc();

    const r = await program.account.disclosureReceipt.fetch(receipt);
    assert.equal(r.txReference, txRef);
    assert.equal(Number(r.index), 0);
  });

  // $VEILED staking and protocol fees

  it("creates the fee config with the default rate, cap and four discount tiers", async () => {
    await program.methods
      .initFees()
      .accountsPartial({
        authority: provider.wallet.publicKey,
        config: configPda,
        feeConfig: feesPda,
        veiledMint,
        stakeVault: stakeVaultPda,
        tokenProgram: TOKEN_PROGRAM_ID,
        systemProgram: SystemProgram.programId,
      })
      .rpc();

    const fees = await program.account.feeConfig.fetch(feesPda);
    assert.equal(fees.baseFeeBps, 10);
    assert.equal(Number(fees.feeCap), 5_000_000);
    assert.equal(fees.heldWeightBps, 5000);
    assert.equal(fees.tiers.length, 4);
  });

  it("locks $VEILED into a stake position and refuses to unstake more than it holds", async () => {
    const gwenVeiled = await createAssociatedTokenAccount(
      connection,
      payer,
      veiledMint,
      john.publicKey
    );
    await mintTo(connection, payer, veiledMint, gwenVeiled, payer, BigInt(VEIL(20_000).toString()));

    await program.methods
      .stake(VEIL(10_000))
      .accountsPartial({
        staker: john.publicKey,
        feeConfig: feesPda,
        stakePosition: stakePda(john.publicKey),
        veiledMint,
        stakerTokenAccount: gwenVeiled,
        stakeVault: stakeVaultPda,
        tokenProgram: TOKEN_PROGRAM_ID,
        systemProgram: SystemProgram.programId,
      })
      .signers([john])
      .rpc();

    const position = await program.account.stakePosition.fetch(stakePda(john.publicKey));
    assert.equal(position.amount.toString(), VEIL(10_000).toString());
    assert.isAbove(Number(position.stakedSince), 0);

    await expectError(
      program.methods
        .unstake(VEIL(20_000))
        .accountsPartial({
          staker: john.publicKey,
          feeConfig: feesPda,
          stakePosition: stakePda(john.publicKey),
          veiledMint,
          stakerTokenAccount: gwenVeiled,
          stakeVault: stakeVaultPda,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([john])
        .rpc(),
      "InsufficientStakedBalance"
    );
  });

  it("sends fees to the treasury and discounts them for a staked payer", async () => {
    const treasuryUsdg = await createAssociatedTokenAccount(
      connection,
      payer,
      usdg,
      treasury.publicKey
    );
    await program.methods
      .setFeeRouting(treasury.publicKey, true)
      .accountsPartial({ authority: provider.wallet.publicKey, config: configPda })
      .rpc();

    // First direction: felix pays john 1000 USDG. john's stake does not help
    // here because discounts follow the payer, and felix has no stake, so the
    // full 0.10% rate applies and the treasury gets 1 USDG.
    await mintTo(connection, payer, usdg, felixUsdg, payer, 2_000_000_000);
    const request = requestPda(john.publicKey, 3);
    const expiry = Math.floor(Date.now() / 1000) + 3600;
    await program.methods
      .createRequest(false, USDG(1000), ZERO32, ZERO32, new anchor.BN(expiry))
      .accountsPartial({
        requester: john.publicKey,
        profile: profilePda(john.publicKey),
        request,
        receiver: john.publicKey,
        mint: usdg,
        systemProgram: SystemProgram.programId,
      })
      .signers([john])
      .rpc();

    await program.methods
      .fulfillRequest(USDG(1000), ZERO32)
      .accountsPartial({
        payer: felix.publicKey,
        config: configPda,
        request,
        mint: usdg,
        payerTokenAccount: felixUsdg,
        receiverTokenAccount: gwenUsdg,
        feeConfig: feesPda,
        payerVeiledTokenAccount: null,
        payerStakePosition: null,
        treasuryTokenAccount: treasuryUsdg,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .signers([felix])
      .rpc();

    let t = await getAccount(connection, treasuryUsdg);
    assert.equal(Number(t.amount), 1_000_000); // full rate, nothing taken off

    // Second direction: john pays felix and passes in her stake position.
    // Her 10k $VEILED earns 25% off, so this fee is 0.75 USDG instead of 1.
    const felixRequest = requestPda(felix.publicKey, 0);
    await program.methods
      .createRequest(false, USDG(1000), ZERO32, ZERO32, new anchor.BN(expiry))
      .accountsPartial({
        requester: felix.publicKey,
        profile: profilePda(felix.publicKey),
        request: felixRequest,
        receiver: felix.publicKey,
        mint: usdg,
        systemProgram: SystemProgram.programId,
      })
      .signers([felix])
      .rpc();

    await program.methods
      .fulfillRequest(USDG(1000), ZERO32)
      .accountsPartial({
        payer: john.publicKey,
        config: configPda,
        request: felixRequest,
        mint: usdg,
        payerTokenAccount: gwenUsdg,
        receiverTokenAccount: felixUsdg,
        feeConfig: feesPda,
        payerVeiledTokenAccount: null,
        payerStakePosition: stakePda(john.publicKey),
        treasuryTokenAccount: treasuryUsdg,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .signers([john])
      .rpc();

    t = await getAccount(connection, treasuryUsdg);
    assert.equal(Number(t.amount), 1_000_000 + 750_000);
  });

  it("never charges more than the fixed fee ceiling on a huge payment", async () => {
    // Uncapped, 0.10% of 100,000 USDG is 100 USDG. The ceiling is 5 USDG.
    await mintTo(connection, payer, usdg, felixUsdg, payer, 200_000_000_000);
    const treasuryUsdg = getAssociatedTokenAddressSync(usdg, treasury.publicKey);
    const before = Number((await getAccount(connection, treasuryUsdg)).amount);

    const request = requestPda(john.publicKey, 4);
    const expiry = Math.floor(Date.now() / 1000) + 3600;
    await program.methods
      .createRequest(false, USDG(100_000), ZERO32, ZERO32, new anchor.BN(expiry))
      .accountsPartial({
        requester: john.publicKey,
        profile: profilePda(john.publicKey),
        request,
        receiver: john.publicKey,
        mint: usdg,
        systemProgram: SystemProgram.programId,
      })
      .signers([john])
      .rpc();

    await program.methods
      .fulfillRequest(USDG(100_000), ZERO32)
      .accountsPartial({
        payer: felix.publicKey,
        config: configPda,
        request,
        mint: usdg,
        payerTokenAccount: felixUsdg,
        receiverTokenAccount: gwenUsdg,
        feeConfig: feesPda,
        payerVeiledTokenAccount: null,
        payerStakePosition: null,
        treasuryTokenAccount: treasuryUsdg,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .signers([felix])
      .rpc();

    const after = Number((await getAccount(connection, treasuryUsdg)).amount);
    assert.equal(after - before, 5_000_000);
  });

  // Global pause switch

  it("refuses to settle a request while the protocol is paused", async () => {
    await program.methods
      .setPause(true)
      .accountsPartial({ authority: provider.wallet.publicKey, config: configPda })
      .rpc();

    const request = requestPda(john.publicKey, 5);
    const expiry = Math.floor(Date.now() / 1000) + 3600;
    await program.methods
      .createRequest(false, USDG(5), ZERO32, ZERO32, new anchor.BN(expiry))
      .accountsPartial({
        requester: john.publicKey,
        profile: profilePda(john.publicKey),
        request,
        receiver: john.publicKey,
        mint: usdg,
        systemProgram: SystemProgram.programId,
      })
      .signers([john])
      .rpc();

    await expectError(
      program.methods
        .fulfillRequest(USDG(5), ZERO32)
        .accountsPartial({
          payer: felix.publicKey,
          config: configPda,
          request,
          mint: usdg,
          payerTokenAccount: felixUsdg,
          receiverTokenAccount: gwenUsdg,
          feeConfig: null,
          payerVeiledTokenAccount: null,
          payerStakePosition: null,
          treasuryTokenAccount: null,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([felix])
        .rpc(),
      "ProtocolPaused"
    );

    await program.methods
      .setPause(false)
      .accountsPartial({ authority: provider.wallet.publicKey, config: configPda })
      .rpc();
  });

  // Reclaiming rent from requests

  it("refunds rent for a finished request but keeps an unexpired open one", async () => {
    // Request 5 was created during the pause test, never paid, and is still in date.
    await expectError(
      program.methods
        .closeRequest()
        .accountsPartial({
          requester: john.publicKey,
          request: requestPda(john.publicKey, 5),
        })
        .signers([john])
        .rpc(),
      "RequestStillOpen"
    );

    // Request 2 was cancelled earlier, so closing it should credit john's lamports.
    const request = requestPda(john.publicKey, 2);
    const before = await connection.getBalance(john.publicKey);
    await program.methods
      .closeRequest()
      .accountsPartial({ requester: john.publicKey, request })
      .signers([john])
      .rpc();
    assert.isNull(await connection.getAccountInfo(request));
    assert.isAbove(await connection.getBalance(john.publicKey), before);
  });
});
