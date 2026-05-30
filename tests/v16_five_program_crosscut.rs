//! Phase 3A.0 — 5-program assembled cross-cut SMOKE (load feasibility + token
//! coexistence). See `~/wrapper-engine-deep-audit/phase3a_crosscut_design.md`.
//!
//! This is the FIRST time wrapper + matcher + NFT + stake + Token-2022 are mounted
//! into ONE litesvm-0.1 instance. It ships before the economic spine (3A.1) to
//! de-risk the genuine unknowns:
//!   * Can all five `.so`s co-load? (stake is GREENFIELD — loaded by ZERO wrapper
//!     tests before now; its id-allowlisting under litesvm 0.1 was unverified.)
//!   * Does `with_spl_programs()` under 0.1 mount BOTH classic SPL (`Tokenkeg`) and
//!     Token-2022 (`TokenzQd`) as distinct, runnable programs?
//!   * Does the wrapper vault accept ONLY classic SPL (`verify_token_program`,
//!     `v16_program.rs:12391`)?
//!
//! # Anti-hollow discipline (carried from Phase 2.E)
//! Each test carries a load-bearing EXECUTED guard, not a silent mount check:
//!   * `coload_executable_and_distinct` — mounts all 7 program accounts and proves
//!     each is `executable`; proves classic ≠ Token-2022 (distinct roles).
//!   * `stake_program_executes_under_litesvm_01` — REAL invocation of the stake
//!     `.so`; an empty-data tx must fail with the stake program's OWN
//!     `InvalidInstructionData` (its `StakeInstruction::unpack` `split_first`
//!     reject), proving the sbpf-v0 stake binary actually runs under the VM — NOT
//!     a loader rejection.
//!   * `classic_and_token2022_mints_coexist` — REAL `InitializeMint2` on EACH token
//!     program in the SAME assembled instance; both succeed and the resulting mints
//!     are owned by their respective (distinct) programs.
//!   * `wrapper_vault_gate_requires_classic_token_program` — REAL `CreateLpVault`
//!     (tag 65) reaching `verify_token_program`: Token-2022 → `Custom(13)`
//!     (`InvalidTokenProgram`) at the gate; classic SPL passes the gate and fails
//!     DOWNSTREAM at market-magic parse (a different code), isolating the gate as
//!     the operative token-program discriminator.

mod common;
use common::*;

use litesvm::LiteSVM;
use percolator::{MarketGroupV16, PortfolioAccountV16};
use percolator_prog::{ix::Instruction as ProgInstruction, processor::ASSET_ACTION_ACTIVATE, state};
use solana_sdk::{
    account::Account,
    instruction::{AccountMeta, Instruction, InstructionError},
    program_pack::Pack,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    system_instruction, system_program,
    transaction::TransactionError,
};
use spl_token::state::{Account as TokenAccount, Mint};

/// `InvalidTokenProgram` — `PercolatorError` ordinal 13, mapped via
/// `From<PercolatorError> for ProgramError => Custom(value as u32)`
/// (`v16_program.rs:189,231-234`).
const INVALID_TOKEN_PROGRAM: u32 = 13;
/// `NotInitialized` — ordinal 3. The post-gate market-magic parse on a zeroed
/// market returns this (verified by diagnostic at 3A.0), proving the classic
/// token program got PAST `verify_token_program`.
const NOT_INITIALIZED: u32 = 3;

// ── 3A.0-A: all five co-load executable + distinct token roles ──────────────

#[test]
fn x0_smoke_coload_executable_and_distinct() {
    let matcher_id = Pubkey::new_unique();
    let svm = assemble_five_program_svm(matcher_id);

    for (label, id) in [
        ("wrapper@MAINNET", PERCOLATOR_MAINNET),
        ("nft", NFT_PROGRAM_ID),
        ("stake", STAKE_ID),
        ("matcher", matcher_id),
        ("classic-spl-token", spl_token_classic_id()),
        ("token-2022", TOKEN_2022),
        ("ata", ATA_PROGRAM),
    ] {
        let acct = svm
            .get_account(&id)
            .unwrap_or_else(|| panic!("{label} ({id}) program account missing after load"));
        assert!(
            acct.executable,
            "{label} ({id}) must be mounted as an executable program"
        );
    }

    // Distinct roles — the vault uses classic only; Token-2022 is the NFT mint side.
    assert_ne!(
        spl_token_classic_id(),
        TOKEN_2022,
        "classic SPL and Token-2022 must be distinct programs"
    );
}

// ── 3A.0-B: the greenfield stake .so actually executes under litesvm 0.1 ────

#[test]
fn x0_smoke_stake_program_executes_under_litesvm_01() {
    let matcher_id = Pubkey::new_unique();
    let mut svm = assemble_five_program_svm(matcher_id);
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 1_000_000_000).unwrap();

    // Empty instruction data → `StakeInstruction::unpack` reaches
    // `data.split_first().ok_or(ProgramError::InvalidInstructionData)`
    // (`instruction.rs:211-213`). A program-level `InvalidInstructionData` (NOT a
    // loader rejection) proves the stake sbpf-v0 binary entered and ran.
    let stake_ix = Instruction {
        program_id: STAKE_ID,
        accounts: vec![],
        data: vec![],
    };
    let res = send_ixs(&mut svm, &payer, vec![stake_ix], &[]);
    assert_instruction_error(
        &res,
        InstructionError::InvalidInstructionData,
        "greenfield stake .so executes (empty-data decode reject)",
    );
}

// ── 3A.0-C: classic SPL + Token-2022 mints coexist in one instance ──────────

#[test]
fn x0_smoke_classic_and_token2022_mints_coexist() {
    let matcher_id = Pubkey::new_unique();
    let mut svm = assemble_five_program_svm(matcher_id);
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();

    let space = Mint::LEN; // 82 — a no-extension mint (identical base layout in both)

    // (1) Real classic-SPL InitializeMint2 via the canonical builder.
    let classic_mint = Keypair::new();
    let lamports = svm.minimum_balance_for_rent_exemption(space);
    let classic_create = system_instruction::create_account(
        &payer.pubkey(),
        &classic_mint.pubkey(),
        lamports,
        space as u64,
        &spl_token_classic_id(),
    );
    let classic_init = spl_token::instruction::initialize_mint2(
        &spl_token_classic_id(),
        &classic_mint.pubkey(),
        &payer.pubkey(),
        None,
        0,
    )
    .unwrap();
    send_ixs(
        &mut svm,
        &payer,
        vec![classic_create, classic_init],
        &[&classic_mint],
    )
    .expect("classic SPL mint initializes in the assembled 5-program instance");

    let classic_acct = svm
        .get_account(&classic_mint.pubkey())
        .expect("classic mint exists");
    assert_eq!(
        classic_acct.owner,
        spl_token_classic_id(),
        "classic mint owned by Tokenkeg"
    );
    let unpacked = Mint::unpack(&classic_acct.data).expect("classic mint unpacks");
    assert!(unpacked.is_initialized, "classic mint is initialized");

    // (2) Real Token-2022 InitializeMint2 (hand-encoded) on a fresh account, in the
    // SAME svm. Wire: tag(1)=20, decimals(1)=0, mint_authority(32), freeze_opt(1)=0.
    let t22_mint = Keypair::new();
    let t22_create = system_instruction::create_account(
        &payer.pubkey(),
        &t22_mint.pubkey(),
        lamports,
        space as u64,
        &TOKEN_2022,
    );
    let mut t22_init_data = Vec::with_capacity(35);
    t22_init_data.push(20u8); // IX_INITIALIZE_MINT2
    t22_init_data.push(0u8); // decimals
    t22_init_data.extend_from_slice(payer.pubkey().as_ref()); // mint authority
    t22_init_data.push(0u8); // freeze authority = None
    let t22_init = Instruction {
        program_id: TOKEN_2022,
        accounts: vec![AccountMeta::new(t22_mint.pubkey(), false)],
        data: t22_init_data,
    };
    send_ixs(&mut svm, &payer, vec![t22_create, t22_init], &[&t22_mint])
        .expect("Token-2022 mint initializes in the assembled 5-program instance");

    let t22_acct = svm
        .get_account(&t22_mint.pubkey())
        .expect("token-2022 mint exists");
    assert_eq!(t22_acct.owner, TOKEN_2022, "token-2022 mint owned by TokenzQd");
    // Layout-explicit init check (offset 45 = `is_initialized`) — avoids relying on
    // classic unpack semantics for a Token-2022-owned account.
    assert!(
        t22_acct.data.len() >= 46 && t22_acct.data[45] == 1,
        "token-2022 mint is initialized"
    );
}

// ── 3A.0-D: wrapper vault gate requires classic SPL (verify_token_program) ──

/// `CreateLpVault` (tag 65). `handle_create_lp_vault` (`v16_program.rs:5266`)
/// calls `verify_token_program` at :5286 after only account-flag + owner checks,
/// so it is the lightest honest path to the gate.
fn create_lp_vault_ix(
    market: Pubkey,
    registry: Pubkey,
    mint: Pubkey,
    admin: Pubkey,
    token_program: Pubkey,
) -> Instruction {
    Instruction {
        program_id: PERCOLATOR_MAINNET,
        accounts: vec![
            AccountMeta::new(admin, true),                        // 0 admin (signer, writable)
            AccountMeta::new_readonly(market, false),             // 1 market (owner==MAINNET; read)
            AccountMeta::new(registry, false),                    // 2 registry PDA (writable)
            AccountMeta::new(mint, false),                        // 3 mint PDA (writable)
            AccountMeta::new_readonly(system_program::ID, false), // 4 system program
            AccountMeta::new_readonly(token_program, false),      // 5 token program (under test)
        ],
        data: ProgInstruction::CreateLpVault {
            fee_share_bps: 0,
            redemption_cooldown_slots: 0,
            oi_reservation_threshold_bps: 0,
            domain: 0,
        }
        .encode(),
    }
}

#[test]
fn x0_smoke_wrapper_vault_gate_requires_classic_token_program() {
    let matcher_id = Pubkey::new_unique();
    let mut svm = assemble_five_program_svm(matcher_id);
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();

    // Market owned by the wrapper (passes `expect_owner(market, program_id=MAINNET)`)
    // but with zeroed content (so the classic-token positive path fails downstream
    // at the magic parse, not at the gate).
    let market = Pubkey::new_unique();
    let market_len = percolator_prog::state::market_account_len_for_capacity(1).unwrap();
    svm.set_account(
        market,
        Account {
            lamports: 1_000_000_000,
            data: vec![0u8; market_len],
            owner: PERCOLATOR_MAINNET,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();
    let registry = Pubkey::new_unique();
    let mint = Pubkey::new_unique();

    // NEGATIVE — Token-2022 supplied where the vault demands classic SPL.
    let res_t22 = send_ixs(
        &mut svm,
        &payer,
        vec![create_lp_vault_ix(market, registry, mint, payer.pubkey(), TOKEN_2022)],
        &[],
    );
    assert_custom(
        res_t22,
        INVALID_TOKEN_PROGRAM,
        "wrapper vault rejects Token-2022 as the token program",
    );

    // POSITIVE — classic SPL passes the gate; the tx fails DOWNSTREAM at the
    // zeroed-market parse with `NotInitialized` (Custom 3), NOT `InvalidTokenProgram`
    // — proving classic SPL is accepted and execution proceeded past the gate into
    // market parsing. Two different operative codes at two different stages isolate
    // `verify_token_program` as the token-program discriminator.
    let res_classic = send_ixs(
        &mut svm,
        &payer,
        vec![create_lp_vault_ix(
            market,
            registry,
            mint,
            payer.pubkey(),
            spl_token_classic_id(),
        )],
        &[],
    );
    assert_custom(
        res_classic,
        NOT_INITIALIZED,
        "wrapper vault accepts classic SPL (gate passed → fails downstream at market parse)",
    );
}

// ════════════════════════════════════════════════════════════════════════════
// 3A.1 — Economic spine. The adversarial `Env` (v16_fork_adversarial.rs:148-486)
// re-keyed to `PERCOLATOR_MAINNET` and hosted in the assembled 5-program svm.
//
// THE GOTCHA (design doc §"load order"): `vault_authority` and every other
// wrapper PDA must be derived under MAINNET, not `percolator_prog::id()`. Because
// `init_matcher_context` derives the matcher delegate from `self.program_id`
// (matcher_delegate_key's FIRST arg, v16_cu.rs:1128), re-keying `program_id` here
// makes the test-side delegate match the wrapper's own derivation automatically.
// ════════════════════════════════════════════════════════════════════════════

const CROSSCUT_MAX_ASSETS: u16 = 1;
/// Tradable asset index (proven v16_cu pattern: activate+trade asset 1, domain 2).
const CROSSCUT_ASSET: u16 = 1;

// Fields are consumed incrementally across the 3A.1→3A.5 lifecycle build
// (matcher_program/vault_authority land with the trade + withdraw steps next).
#[allow(dead_code)]
struct CrosscutEnv {
    svm: LiteSVM,
    /// = `PERCOLATOR_MAINNET` (NOT `percolator_prog::id()`).
    program_id: Pubkey,
    matcher_program: Pubkey,
    payer: Keypair,
    admin: Keypair,
    market: Pubkey,
    mint: Pubkey,
    vault: Pubkey,
    vault_authority: Pubkey,
    portfolio_len: usize,
}

impl CrosscutEnv {
    /// 100% margins, 0 base trade fee — one tradable asset activated.
    fn new() -> Self {
        let matcher_program = Pubkey::new_unique();
        let mut svm = assemble_five_program_svm(matcher_program);
        let program_id = PERCOLATOR_MAINNET;

        let payer = Keypair::new();
        let admin = Keypair::new();
        let market = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let vault = Pubkey::new_unique();
        // PDA derived under MAINNET — the error-prone re-key.
        let (vault_authority, _) =
            Pubkey::find_program_address(&[b"vault", market.as_ref()], &program_id);

        svm.airdrop(&payer.pubkey(), 1_000_000_000_000).unwrap();
        svm.airdrop(&admin.pubkey(), 1_000_000_000_000).unwrap();
        svm.set_account(
            mint,
            Account {
                lamports: 1_000_000_000,
                data: make_mint_data(),
                owner: spl_token_classic_id(),
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
        svm.set_account(
            vault,
            Account {
                lamports: 1_000_000_000,
                data: make_token_data(mint, vault_authority, 0),
                owner: spl_token_classic_id(),
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
        let market_len = state::market_account_len_for_capacity(CROSSCUT_MAX_ASSETS as usize).unwrap();
        svm.set_account(
            market,
            Account {
                lamports: 1_000_000_000,
                data: vec![0u8; market_len],
                owner: program_id, // MAINNET — passes expect_owner(market, program_id)
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

        let portfolio_len =
            state::portfolio_account_len_for_market_slots(CROSSCUT_MAX_ASSETS as usize).unwrap();
        let mut env = CrosscutEnv {
            svm,
            program_id,
            matcher_program,
            payer,
            admin,
            market,
            mint,
            vault,
            vault_authority,
            portfolio_len,
        };

        let admin = env.admin.insecure_clone();
        env.try_wrapper(
            ProgInstruction::InitMarket {
                max_portfolio_assets: CROSSCUT_MAX_ASSETS,
                h_min: 0,
                h_max: 10,
                initial_price: 100,
                min_nonzero_mm_req: 1,
                min_nonzero_im_req: 2,
                maintenance_margin_bps: 10_000,
                initial_margin_bps: 10_000,
                max_trading_fee_bps: 10_000,
                trade_fee_base_bps: 0,
                liquidation_fee_bps: 0,
                liquidation_fee_cap: 0,
                min_liquidation_abs: 0,
                max_price_move_bps_per_slot: 10_000,
                max_accrual_dt_slots: 1,
                max_abs_funding_e9_per_slot: 0,
                min_funding_lifetime_slots: 1,
                max_account_b_settlement_chunks: 1,
                max_bankrupt_close_chunks: 1,
                max_bankrupt_close_lifetime_slots: 100,
                public_b_chunk_atoms: percolator::MAX_VAULT_TVL,
                maintenance_fee_per_slot: 0,
            },
            vec![
                AccountMeta::new(admin.pubkey(), true),
                AccountMeta::new(env.market, false),
                AccountMeta::new_readonly(env.mint, false),
            ],
            &[&admin],
        )
        .expect("init market @ MAINNET");

        env.activate_asset(CROSSCUT_ASSET, 1, 100);
        env
    }

    fn activate_asset(&mut self, asset_index: u16, now_slot: u64, initial_price: u64) {
        let admin = self.admin.insecure_clone();
        self.try_wrapper(
            ProgInstruction::UpdateAssetLifecycle {
                action: ASSET_ACTION_ACTIVATE,
                asset_index,
                now_slot,
                initial_price,
                insurance_authority: admin.pubkey().to_bytes(),
                insurance_operator: admin.pubkey().to_bytes(),
                backing_bucket_authority: admin.pubkey().to_bytes(),
                oracle_authority: admin.pubkey().to_bytes(),
            },
            vec![
                AccountMeta::new(admin.pubkey(), true),
                AccountMeta::new(self.market, false),
            ],
            &[&admin],
        )
        .expect("activate asset");
    }

    fn create_portfolio(&mut self, owner: &Keypair) -> Pubkey {
        self.svm.airdrop(&owner.pubkey(), 1_000_000_000).unwrap();
        let portfolio = Pubkey::new_unique();
        self.svm
            .set_account(
                portfolio,
                Account {
                    lamports: 1_000_000_000,
                    data: vec![0u8; self.portfolio_len],
                    owner: self.program_id,
                    executable: false,
                    rent_epoch: 0,
                },
            )
            .unwrap();
        self.try_wrapper(
            ProgInstruction::InitPortfolio,
            vec![
                AccountMeta::new(owner.pubkey(), true),
                AccountMeta::new(self.market, false),
                AccountMeta::new(portfolio, false),
            ],
            &[owner],
        )
        .expect("init portfolio");
        portfolio
    }

    /// Deposit `amount` collateral; returns the (now-drained) source token account.
    fn deposit(&mut self, owner: &Keypair, portfolio: Pubkey, amount: u128) -> Pubkey {
        let source = Pubkey::new_unique();
        self.svm
            .set_account(
                source,
                Account {
                    lamports: 1_000_000_000,
                    data: make_token_data(self.mint, owner.pubkey(), amount as u64),
                    owner: spl_token_classic_id(),
                    executable: false,
                    rent_epoch: 0,
                },
            )
            .unwrap();
        self.try_wrapper(
            ProgInstruction::Deposit { amount },
            vec![
                AccountMeta::new(owner.pubkey(), true),
                AccountMeta::new(self.market, false),
                AccountMeta::new(portfolio, false),
                AccountMeta::new(source, false),
                AccountMeta::new(self.vault, false),
                AccountMeta::new_readonly(spl_token_classic_id(), false),
            ],
            &[owner],
        )
        .expect("deposit");
        source
    }

    fn try_wrapper(
        &mut self,
        ix: ProgInstruction,
        accounts: Vec<AccountMeta>,
        signers: &[&Keypair],
    ) -> Result<(), TransactionError> {
        let wix = Instruction {
            program_id: self.program_id,
            accounts,
            data: ix.encode(),
        };
        send_ixs(&mut self.svm, &self.payer, vec![wix], signers)
    }

    // ── readers ──
    fn group(&self) -> MarketGroupV16 {
        state::read_market(&self.svm.get_account(&self.market).unwrap().data)
            .unwrap()
            .1
    }
    fn portfolio(&self, p: Pubkey) -> PortfolioAccountV16 {
        state::read_portfolio(&self.svm.get_account(&p).unwrap().data).unwrap()
    }
    fn token_amount(&self, key: Pubkey) -> u64 {
        TokenAccount::unpack(&self.svm.get_account(&key).unwrap().data)
            .unwrap()
            .amount
    }
}

/// 3A.1a — the MAINNET re-key holds in the assembled 5-program instance: a real
/// Deposit moves SPL tokens into the MAINNET-derived vault and credits the
/// portfolio + group ledger in lockstep.
#[test]
fn x0_economic_spine_deposit_moves_tokens_at_mainnet() {
    let mut env = CrosscutEnv::new();
    let owner = Keypair::new();
    let portfolio = env.create_portfolio(&owner);

    let source = env.deposit(&owner, portfolio, 1_000);

    // Executed guard: real token movement into the MAINNET-derived vault.
    assert_eq!(env.token_amount(source), 0, "source token account drained");
    assert_eq!(
        env.token_amount(env.vault),
        1_000,
        "vault (authority derived under MAINNET) credited"
    );
    // Ledger lockstep: group.vault ledger == on-chain vault balance.
    assert_eq!(env.group().vault, 1_000, "group.vault ledger == on-chain vault");
    assert_eq!(env.portfolio(portfolio).capital, 1_000, "portfolio capital credited");
}
