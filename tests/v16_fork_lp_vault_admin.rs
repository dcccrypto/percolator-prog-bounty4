//! LP Vault crank + admin tests (Phase 2.B Tier 3, Workstream 4B — Phases F + G).
//!
//! F — LpVaultCrankFees (tag 78): no-new-fees reject (happy path needs bucket
//!     utilization earnings, i.e. trades → Phase 2.E assembled-system test).
//! G — SetLpVaultPaused (79) + CloseLpVault (80): pause blocks deposit, unpause
//!     restores, close-with-shares rejects, close-empty ok, non-admin rejects.
//!
//! Cross-reference: lp_vault_design.md §5.3/§5; src/v16_program.rs
//! handle_lp_vault_crank_fees / handle_set_lp_vault_paused / handle_close_lp_vault.

use litesvm::LiteSVM;
use percolator_prog::ix::Instruction as ProgInstruction;
use percolator_prog::processor::ASSET_ACTION_ACTIVATE;
use percolator_prog::state::{
    self, derive_lp_backing_ledger, derive_lp_vault_mint, derive_lp_vault_registry,
};
use solana_sdk::{
    account::Account,
    compute_budget::ComputeBudgetInstruction,
    instruction::{AccountMeta, Instruction},
    program_option::COption,
    program_pack::Pack,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    transaction::Transaction,
};
use spl_token::state::{Account as TokenAccount, AccountState, Mint};
use std::path::PathBuf;

const MAX_PORTFOLIO_ASSETS: u16 = 1;
const APPEND_ASSET_INDEX: u16 = 1;
const DOMAIN: u16 = 2;
const DEPOSIT: u128 = 1_000_000;

fn program_path() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("target/deploy/percolator_prog.so");
    assert!(p.exists(), "wrapper BPF missing");
    p
}
fn spl_token_program_path() -> PathBuf {
    let cargo_home = std::env::var_os("CARGO_HOME").map(PathBuf::from).unwrap_or_else(|| {
        let mut h = PathBuf::from(std::env::var_os("HOME").expect("HOME"));
        h.push(".cargo");
        h
    });
    for reg in std::fs::read_dir(cargo_home.join("registry/src")).expect("registry/src") {
        let c = reg.expect("e").path().join("litesvm-0.1.0/src/spl/programs/spl_token-3.5.0.so");
        if c.exists() {
            return c;
        }
    }
    panic!("spl_token BPF not found");
}
fn make_mint_data() -> Vec<u8> {
    let mut d = vec![0u8; Mint::LEN];
    Mint::pack(Mint { mint_authority: COption::None, supply: 0, decimals: 0, is_initialized: true, freeze_authority: COption::None }, &mut d).unwrap();
    d
}
fn make_token_data(mint: Pubkey, owner: Pubkey, amount: u64) -> Vec<u8> {
    let mut d = vec![0u8; TokenAccount::LEN];
    TokenAccount::pack(TokenAccount { mint, owner, amount, delegate: COption::None, state: AccountState::Initialized, is_native: COption::None, delegated_amount: 0, close_authority: COption::None }, &mut d).unwrap();
    d
}
fn set_token(svm: &mut LiteSVM, key: Pubkey, mint: Pubkey, owner: Pubkey, amount: u64) {
    svm.set_account(key, Account { lamports: 1_000_000_000, data: make_token_data(mint, owner, amount), owner: spl_token::ID, executable: false, rent_epoch: 0 }).unwrap();
}

struct Env {
    svm: LiteSVM,
    program_id: Pubkey,
    payer: Keypair,
    admin: Keypair,
    market: Pubkey,
    collateral_mint: Pubkey,
    vault_token: Pubkey,
    registry: Pubkey,
    lp_mint: Pubkey,
    ledger: Pubkey,
}

fn send(svm: &mut LiteSVM, pid: Pubkey, payer: &Keypair, ix: ProgInstruction, accounts: Vec<AccountMeta>, extra: &[&Keypair]) -> Result<(), String> {
    let instruction = Instruction { program_id: pid, accounts, data: ix.encode() };
    let mut signers = vec![payer];
    signers.extend_from_slice(extra);
    let tx = Transaction::new_signed_with_payer(
        &[ComputeBudgetInstruction::request_heap_frame(128 * 1024), ComputeBudgetInstruction::set_compute_unit_limit(1_400_000), instruction],
        Some(&payer.pubkey()), &signers, svm.latest_blockhash());
    svm.send_transaction(tx).map(|_| ()).map_err(|e| format!("{e:?}"))
}

fn init_market_ix() -> ProgInstruction {
    ProgInstruction::InitMarket {
        max_portfolio_assets: MAX_PORTFOLIO_ASSETS, h_min: 0, h_max: 10, initial_price: 100, min_nonzero_mm_req: 1, min_nonzero_im_req: 2,
        maintenance_margin_bps: 10_000, initial_margin_bps: 10_000, max_trading_fee_bps: 10_000, trade_fee_base_bps: 0, liquidation_fee_bps: 0,
        liquidation_fee_cap: 0, min_liquidation_abs: 0, max_price_move_bps_per_slot: 10_000, max_accrual_dt_slots: 1, max_abs_funding_e9_per_slot: 0,
        min_funding_lifetime_slots: 1, max_account_b_settlement_chunks: 1, max_bankrupt_close_chunks: 1, max_bankrupt_close_lifetime_slots: 100,
        public_b_chunk_atoms: percolator::MAX_VAULT_TVL, maintenance_fee_per_slot: 0,
    }
}

fn setup_vault() -> Env {
    let mut svm = LiteSVM::new();
    let program_id = percolator_prog::id();
    svm.add_program(program_id, &std::fs::read(program_path()).expect("wrapper BPF"));
    svm.add_program(spl_token::ID, &std::fs::read(spl_token_program_path()).expect("token BPF"));
    let payer = Keypair::new();
    let admin = Keypair::new();
    let market = Pubkey::new_unique();
    let collateral_mint = Pubkey::new_unique();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    svm.airdrop(&admin.pubkey(), 100_000_000_000).unwrap();
    svm.set_account(collateral_mint, Account { lamports: 1_000_000_000, data: make_mint_data(), owner: spl_token::ID, executable: false, rent_epoch: 0 }).unwrap();
    svm.set_account(market, Account { lamports: 1_000_000_000, data: vec![0u8; state::market_account_len_for_capacity(MAX_PORTFOLIO_ASSETS as usize).unwrap()], owner: program_id, executable: false, rent_epoch: 0 }).unwrap();
    send(&mut svm, program_id, &payer, init_market_ix(), vec![AccountMeta::new(admin.pubkey(), true), AccountMeta::new(market, false), AccountMeta::new_readonly(collateral_mint, false)], &[&admin]).expect("init market");

    let (registry, _) = derive_lp_vault_registry(&program_id, &market);
    let (lp_mint, _) = derive_lp_vault_mint(&program_id, &market);
    let (ledger, _) = derive_lp_backing_ledger(&program_id, &market, DOMAIN);
    let (vault_authority, _) = Pubkey::find_program_address(&[b"vault", market.as_ref()], &program_id);
    let vault_token = canonical_vault_ata(&vault_authority, &collateral_mint);
    set_token(&mut svm, vault_token, collateral_mint, vault_authority, 0);

    send(&mut svm, program_id, &payer, ProgInstruction::UpdateAssetLifecycle {
        action: ASSET_ACTION_ACTIVATE, asset_index: APPEND_ASSET_INDEX, now_slot: 1, initial_price: 100,
        insurance_authority: admin.pubkey().to_bytes(), insurance_operator: admin.pubkey().to_bytes(),
        backing_bucket_authority: registry.to_bytes(), oracle_authority: admin.pubkey().to_bytes(),
    }, vec![AccountMeta::new(admin.pubkey(), true), AccountMeta::new(market, false)], &[&admin]).expect("append asset 1");

    send(&mut svm, program_id, &payer, ProgInstruction::CreateLpVault { fee_share_bps: 5_000, redemption_cooldown_slots: 0, oi_reservation_threshold_bps: 0, domain: DOMAIN },
        vec![AccountMeta::new(admin.pubkey(), true), AccountMeta::new_readonly(market, false), AccountMeta::new(registry, false), AccountMeta::new(lp_mint, false), AccountMeta::new_readonly(solana_sdk::system_program::ID, false), AccountMeta::new_readonly(spl_token::ID, false)], &[&admin]).expect("create lp vault");

    Env { svm, program_id, payer, admin, market, collateral_mint, vault_token, registry, lp_mint, ledger }
}

/// Make a depositor + deposit `amount`. Returns (kp, lp_ata, source).
fn deposit(env: &mut Env, amount: u128) -> (Keypair, Pubkey, Pubkey) {
    let kp = Keypair::new();
    env.svm.airdrop(&kp.pubkey(), 100_000_000_000).unwrap();
    let source = Pubkey::new_unique();
    set_token(&mut env.svm, source, env.collateral_mint, kp.pubkey(), 10_000_000);
    let lp_ata = Pubkey::new_unique();
    set_token(&mut env.svm, lp_ata, env.lp_mint, kp.pubkey(), 0);
    let pid = env.program_id;
    let payer = env.payer.insecure_clone();
    let accts = vec![
        AccountMeta::new(kp.pubkey(), true), AccountMeta::new(env.market, false), AccountMeta::new(env.registry, false),
        AccountMeta::new(env.lp_mint, false), AccountMeta::new(lp_ata, false), AccountMeta::new(source, false),
        AccountMeta::new(env.vault_token, false), AccountMeta::new(env.ledger, false),
        AccountMeta::new_readonly(spl_token::ID, false), AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
    ];
    send(&mut env.svm, pid, &payer, ProgInstruction::DepositToLpVault { amount }, accts, &[&kp]).expect("deposit");
    (kp, lp_ata, source)
}

fn try_deposit(env: &mut Env, kp: &Keypair, lp_ata: Pubkey, source: Pubkey, amount: u128) -> Result<(), String> {
    let pid = env.program_id;
    let payer = env.payer.insecure_clone();
    let accts = vec![
        AccountMeta::new(kp.pubkey(), true), AccountMeta::new(env.market, false), AccountMeta::new(env.registry, false),
        AccountMeta::new(env.lp_mint, false), AccountMeta::new(lp_ata, false), AccountMeta::new(source, false),
        AccountMeta::new(env.vault_token, false), AccountMeta::new(env.ledger, false),
        AccountMeta::new_readonly(spl_token::ID, false), AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
    ];
    send(&mut env.svm, pid, &payer, ProgInstruction::DepositToLpVault { amount }, accts, &[kp])
}

fn set_paused(env: &mut Env, signer: &Keypair, paused: u8) -> Result<(), String> {
    let pid = env.program_id;
    let payer = env.payer.insecure_clone();
    send(&mut env.svm, pid, &payer, ProgInstruction::SetLpVaultPaused { paused },
        vec![AccountMeta::new(signer.pubkey(), true), AccountMeta::new_readonly(env.market, false), AccountMeta::new(env.registry, false)], &[signer])
}

fn close_vault(env: &mut Env, signer: &Keypair) -> Result<(), String> {
    let pid = env.program_id;
    let payer = env.payer.insecure_clone();
    send(&mut env.svm, pid, &payer, ProgInstruction::CloseLpVault,
        vec![AccountMeta::new(signer.pubkey(), true), AccountMeta::new_readonly(env.market, false), AccountMeta::new(env.registry, false), AccountMeta::new_readonly(env.lp_mint, false)], &[signer])
}

fn crank_fees(env: &mut Env) -> Result<(), String> {
    let pid = env.program_id;
    let payer = env.payer.insecure_clone();
    send(&mut env.svm, pid, &payer, ProgInstruction::LpVaultCrankFees,
        vec![AccountMeta::new(payer.pubkey(), true), AccountMeta::new(env.market, false), AccountMeta::new(env.registry, false), AccountMeta::new(env.ledger, false)], &[])
}

// ── F: CrankFees ────────────────────────────────────────────────────

#[test]
fn crank_fees_no_new_fees_rejects() {
    // No trades → bucket utilization_fee_earnings == 0 → total_earnings == 0 ==
    // snapshot → delta 0 → LpVaultNoFeesToCrank. (Happy path needs bucket
    // earnings from trades → Phase 2.E assembled-system test.)
    let mut env = setup_vault();
    let _ = deposit(&mut env, DEPOSIT); // creates the backing ledger
    env.svm.expire_blockhash();
    let res = crank_fees(&mut env);
    assert!(res.is_err(), "crank with no new fees must reject: {res:?}");
}

// ── G: Pause / Unpause / Close ──────────────────────────────────────

#[test]
fn pause_blocks_deposit_unpause_restores() {
    let mut env = setup_vault();
    let admin = env.admin.insecure_clone();
    // Pause.
    set_paused(&mut env, &admin, 1).expect("pause");
    let kp = Keypair::new();
    env.svm.airdrop(&kp.pubkey(), 100_000_000_000).unwrap();
    let source = Pubkey::new_unique();
    set_token(&mut env.svm, source, env.collateral_mint, kp.pubkey(), 10_000_000);
    let lp_ata = Pubkey::new_unique();
    set_token(&mut env.svm, lp_ata, env.lp_mint, kp.pubkey(), 0);
    let res = try_deposit(&mut env, &kp, lp_ata, source, DEPOSIT);
    assert!(res.is_err(), "deposit while paused must reject (LpVaultPaused): {res:?}");

    // Unpause → deposit ok.
    env.svm.expire_blockhash();
    set_paused(&mut env, &admin, 0).expect("unpause");
    env.svm.expire_blockhash();
    try_deposit(&mut env, &kp, lp_ata, source, DEPOSIT).expect("deposit after unpause");
}

#[test]
fn set_paused_rejects_non_admin() {
    let mut env = setup_vault();
    let attacker = Keypair::new();
    env.svm.airdrop(&attacker.pubkey(), 100_000_000_000).unwrap();
    let res = set_paused(&mut env, &attacker, 1);
    assert!(res.is_err(), "non-admin pause must reject: {res:?}");
}

#[test]
fn close_with_shares_outstanding_rejects() {
    let mut env = setup_vault();
    let _ = deposit(&mut env, DEPOSIT); // outstanding shares > 0
    env.svm.expire_blockhash();
    let admin = env.admin.insecure_clone();
    let res = close_vault(&mut env, &admin);
    assert!(res.is_err(), "close with outstanding shares must reject: {res:?}");
}

#[test]
fn close_empty_vault_ok() {
    // Fresh vault, no deposits → outstanding 0, mint supply 0 → close ok.
    let mut env = setup_vault();
    let admin = env.admin.insecure_clone();
    let lamports_before = env.svm.get_account(&admin.pubkey()).unwrap().lamports;
    close_vault(&mut env, &admin).expect("close empty vault");
    // Registry data zeroed → unreadable.
    let acct = env.svm.get_account(&env.registry).unwrap();
    assert!(state::read_lp_vault_registry(&acct.data).is_err(), "registry consumed");
    // Rent reclaimed to admin.
    let lamports_after = env.svm.get_account(&admin.pubkey()).unwrap().lamports;
    assert!(lamports_after >= lamports_before, "rent reclaimed to admin");
}

#[test]
fn close_rejects_non_admin() {
    let mut env = setup_vault();
    let attacker = Keypair::new();
    env.svm.airdrop(&attacker.pubkey(), 100_000_000_000).unwrap();
    let res = close_vault(&mut env, &attacker);
    assert!(res.is_err(), "non-admin close must reject: {res:?}");
}

// ── Phase 2.E deferred LP test #2: LpVaultCrankFees happy path ──────────────
/// Seed accrued utilization-fee earnings into the domain backing bucket — the
/// EXACT field that real backing trades write and that `sync_backing_domain_ledger`
/// reads (v16_program.rs:7918). `group.vault` is bumped by the same amount so the
/// seeded market stays solvent (senior <= vault). This isolates the crank's
/// distribution bookkeeping (the operative thing under test) from the orthogonal
/// trade-fee-accrual machinery.
fn seed_bucket_earnings(env: &mut Env, earnings: u128) {
    let mut acct = env.svm.get_account(&env.market).expect("market");
    let (cfg, mut group) = state::read_market(&acct.data).expect("read market");
    group.source_backing_buckets[DOMAIN as usize].utilization_fee_earnings += earnings;
    group.vault += earnings;
    state::write_market(&mut acct.data, &cfg, &group).expect("write market");
    env.svm.set_account(env.market, acct).unwrap();
}

fn registry(env: &Env) -> state::LpVaultRegistryV16 {
    state::read_lp_vault_registry(&env.svm.get_account(&env.registry).unwrap().data).unwrap()
}

fn ledger_total_earnings(env: &Env) -> u128 {
    state::read_backing_domain_ledger(&env.svm.get_account(&env.ledger).unwrap().data)
        .unwrap()
        .total_earnings_atoms
}

#[test]
fn crank_fees_distributes_lp_share_after_earnings() {
    let mut env = setup_vault();
    let _ = deposit(&mut env, DEPOSIT); // creates the backing ledger + outstanding shares

    let earnings: u128 = 1_000_000;
    seed_bucket_earnings(&mut env, earnings);

    let reg_before = registry(&env);
    let te_before = ledger_total_earnings(&env);
    env.svm.expire_blockhash();
    crank_fees(&mut env).expect("crank with accrued earnings must succeed");
    let reg_after = registry(&env);
    let te_after = ledger_total_earnings(&env);

    // GAP-CRANKFEES (Phase 3A.2 belt-and-braces): exact sync + formula-derived split.
    // NOTE on the seed: the fee SOURCE here is `seed_bucket_earnings` — the SAME
    // accepted direct-write boundary `tests/v16_cu.rs:4876` uses to seed
    // `utilization_fee_earnings`. The design doc's "real TradeNoCpi fee" recipe is
    // mis-modeled: the backing/provider fee accrues on the increase in
    // `source_lien_counterparty_backing_num` (a counterparty-backed LOSS
    // reservation, v16_program.rs:11643-11683), NOT on positive PnL — so a real
    // fee needs a sub-100%-margin loss-backed trade, requiring an Env-margin change
    // that would perturb this file's other tests. Deferred (PENDING_DECISIONS);
    // here we harden the crank's DISTRIBUTION bookkeeping against the seeded input.

    // total_earnings_atoms grew by EXACTLY the accrued earnings (was a `>=` bound).
    assert_eq!(
        te_after - te_before,
        earnings,
        "total_earnings_atoms grew by EXACTLY the accrued earnings: {te_before} -> {te_after}"
    );
    // The snapshot advances to the current total_earnings.
    assert_eq!(reg_after.insurance_fee_snapshot_atoms, te_after, "fee snapshot must advance to current total_earnings");
    // LP distribution == lp_fee_split(delta, fee_share_bps).0 = floor(delta * bps / 10_000)
    // (percolator/src/v16.rs:21008), fee_share_bps = 5_000 from setup_vault's CreateLpVault —
    // the REAL engine formula, not a hard-coded /2 constant.
    const CRANK_FEE_SHARE_BPS: u128 = 5_000;
    let expected_lp_side = earnings * CRANK_FEE_SHARE_BPS / 10_000;
    let grew = reg_after.fee_distribution_total_atoms - reg_before.fee_distribution_total_atoms;
    assert!(grew > 0, "LP fee distribution must grow after a fee crank");
    assert_eq!(grew, expected_lp_side, "lp_side must equal lp_fee_split(earnings, fee_share_bps).0");
}

// W3 (canonical-ATA): mirror of v16_program::processor::canonical_vault_address — the SPL
// Associated Token Account of the vault_authority PDA for this mint. Kept byte-in-lock-step with
// the program so vault fixtures satisfy the F-VAULT-FRAG pin (a green test == the derivation matches).
fn canonical_vault_ata(vault_authority: &Pubkey, mint: &Pubkey) -> Pubkey {
    let ata_program: Pubkey = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL".parse().unwrap();
    Pubkey::find_program_address(
        &[vault_authority.as_ref(), spl_token::ID.as_ref(), mint.as_ref()],
        &ata_program,
    )
    .0
}
