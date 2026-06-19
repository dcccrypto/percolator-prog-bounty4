// Skip this integration-test binary when Kani builds the test suite.
#![cfg(not(kani))]
//! LP Vault redeem-flow integration tests (Phase 2.B Tier 3, Workstream 4B — Phase E).
//!
//! RequestRedeemLpShares (tag 76) + ExecuteRedemption (tag 77), B-7 cooldown.
//!
//! HEADLINE invariants (per the pass directive):
//!   - DOUBLE-EXECUTE REPLAY GUARD (data-keyed on the zeroed magic, not lamports):
//!       * `execute_twice_two_tx_second_rejects` — request→execute→execute MUST reject.
//!       * `execute_twice_same_tx_second_rejects` — two ExecuteRedemption ix in ONE
//!         tx → the tx fails (2nd ix rejects on the zeroed magic). This is where a
//!         naive lamport-only close would break.
//!   - I12 escrow == Σ outstanding shares: `multi_redeemer_each_gets_pro_rata` — two
//!     concurrent pending redemptions, execute both, each gets its pro-rata, escrow
//!     zeroes out.
//!   - DIFFERENTIAL `execute_redemption_backing_state_matches_withdraw`.
//!
//! Cross-reference: lp_vault_design.md §5.6; src/v16_program.rs
//! handle_request_redeem_lp_shares / handle_execute_redemption (mirrors
//! handle_withdraw_backing_bucket).

use litesvm::LiteSVM;
use percolator::MarketModeV16;
use percolator_prog::ix::Instruction as ProgInstruction;
use percolator_prog::processor::ASSET_ACTION_ACTIVATE;
use percolator_prog::state::{
    self, derive_lp_backing_ledger, derive_lp_escrow, derive_lp_redemption, derive_lp_vault_mint,
    derive_lp_vault_registry,
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
    assert!(p.exists(), "wrapper BPF missing — cargo build-sbf --no-default-features");
    p
}

fn spl_token_program_path() -> PathBuf {
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let mut h = PathBuf::from(std::env::var_os("HOME").expect("HOME"));
            h.push(".cargo");
            h
        });
    for reg in std::fs::read_dir(cargo_home.join("registry/src")).expect("registry/src") {
        let cand = reg.expect("entry").path().join("litesvm-0.1.0/src/spl/programs/spl_token-3.5.0.so");
        if cand.exists() {
            return cand;
        }
    }
    panic!("spl_token BPF not found");
}

fn make_mint_data() -> Vec<u8> {
    let mut d = vec![0u8; Mint::LEN];
    Mint::pack(
        Mint {
            mint_authority: COption::None,
            supply: 0,
            decimals: 0,
            is_initialized: true,
            freeze_authority: COption::None,
        },
        &mut d,
    )
    .unwrap();
    d
}

fn make_token_data(mint: Pubkey, owner: Pubkey, amount: u64) -> Vec<u8> {
    let mut d = vec![0u8; TokenAccount::LEN];
    TokenAccount::pack(
        TokenAccount {
            mint,
            owner,
            amount,
            delegate: COption::None,
            state: AccountState::Initialized,
            is_native: COption::None,
            delegated_amount: 0,
            close_authority: COption::None,
        },
        &mut d,
    )
    .unwrap();
    d
}

fn set_token(svm: &mut LiteSVM, key: Pubkey, mint: Pubkey, owner: Pubkey, amount: u64) {
    svm.set_account(
        key,
        Account { lamports: 1_000_000_000, data: make_token_data(mint, owner, amount), owner: spl_token::ID, executable: false, rent_epoch: 0 },
    )
    .unwrap();
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
    escrow: Pubkey,
    vault_authority: Pubkey,
}

fn send(svm: &mut LiteSVM, program_id: Pubkey, payer: &Keypair, ixs: Vec<(ProgInstruction, Vec<AccountMeta>)>, extra: &[&Keypair]) -> Result<(), String> {
    let mut instructions = vec![
        ComputeBudgetInstruction::request_heap_frame(128 * 1024),
        ComputeBudgetInstruction::set_compute_unit_limit(1_400_000),
    ];
    for (ix, accounts) in ixs {
        instructions.push(Instruction { program_id, accounts, data: ix.encode() });
    }
    let mut signers = vec![payer];
    signers.extend_from_slice(extra);
    let tx = Transaction::new_signed_with_payer(&instructions, Some(&payer.pubkey()), &signers, svm.latest_blockhash());
    svm.send_transaction(tx).map(|_| ()).map_err(|e| format!("{e:?}"))
}

fn init_market_ix() -> ProgInstruction {
    ProgInstruction::InitMarket {
        max_portfolio_assets: MAX_PORTFOLIO_ASSETS, h_min: 0, h_max: 10, initial_price: 100,
        min_nonzero_mm_req: 1, min_nonzero_im_req: 2, maintenance_margin_bps: 10_000,
        initial_margin_bps: 10_000, max_trading_fee_bps: 10_000, trade_fee_base_bps: 0,
        liquidation_fee_bps: 0, liquidation_fee_cap: 0, min_liquidation_abs: 0,
        max_price_move_bps_per_slot: 10_000, max_accrual_dt_slots: 1, max_abs_funding_e9_per_slot: 0,
        min_funding_lifetime_slots: 1, max_account_b_settlement_chunks: 1, max_bankrupt_close_chunks: 1,
        max_bankrupt_close_lifetime_slots: 100, public_b_chunk_atoms: percolator::MAX_VAULT_TVL,
        maintenance_fee_per_slot: 0,
    }
}

/// Build a vault on domain 2 (asset 1) with backing authority = registry, and a
/// given redemption cooldown. Returns Env.
fn setup_vault(cooldown_slots: u64) -> Env {
    setup_vault_oi(cooldown_slots, 0)
}

/// As `setup_vault` but with an explicit OI-reservation threshold (bps). A
/// non-zero threshold arms the I6 guard at ExecuteRedemption (Phase 2.E).
fn setup_vault_oi(cooldown_slots: u64, oi_reservation_threshold_bps: u16) -> Env {
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

    send(&mut svm, program_id, &payer, vec![(init_market_ix(), vec![
        AccountMeta::new(admin.pubkey(), true),
        AccountMeta::new(market, false),
        AccountMeta::new_readonly(collateral_mint, false),
    ])], &[&admin]).expect("init market");

    let (registry, _) = derive_lp_vault_registry(&program_id, &market);
    let (lp_mint, _) = derive_lp_vault_mint(&program_id, &market);
    let (ledger, _) = derive_lp_backing_ledger(&program_id, &market, DOMAIN);
    let (escrow, _) = derive_lp_escrow(&program_id, &market);
    let (vault_authority, _) = Pubkey::find_program_address(&[b"vault", market.as_ref()], &program_id);
    let vault_token = canonical_vault_ata(&vault_authority, &collateral_mint);
    set_token(&mut svm, vault_token, collateral_mint, vault_authority, 0);

    // Append asset 1 with registry as backing authority, then create vault.
    send(&mut svm, program_id, &payer, vec![(ProgInstruction::UpdateAssetLifecycle {
        action: ASSET_ACTION_ACTIVATE, asset_index: APPEND_ASSET_INDEX, now_slot: 1, initial_price: 100,
        insurance_authority: admin.pubkey().to_bytes(), insurance_operator: admin.pubkey().to_bytes(),
        backing_bucket_authority: registry.to_bytes(), oracle_authority: admin.pubkey().to_bytes(),
    }, vec![AccountMeta::new(admin.pubkey(), true), AccountMeta::new(market, false)])], &[&admin]).expect("append asset 1");

    send(&mut svm, program_id, &payer, vec![(ProgInstruction::CreateLpVault {
        fee_share_bps: 5_000, redemption_cooldown_slots: cooldown_slots, oi_reservation_threshold_bps, domain: DOMAIN,
    }, vec![
        AccountMeta::new(admin.pubkey(), true),
        AccountMeta::new_readonly(market, false),
        AccountMeta::new(registry, false),
        AccountMeta::new(lp_mint, false),
        AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        AccountMeta::new_readonly(spl_token::ID, false),
    ])], &[&admin]).expect("create lp vault");

    Env { svm, program_id, payer, admin, market, collateral_mint, vault_token, registry, lp_mint, ledger, escrow, vault_authority }
}

/// A depositor with a funded source + LP ATA who has deposited `amount`.
struct Depositor {
    kp: Keypair,
    source: Pubkey,
    lp_ata: Pubkey,
    dest: Pubkey, // collateral payout destination
    redemption: Pubkey,
}

fn deposit_accounts(env: &Env, lp_ata: Pubkey, source: Pubkey, depositor: Pubkey) -> Vec<AccountMeta> {
    vec![
        AccountMeta::new(depositor, true),
        AccountMeta::new(env.market, false),
        AccountMeta::new(env.registry, false),
        AccountMeta::new(env.lp_mint, false),
        AccountMeta::new(lp_ata, false),
        AccountMeta::new(source, false),
        AccountMeta::new(env.vault_token, false),
        AccountMeta::new(env.ledger, false),
        AccountMeta::new_readonly(spl_token::ID, false),
        AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
    ]
}

fn new_depositor(env: &mut Env, amount: u128) -> Depositor {
    let kp = Keypair::new();
    env.svm.airdrop(&kp.pubkey(), 100_000_000_000).unwrap();
    let source = Pubkey::new_unique();
    set_token(&mut env.svm, source, env.collateral_mint, kp.pubkey(), 10_000_000);
    let lp_ata = Pubkey::new_unique();
    set_token(&mut env.svm, lp_ata, env.lp_mint, kp.pubkey(), 0);
    let dest = Pubkey::new_unique();
    set_token(&mut env.svm, dest, env.collateral_mint, kp.pubkey(), 0);
    let (redemption, _) = derive_lp_redemption(&env.program_id, &env.registry, &kp.pubkey());

    let market = env.market;
    let pid = env.program_id;
    let payer = env.payer.insecure_clone();
    let accts = deposit_accounts(env, lp_ata, source, kp.pubkey());
    send(&mut env.svm, pid, &payer, vec![(ProgInstruction::DepositToLpVault { amount }, accts)], &[&kp]).expect("deposit");
    let _ = market;
    Depositor { kp, source, lp_ata, dest, redemption }
}

fn request_accounts(env: &Env, d: &Depositor) -> Vec<AccountMeta> {
    vec![
        AccountMeta::new(d.kp.pubkey(), true),
        AccountMeta::new(env.registry, false),
        AccountMeta::new(env.lp_mint, false),
        AccountMeta::new(d.lp_ata, false),
        AccountMeta::new(env.escrow, false),
        AccountMeta::new(d.redemption, false),
        AccountMeta::new_readonly(spl_token::ID, false),
        AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
    ]
}

fn execute_accounts(env: &Env, d: &Depositor) -> Vec<AccountMeta> {
    vec![
        AccountMeta::new(env.payer.pubkey(), true),
        AccountMeta::new(env.market, false),
        AccountMeta::new(env.registry, false),
        AccountMeta::new(d.redemption, false),
        AccountMeta::new(env.lp_mint, false),
        AccountMeta::new(env.escrow, false),
        AccountMeta::new(env.vault_token, false),
        AccountMeta::new_readonly(env.vault_authority, false),
        AccountMeta::new(env.ledger, false),
        AccountMeta::new(d.dest, false),
        AccountMeta::new_readonly(spl_token::ID, false),
    ]
}

fn tok(svm: &LiteSVM, key: Pubkey) -> u64 {
    TokenAccount::unpack(&svm.get_account(&key).expect("acct").data).expect("decode").amount
}

fn request(env: &mut Env, d: &Depositor, lp_amount: u128) -> Result<(), String> {
    let pid = env.program_id;
    let payer = env.payer.insecure_clone();
    let kp = d.kp.insecure_clone();
    let accts = request_accounts(env, d);
    // v17 CHANGE: field renamed from lp_amount → shares.
    send(&mut env.svm, pid, &payer, vec![(ProgInstruction::RequestRedeemLpShares { shares: lp_amount }, accts)], &[&kp])
}

fn execute(env: &mut Env, d: &Depositor) -> Result<(), String> {
    let pid = env.program_id;
    let payer = env.payer.insecure_clone();
    let accts = execute_accounts(env, d);
    send(&mut env.svm, pid, &payer, vec![(ProgInstruction::ExecuteRedemption, accts)], &[])
}

fn resolve_market(env: &mut Env) -> Result<(), String> {
    let pid = env.program_id;
    let payer = env.payer.insecure_clone();
    let admin = env.admin.insecure_clone();
    send(
        &mut env.svm,
        pid,
        &payer,
        vec![(
            ProgInstruction::ResolveMarket,
            vec![
                AccountMeta::new(admin.pubkey(), true),
                AccountMeta::new(env.market, false),
            ],
        )],
        &[&admin],
    )
}

fn close_lp_vault(env: &mut Env) -> Result<(), String> {
    let pid = env.program_id;
    let payer = env.payer.insecure_clone();
    let admin = env.admin.insecure_clone();
    send(
        &mut env.svm,
        pid,
        &payer,
        vec![(
            ProgInstruction::CloseLpVault,
            vec![
                AccountMeta::new(admin.pubkey(), true),
                AccountMeta::new_readonly(env.market, false),
                AccountMeta::new(env.registry, false),
                AccountMeta::new_readonly(env.lp_mint, false),
            ],
        )],
        &[&admin],
    )
}

fn close_slab(env: &mut Env, dest: Pubkey) -> Result<(), String> {
    let pid = env.program_id;
    let payer = env.payer.insecure_clone();
    let admin = env.admin.insecure_clone();
    send(
        &mut env.svm,
        pid,
        &payer,
        vec![(
            ProgInstruction::CloseSlab,
            vec![
                AccountMeta::new(admin.pubkey(), true),
                AccountMeta::new(env.market, false),
                AccountMeta::new(env.vault_token, false),
                AccountMeta::new_readonly(env.vault_authority, false),
                AccountMeta::new(dest, false),
                AccountMeta::new_readonly(spl_token::ID, false),
            ],
        )],
        &[&admin],
    )
}

#[test]
fn request_then_execute_pays_pro_rata() {
    let mut env = setup_vault(0); // immediate
    let d = new_depositor(&mut env, DEPOSIT);
    assert_eq!(tok(&env.svm, d.lp_ata), DEPOSIT as u64);

    request(&mut env, &d, DEPOSIT).expect("request");
    // Shares moved to escrow; redeemer's LP ATA drained.
    assert_eq!(tok(&env.svm, d.lp_ata), 0);
    assert_eq!(tok(&env.svm, env.escrow), DEPOSIT as u64);

    env.svm.expire_blockhash();
    execute(&mut env, &d).expect("execute");
    // Redeemer paid 1:1 (no earnings); escrow burned to 0; vault drained.
    assert_eq!(tok(&env.svm, d.dest), DEPOSIT as u64, "redeemer paid pro-rata");
    assert_eq!(tok(&env.svm, env.escrow), 0, "escrow burned to zero");
    assert_eq!(tok(&env.svm, env.vault_token), 0, "vault drained");
    // Registry outstanding back to 0.
    let reg = state::read_lp_vault_registry(&env.svm.get_account(&env.registry).unwrap().data).unwrap();
    assert_eq!(reg.total_lp_shares_outstanding, 0);
    // Redemption PDA consumed (magic zeroed) → unreadable.
    assert!(state::read_lp_redemption(&env.svm.get_account(&d.redemption).unwrap().data).is_err(),
        "redemption PDA consumed");
}

#[test]
fn execute_before_cooldown_rejects() {
    let mut env = setup_vault(1000); // long cooldown
    let d = new_depositor(&mut env, DEPOSIT);
    request(&mut env, &d, DEPOSIT).expect("request");
    env.svm.expire_blockhash();
    let res = execute(&mut env, &d);
    assert!(res.is_err(), "execute before cooldown must reject (I5): {res:?}");
    assert_eq!(tok(&env.svm, d.dest), 0, "no payout before cooldown");
}

#[test]
fn execute_twice_two_tx_second_rejects() {
    // HEADLINE replay guard (two-tx): request → execute → execute MUST reject, no second payout.
    let mut env = setup_vault(0);
    let d = new_depositor(&mut env, DEPOSIT);
    request(&mut env, &d, DEPOSIT).expect("request");
    env.svm.expire_blockhash();
    execute(&mut env, &d).expect("first execute");
    assert_eq!(tok(&env.svm, d.dest), DEPOSIT as u64);

    env.svm.expire_blockhash();
    let res = execute(&mut env, &d);
    assert!(res.is_err(), "second execute (2nd tx) MUST reject — replay guard: {res:?}");
    assert_eq!(tok(&env.svm, d.dest), DEPOSIT as u64, "no second payout");
}

#[test]
fn execute_twice_same_tx_second_rejects() {
    // HEADLINE replay guard (same-tx): two ExecuteRedemption ix in ONE tx. The
    // 2nd reads the magic the 1st zeroed → rejects → whole tx fails atomically.
    // A naive lamport-only close would let a re-funded 2nd execute double-pay;
    // the data-keyed (magic) guard prevents it.
    let mut env = setup_vault(0);
    let d = new_depositor(&mut env, DEPOSIT);
    request(&mut env, &d, DEPOSIT).expect("request");
    env.svm.expire_blockhash();

    let pid = env.program_id;
    let payer = env.payer.insecure_clone();
    let a1 = execute_accounts(&env, &d);
    let a2 = execute_accounts(&env, &d);
    let res = send(&mut env.svm, pid, &payer, vec![
        (ProgInstruction::ExecuteRedemption, a1),
        (ProgInstruction::ExecuteRedemption, a2),
    ], &[]);
    assert!(res.is_err(), "two ExecuteRedemption in one tx MUST fail (2nd rejects on zeroed magic): {res:?}");
    // Atomic rollback: no payout, redemption PDA intact.
    assert_eq!(tok(&env.svm, d.dest), 0, "no payout — tx rolled back");
    assert!(state::read_lp_redemption(&env.svm.get_account(&d.redemption).unwrap().data).is_ok(),
        "redemption PDA intact after rolled-back tx");
}

#[test]
fn multi_redeemer_each_gets_pro_rata() {
    // I12: escrow == Σ outstanding shares. Two depositors, two concurrent
    // pending redemptions in the shared escrow. Execute both; each gets exactly
    // its pro-rata; escrow zeroes out.
    let mut env = setup_vault(0);
    let d1 = new_depositor(&mut env, DEPOSIT); // 1_000_000 shares
    let d2 = new_depositor(&mut env, 2 * DEPOSIT); // 2_000_000 shares (1:1, no earnings)

    request(&mut env, &d1, DEPOSIT).expect("request d1");
    request(&mut env, &d2, 2 * DEPOSIT).expect("request d2");
    // I12: escrow holds the sum of both pending redemptions.
    assert_eq!(tok(&env.svm, env.escrow), (DEPOSIT + 2 * DEPOSIT) as u64, "escrow == Σ pending shares");

    env.svm.expire_blockhash();
    execute(&mut env, &d1).expect("execute d1");
    env.svm.expire_blockhash();
    execute(&mut env, &d2).expect("execute d2");

    // 1:1, no earnings → each redeems exactly what they deposited.
    assert_eq!(tok(&env.svm, d1.dest), DEPOSIT as u64, "d1 pro-rata");
    assert_eq!(tok(&env.svm, d2.dest), 2 * DEPOSIT as u64, "d2 pro-rata");
    assert_eq!(tok(&env.svm, env.escrow), 0, "escrow zeroes out");
    let reg = state::read_lp_vault_registry(&env.svm.get_account(&env.registry).unwrap().data).unwrap();
    assert_eq!(reg.total_lp_shares_outstanding, 0, "all shares redeemed");
    assert_eq!(tok(&env.svm, env.vault_token), 0, "vault fully drained");
}

#[test]
fn execute_redemption_backing_state_matches_withdraw() {
    // DIFFERENTIAL drift safety-net: ExecuteRedemption's BackingDomainLedger
    // counters + market vault total match an equivalent WithdrawBackingBucket
    // (atoms) call (admin authority). Identity fields (authority/market) differ
    // across the two markets and are excluded.

    // Path A: deposit then WithdrawBackingBucket(DEPOSIT) by admin (admin is the
    // backing authority on this market's appended asset).
    let mut env_a = setup_vault_admin_authority();
    // Top up backing via admin so there is principal to withdraw, then withdraw it.
    let admin_a = env_a.admin.insecure_clone();
    let src_a = Pubkey::new_unique();
    set_token(&mut env_a.svm, src_a, env_a.collateral_mint, admin_a.pubkey(), 10_000_000);
    let ledger_a = Pubkey::new_unique();
    env_a.svm.set_account(ledger_a, Account { lamports: 1_000_000_000, data: vec![0u8; state::backing_domain_ledger_account_len()], owner: env_a.program_id, executable: false, rent_epoch: 0 }).unwrap();
    let dest_a = Pubkey::new_unique();
    set_token(&mut env_a.svm, dest_a, env_a.collateral_mint, admin_a.pubkey(), 0);
    let pid_a = env_a.program_id;
    let payer_a = env_a.payer.insecure_clone();
    send(&mut env_a.svm, pid_a, &payer_a, vec![(ProgInstruction::TopUpBackingBucket { domain: DOMAIN, amount: DEPOSIT, expiry_slot: percolator_prog::constants::LP_VAULT_BACKING_EXPIRY_SLOT },
        vec![AccountMeta::new(admin_a.pubkey(), true), AccountMeta::new(env_a.market, false), AccountMeta::new(src_a, false), AccountMeta::new(env_a.vault_token, false), AccountMeta::new_readonly(spl_token::ID, false), AccountMeta::new(ledger_a, false)])], &[&admin_a]).expect("top up");
    env_a.svm.expire_blockhash();
    send(&mut env_a.svm, pid_a, &payer_a, vec![(ProgInstruction::WithdrawBackingBucket { domain: DOMAIN, amount: DEPOSIT },
        vec![AccountMeta::new(admin_a.pubkey(), true), AccountMeta::new(env_a.market, false), AccountMeta::new(dest_a, false), AccountMeta::new(env_a.vault_token, false), AccountMeta::new_readonly(env_a.vault_authority, false), AccountMeta::new_readonly(spl_token::ID, false), AccountMeta::new(ledger_a, false)])], &[&admin_a]).expect("withdraw");
    let led_a = state::read_backing_domain_ledger(&env_a.svm.get_account(&ledger_a).unwrap().data).unwrap();
    let (_, group_a) = state::read_market(&env_a.svm.get_account(&env_a.market).unwrap().data).unwrap();

    // Path B: deposit then request+execute (registry authority).
    let mut env_b = setup_vault(0);
    let d = new_depositor(&mut env_b, DEPOSIT);
    request(&mut env_b, &d, DEPOSIT).expect("request");
    env_b.svm.expire_blockhash();
    execute(&mut env_b, &d).expect("execute");
    let led_b = state::read_backing_domain_ledger(&env_b.svm.get_account(&env_b.ledger).unwrap().data).unwrap();
    let (_, group_b) = state::read_market(&env_b.svm.get_account(&env_b.market).unwrap().data).unwrap();

    assert_eq!(led_a.total_principal_atoms, led_b.total_principal_atoms, "principal drift");
    assert_eq!(led_a.total_principal_withdrawn_atoms, led_b.total_principal_withdrawn_atoms, "withdrawn drift");
    assert_eq!(led_a.total_deposited_atoms, led_b.total_deposited_atoms, "deposited drift");
    assert_eq!(led_a.total_earnings_atoms, led_b.total_earnings_atoms);
    assert_eq!(led_a.cumulative_loss_atoms, led_b.cumulative_loss_atoms);
    assert_eq!(led_a.cumulative_recovery_atoms, led_b.cumulative_recovery_atoms);
    assert_eq!(group_a.vault, group_b.vault, "vault total drift between Withdraw and ExecuteRedemption");
}

/// Same as setup_vault(0) but the appended asset's backing authority = admin
/// (so admin can drive TopUpBackingBucket / WithdrawBackingBucket directly for
/// the differential reference path).
fn setup_vault_admin_authority() -> Env {
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
    send(&mut svm, program_id, &payer, vec![(init_market_ix(), vec![
        AccountMeta::new(admin.pubkey(), true), AccountMeta::new(market, false), AccountMeta::new_readonly(collateral_mint, false),
    ])], &[&admin]).expect("init market");
    let (registry, _) = derive_lp_vault_registry(&program_id, &market);
    let (lp_mint, _) = derive_lp_vault_mint(&program_id, &market);
    let (ledger, _) = derive_lp_backing_ledger(&program_id, &market, DOMAIN);
    let (escrow, _) = derive_lp_escrow(&program_id, &market);
    let (vault_authority, _) = Pubkey::find_program_address(&[b"vault", market.as_ref()], &program_id);
    let vault_token = canonical_vault_ata(&vault_authority, &collateral_mint);
    set_token(&mut svm, vault_token, collateral_mint, vault_authority, 0);
    send(&mut svm, program_id, &payer, vec![(ProgInstruction::UpdateAssetLifecycle {
        action: ASSET_ACTION_ACTIVATE, asset_index: APPEND_ASSET_INDEX, now_slot: 1, initial_price: 100,
        insurance_authority: admin.pubkey().to_bytes(), insurance_operator: admin.pubkey().to_bytes(),
        backing_bucket_authority: admin.pubkey().to_bytes(), oracle_authority: admin.pubkey().to_bytes(),
    }, vec![AccountMeta::new(admin.pubkey(), true), AccountMeta::new(market, false)])], &[&admin]).expect("append asset 1");
    Env { svm, program_id, payer, admin, market, collateral_mint, vault_token, registry, lp_mint, ledger, escrow, vault_authority }
}

// ── Phase 2.E deferred LP test #1: OI-reservation reject (I6) ───────────────
/// A redemption that would leave the vault's outstanding backing uncovered by
/// (nav_post * oi_reservation_threshold_bps) is rejected at
/// LpVaultOiReservationViolated (Custom 37); guard at v16_program.rs:5990-6013.
/// The accept path (threshold = 0, guard disabled) is `request_then_execute_pays_pro_rata`.
#[test]
fn execute_redemption_oi_reservation_violation_rejects() {
    let mut env = setup_vault_oi(0, 5_000); // 50% OI reservation, immediate cooldown
    let d = new_depositor(&mut env, DEPOSIT);
    // Redeem a fraction so backing remains outstanding; with a 50% reservation the
    // post-redemption NAV cannot cover the still-reserved backing -> reject.
    request(&mut env, &d, DEPOSIT / 4).expect("request");
    env.svm.expire_blockhash();
    let res = execute(&mut env, &d);
    let msg = format!("{res:?}");
    assert!(msg.contains("Custom(37)"), "expected LpVaultOiReservationViolated Custom(37), got: {msg}");
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

// ── W3 (F-VAULT-FRAG) canonical-ATA pin on the LP-vault collateral paths ──
// DepositToLpVault and ExecuteRedemption both verify the collateral vault is the canonical ATA of
// the vault_authority for the mint. A vault_authority-owned account at any other address is rejected
// with InvalidVaultAccount (Custom 12), so a fragmenting second vault cannot strand LP redemptions.
// The dual-gate ACCEPT path (a real deposit -> request -> execute that pays pro-rata and DRAINS the
// canonical env.vault_token) is proven by request_then_execute_pays_pro_rata above.

fn noncanonical_vault(env: &mut Env, amount: u64) -> Pubkey {
    let bad = Pubkey::new_unique();
    set_token(&mut env.svm, bad, env.collateral_mint, env.vault_authority, amount);
    bad
}

#[test]
fn deposit_to_lp_vault_rejects_noncanonical_vault() {
    let mut env = setup_vault(0);
    let kp = Keypair::new();
    env.svm.airdrop(&kp.pubkey(), 100_000_000_000).unwrap();
    let source = Pubkey::new_unique();
    set_token(&mut env.svm, source, env.collateral_mint, kp.pubkey(), 10_000_000);
    let lp_ata = Pubkey::new_unique();
    set_token(&mut env.svm, lp_ata, env.lp_mint, kp.pubkey(), 0);
    let bad_vault = noncanonical_vault(&mut env, 0);

    let mut accts = deposit_accounts(&env, lp_ata, source, kp.pubkey());
    accts[6] = AccountMeta::new(bad_vault, false); // swap the canonical vault for a fragmenting one
    let pid = env.program_id;
    let payer = env.payer.insecure_clone();
    let res = send(&mut env.svm, pid, &payer, vec![(ProgInstruction::DepositToLpVault { amount: DEPOSIT }, accts)], &[&kp]);
    let msg = res.expect_err("DepositToLpVault to a non-canonical vault must reject");
    assert!(msg.contains("Custom(12)"), "expected InvalidVaultAccount Custom(12), got: {msg}");

    // ACCEPT: the canonical vault deposit goes through.
    let d = new_depositor(&mut env, DEPOSIT);
    assert_eq!(tok(&env.svm, d.lp_ata), DEPOSIT as u64, "canonical-vault deposit accepted");
}

#[test]
fn execute_redemption_rejects_noncanonical_vault() {
    let mut env = setup_vault(0);
    let d = new_depositor(&mut env, DEPOSIT);
    request(&mut env, &d, DEPOSIT).expect("request");
    env.svm.expire_blockhash();

    let mut accts = execute_accounts(&env, &d);
    accts[6] = AccountMeta::new(noncanonical_vault(&mut env, DEPOSIT as u64), false); // fragmenting vault
    let pid = env.program_id;
    let payer = env.payer.insecure_clone();
    let res = send(&mut env.svm, pid, &payer, vec![(ProgInstruction::ExecuteRedemption, accts)], &[]);
    let msg = res.expect_err("ExecuteRedemption from a non-canonical vault must reject");
    assert!(msg.contains("Custom(12)"), "expected InvalidVaultAccount Custom(12), got: {msg}");

    // The rejected execute reverted; the redemption is still pending and the CANONICAL vault still
    // executes (dual-gate intact) — draining env.vault_token, as proven end-to-end above.
    env.svm.expire_blockhash();
    execute(&mut env, &d).expect("canonical-vault execute accepted");
    assert_eq!(tok(&env.svm, env.vault_token), 0, "canonical vault drained on accept");
    assert_eq!(tok(&env.svm, d.dest), DEPOSIT as u64, "redeemer paid pro-rata through the canonical vault");
}

// ── Conservation tests for the principal/earnings split (v17 bug fix) ────────
//
// These tests prove the exact accounting invariant for ExecuteRedemption when
// earnings are present.  fee_share_bps = 5_000 (50%).
//
// Setup:
//   - LP A deposits 1_000_000 atoms → 1_000_000 shares (1:1, fresh vault).
//   - LP B deposits 2_000_000 atoms → 2_000_000 shares (1:1, no earnings yet).
//   - Seed 400_000 gross earnings atoms into the domain bucket.
//   - After sync:  total_earnings_atoms = 400_000
//                  lp_earnings           = 200_000   (50%)
//                  insurance_stub        = 200_000   (stays in bucket, not LP NAV)
//                  available_principal   = 3_000_000 (no impairment)
//                  nav                   = 3_200_000
//   - total_shares = 3_000_000
//
// LP A holds 1_000_000/3_000_000 of the vault:
//   atoms_A = floor(1_000_000 * 3_200_000 / 3_000_000) = floor(3_200_000_000 / 3_000_000)
//           = 1_066_666
//   principal_A = floor(1_000_000 * 3_000_000 / 3_000_000) = 1_000_000
//   earnings_A  = 1_066_666 - 1_000_000 = 66_666
//
// After LP A redeems (gross_consumed model):
//   gross_consumed_A = ceil(66_666 * 10_000 / 5_000) = 133_332
//   net_earnings_remaining = 400_000 - 133_332 = 266_668
//   lp_earnings_remaining  = floor(266_668 * 5_000 / 10_000) = 133_334
//   available_principal    = 2_000_000
//   nav_remaining          = 2_133_334
//   total_shares_remaining = 2_000_000
//
// LP B holds all remaining shares (2_000_000) — GROSS_CONSUMED model:
//   atoms_B = floor(2_000_000 * 2_133_334 / 2_000_000) = 2_133_334
//   principal_B = 2_000_000, earnings_B = 133_334
//   gross_consumed_B = ceil(133_334 * 10_000 / 5_000) = 266_668
//
// Total paid out = 1_066_666 + 2_133_334 = 3_200_000
// Vault seeded  = 3_000_000 (deposits) + 400_000 (earnings) = 3_400_000
// Insurance stub = 3_400_000 - 3_200_000 = 200_000 = 50% of 400_000 EXACTLY
//
// Fee_share split PRESERVED: LP total = 200_000 (50%), insurance = 200_000 (50%).
// (89382f1 model gave only 166_667 stub — 33_333 leaked to LPs beyond fee_share.)

/// Seeds bucket earnings by directly writing the market account — same pattern
/// as v16_fork_lp_vault_admin.rs:seed_bucket_earnings. Both `group.vault` (the
/// logical counter) and the SPL vault_token account balance are bumped so that
/// the redemption token-transfer can actually succeed.
fn seed_earnings(env: &mut Env, earnings: u128) {
    // 1. Update the market group account: bucket earnings + logical vault.
    let mut acct = env.svm.get_account(&env.market).expect("market");
    let (cfg, mut group) = state::read_market(&acct.data).expect("read market");
    group.source_backing_buckets[DOMAIN as usize].utilization_fee_earnings += earnings;
    group.vault += earnings;
    // backing_provider_earnings_total must also increase (validate_shape audit scan).
    group.backing_provider_earnings_total += earnings;
    state::write_market(&mut acct.data, &cfg, &group).expect("write market");
    env.svm.set_account(env.market, acct).unwrap();

    // 2. Bump the SPL vault_token account balance so the transfer in ExecuteRedemption
    //    can succeed. The vault token account is the real SPL token account; seeding
    //    earnings corresponds to fee revenue accruing in the backing vault.
    let earnings_u64 = u64::try_from(earnings).expect("earnings fits u64");
    let mut tok_acct = env.svm.get_account(&env.vault_token).expect("vault_token");
    let mut tok_data = TokenAccount::unpack(&tok_acct.data).expect("unpack vault token");
    tok_data.amount = tok_data.amount.checked_add(earnings_u64).expect("no overflow");
    let mut new_data = vec![0u8; TokenAccount::LEN];
    TokenAccount::pack(tok_data, &mut new_data).expect("pack");
    tok_acct.data = new_data;
    env.svm.set_account(env.vault_token, tok_acct).unwrap();
}

fn ledger(env: &Env) -> state::BackingDomainLedgerAccountV16 {
    state::read_backing_domain_ledger(&env.svm.get_account(&env.ledger).unwrap().data).unwrap()
}

fn reg(env: &Env) -> state::LpVaultRegistryV16 {
    state::read_lp_vault_registry(&env.svm.get_account(&env.registry).unwrap().data).unwrap()
}

fn vault_balance(env: &Env) -> u64 {
    tok(&env.svm, env.vault_token)
}

fn set_c_tot_for_test(env: &mut Env, c_tot: u128) {
    let mut acct = env.svm.get_account(&env.market).expect("market");
    let (cfg, mut group) = state::read_market(&acct.data).expect("read market");
    group.c_tot = c_tot;
    state::write_market(&mut acct.data, &cfg, &group).expect("write market");
    env.svm.set_account(env.market, acct).unwrap();
}

/// CONSERVATION TEST: deposit → seed_earnings → partial redeem (LP A) → full
/// redeem (LP B).  Asserts exact principal+earnings split, ledger counters,
/// remaining vault (insurance stub), and no-double-redeem.
/// SUPERSEDES 89382f1 conservation test — uses the corrected gross_consumed model.
///
/// SETUP: fee_share_bps = 5_000 (50% LP, 50% insurance).
///   Two depositors: LP A (1_000_000), LP B (2_000_000).
///   Total principal = 3_000_000. Gross earnings seeded = 400_000.
///   lp_earnings_total = floor(400_000 * 5_000 / 10_000) = 200_000.
///   NAV_total = 3_200_000.
///
/// LP A REDEEM (1_000_000 / 3_000_000 shares):
///   atoms_A = floor(1_000_000 * 3_200_000 / 3_000_000) = 1_066_666
///   principal_A = floor(1_000_000 * 3_000_000 / 3_000_000) = 1_000_000
///   earnings_A = 66_666
///   gross_consumed_A = ceil(66_666 * 10_000 / 5_000) = ceil(133_332) = 133_332
///   → net_earnings_remaining = 400_000 - 133_332 = 266_668
///   → lp_earnings_remaining  = floor(266_668 * 5_000/10_000) = 133_334
///
/// LP B REDEEM (2_000_000 / 2_000_000 shares):
///   NAV_B = 2_000_000 + 133_334 = 2_133_334
///   atoms_B = floor(2_000_000 * 2_133_334 / 2_000_000) = 2_133_334
///   principal_B = 2_000_000, earnings_B = 133_334
///   gross_consumed_B = ceil(133_334 * 10_000 / 5_000) = ceil(266_668) = 266_668
///
/// CONSERVATION:
///   total_payout = 1_066_666 + 2_133_334 = 3_200_000
///   insurance_stub = 3_400_000 - 3_200_000 = 200_000 = 50% of 400_000 EXACTLY
///   (89382f1 bug: stub was only 166_667, short by 33_333 due to fee_share leak)
///
/// FEE_SHARE SPLIT PRESERVED: LP total = 200_000 = 50% of gross earnings ✓
///                             insurance stub = 200_000 = 50% ✓
#[test]
fn conservation_with_earnings_partial_then_full_redeem() {
    let mut env = setup_vault(0); // fee_share_bps = 5_000, immediate cooldown

    // ── Setup: two depositors, then seed earnings. ──
    let d_a = new_depositor(&mut env, 1_000_000);
    let d_b = new_depositor(&mut env, 2_000_000);
    assert_eq!(vault_balance(&env), 3_000_000, "vault after deposits");
    assert_eq!(reg(&env).total_lp_shares_outstanding, 3_000_000, "shares after deposits");

    // Seed 400_000 gross earnings into domain bucket.
    seed_earnings(&mut env, 400_000);
    assert_eq!(vault_balance(&env), 3_400_000, "vault after earnings seed");

    // ── LP A redeems all 1_000_000 shares. ──
    request(&mut env, &d_a, 1_000_000).expect("A request");
    env.svm.expire_blockhash();
    execute(&mut env, &d_a).expect("A execute");

    let payout_a = tok(&env.svm, d_a.dest) as u128;
    // atoms_A = floor(1_000_000 * 3_200_000 / 3_000_000) = 1_066_666
    assert_eq!(payout_a, 1_066_666, "LP A payout = principal + earnings slice");

    // Ledger after A (gross_consumed model):
    //   principal decremented by 1_000_000
    //   total_earnings_withdrawn_atoms += gross_consumed_A = 133_332
    //     (NOT 66_666 as in 89382f1 — the gross chunk is consumed, insurance stub stays)
    let led_a = ledger(&env);
    assert_eq!(led_a.total_principal_atoms, 2_000_000, "principal remaining after A");
    assert_eq!(led_a.total_principal_withdrawn_atoms, 1_000_000, "principal withdrawn after A");
    assert_eq!(
        led_a.total_earnings_withdrawn_atoms, 133_332,
        "earnings_withdrawn = gross_consumed_A (133_332), not LP slice (66_666)"
    );
    assert_eq!(led_a.total_earnings_atoms, 400_000, "total_earnings_atoms unchanged by redemption");

    // Remaining shares: 2_000_000 (all held by LP B).
    assert_eq!(reg(&env).total_lp_shares_outstanding, 2_000_000, "shares after A redeems");

    // Vault after A: 3_400_000 - 1_066_666 = 2_333_334
    // Insurance stub for A's chunk: 133_332 - 66_666 = 66_666 atoms still in vault.
    assert_eq!(vault_balance(&env), 2_333_334, "vault balance after A redeems");

    // ── LP B redeems all 2_000_000 shares. ──
    request(&mut env, &d_b, 2_000_000).expect("B request");
    env.svm.expire_blockhash();
    execute(&mut env, &d_b).expect("B execute");

    let payout_b = tok(&env.svm, d_b.dest) as u128;
    // net_earnings_remaining = 400_000 - 133_332 = 266_668
    // lp_earnings_B = floor(266_668 * 5_000 / 10_000) = floor(133_334) = 133_334
    // NAV_B = 2_000_000 + 133_334 = 2_133_334
    // atoms_B = floor(2_000_000 * 2_133_334 / 2_000_000) = 2_133_334
    assert_eq!(payout_b, 2_133_334, "LP B payout (fee_share split preserved — not inflated by A's stub)");

    // Ledger after B.
    let led_b = ledger(&env);
    assert_eq!(led_b.total_principal_atoms, 0, "principal zero after B");
    assert_eq!(led_b.total_principal_withdrawn_atoms, 3_000_000, "total principal withdrawn");
    // gross_consumed_B = ceil(133_334 * 10_000 / 5_000) = 266_668
    // total_earnings_withdrawn_atoms = 133_332 + 266_668 = 400_000
    assert_eq!(
        led_b.total_earnings_withdrawn_atoms, 400_000,
        "total earnings withdrawn = full gross (133_332 + 266_668)"
    );

    // Outstanding shares zero.
    assert_eq!(reg(&env).total_lp_shares_outstanding, 0, "all shares redeemed");

    // ── Conservation: vault remainder = insurance stub = exactly 50% of gross earnings. ──
    // Total paid = 1_066_666 + 2_133_334 = 3_200_000
    // Total seeded = 3_400_000
    // Insurance stub = 200_000 = 50% of 400_000 = fee_share preserved exactly.
    let total_payout = payout_a + payout_b;
    assert_eq!(total_payout, 3_200_000, "total payout = principal + 50% of earnings");
    let remainder = vault_balance(&env) as u128;
    assert_eq!(remainder, 200_000, "vault remainder = insurance stub (50% of earnings)");
    assert_eq!(remainder, 3_400_000 - total_payout, "vault remainder = seeded - paid");

    // CRITICAL: insurance stub equals exactly (1 - fee_share) * gross_earnings.
    // 89382f1 model gave only 166_667 here (stub shrank by 33_333 due to fee_share leak).
    // Correct model: 50% * 400_000 = 200_000. ✓
    assert_eq!(remainder, 200_000, "fee_share split preserved: insurance stub = 50% of gross earnings");

    // ── Double-redeem guard still holds after split. ──
    env.svm.expire_blockhash();
    assert!(execute(&mut env, &d_a).is_err(), "double-redeem A must reject");
    assert!(execute(&mut env, &d_b).is_err(), "double-redeem B must reject");
}

#[test]
fn lp_shares_held_at_resolution_can_redeem_and_teardown_vault() {
    let mut env = setup_vault(0);
    let d = new_depositor(&mut env, DEPOSIT);
    assert_eq!(tok(&env.svm, d.lp_ata), DEPOSIT as u64);
    assert_eq!(vault_balance(&env), DEPOSIT as u64);

    resolve_market(&mut env).expect("resolve empty market with LP backing");

    request(&mut env, &d, DEPOSIT).expect("request still succeeds after resolution");
    assert_eq!(tok(&env.svm, d.lp_ata), 0, "shares are escrowed");
    execute(&mut env, &d).expect("terminal resolved redemption should pay out");
    assert_eq!(tok(&env.svm, d.dest), DEPOSIT as u64, "LP receives collateral payout");
    assert_eq!(vault_balance(&env), 0, "collateral leaves the program vault");

    let admin_dest = Pubkey::new_unique();
    set_token(&mut env.svm, admin_dest, env.collateral_mint, env.admin.pubkey(), 0);
    close_lp_vault(&mut env).expect("redeemed shares should no longer block CloseLpVault");
    close_slab(&mut env, admin_dest).expect("redeemed backing should no longer block CloseSlab");
}

#[test]
fn lp_redemption_after_resolution_requires_terminal_flat_state() {
    let mut env = setup_vault(0);
    let d = new_depositor(&mut env, DEPOSIT);

    resolve_market(&mut env).expect("resolve empty market with LP backing");
    request(&mut env, &d, DEPOSIT).expect("request still succeeds after resolution");
    set_c_tot_for_test(&mut env, 1);

    let blocked = execute(&mut env, &d).expect_err("resolved redemption must wait for c_tot = 0");
    assert!(blocked.contains("Custom(21)"), "expected EngineLockActive, got {blocked}");
    assert_eq!(tok(&env.svm, d.dest), 0, "LP payout must not occur before terminal flat state");
}

/// RED CONTROL: verifies that the liveness guard no longer blocks redemption
/// when earnings make atoms > total_principal_atoms.
///
/// Pre-fix: `if atoms > ledger.total_principal_atoms` would reject with
/// EngineCounterUnderflow (Custom 5). Post-fix: guard uses principal_portion
/// (which is <= total_principal_atoms by construction).
///
/// We trigger the old guard by having a single LP whose earnings make their
/// atom payout exceed total_principal_atoms, then assert the redemption SUCCEEDS.
#[test]
fn liveness_not_blocked_when_earnings_exceed_principal() {
    // Single depositor with large earnings relative to deposit.
    // Deposit = 100_000, earnings = 300_000 (50% LP share = 150_000 lp_earnings).
    // NAV = 100_000 + 150_000 = 250_000.
    // atoms = floor(1 * 250_000 / 1) = 250_000 > total_principal_atoms (100_000).
    // Pre-fix: `atoms > total_principal_atoms` → EngineCounterUnderflow.
    // Post-fix: `principal_portion (100_000) > total_principal_atoms (100_000)` → false → proceed.
    let mut env = setup_vault(0);
    let d = new_depositor(&mut env, 100_000);
    assert_eq!(reg(&env).total_lp_shares_outstanding, 100_000);

    seed_earnings(&mut env, 300_000);
    // NAV = 100_000 (principal) + 150_000 (50% of 300_000) = 250_000
    // atoms = 250_000, principal_portion = 100_000, earnings_portion = 150_000
    // total_principal_atoms = 100_000 — pre-fix guard would reject (250_000 > 100_000)

    request(&mut env, &d, 100_000).expect("request must succeed");
    env.svm.expire_blockhash();
    // POST-FIX: this must succeed. Pre-fix: EngineCounterUnderflow (Custom 5).
    execute(&mut env, &d).expect("execute must succeed (liveness fix — earnings do not block redemption)");

    // LP receives exactly NAV: 250_000
    assert_eq!(tok(&env.svm, d.dest), 250_000, "LP paid full NAV (principal + earnings)");
    // Vault remainder = insurance stub = 300_000 (gross) - 150_000 (LP payout) = 150_000
    assert_eq!(vault_balance(&env), 150_000, "insurance stub stays in vault");
    // Ledger: gross_consumed = ceil(150_000 * 10_000 / 5_000) = 300_000
    // total_earnings_withdrawn_atoms = 300_000 (full gross chunk consumed).
    let led = ledger(&env);
    assert_eq!(led.total_principal_atoms, 0, "principal zero");
    assert_eq!(
        led.total_earnings_withdrawn_atoms, 300_000,
        "earnings_withdrawn = gross_consumed (300_000), not LP slice (150_000)"
    );
    assert_eq!(reg(&env).total_lp_shares_outstanding, 0, "shares zero");
}

/// RED CONTROL STRUCTURAL CHECK: if the old guard logic `atoms > total_principal`
/// is applied manually, the liveness test above would reject.  This test is a
/// pure-Rust (no BPF) unit assertion confirming the guard logic changed correctly.
/// It does NOT interact with the program; it verifies the fix's guard invariant.
#[test]
fn guard_invariant_principal_portion_le_total_principal_by_construction() {
    // Invariant: principal_portion = floor(shares * available_principal / total_shares)
    // Since available_principal = total_principal_atoms - net_impairment,
    // and net_impairment >= 0, we have available_principal <= total_principal_atoms.
    // Therefore principal_portion <= available_principal <= total_principal_atoms.
    // The guard `principal_portion > total_principal_atoms` can never fire legitimately.
    use percolator::wide_math::wide_mul_div_floor_u128;

    let total_principal_atoms: u128 = 100_000;
    let net_impairment: u128 = 0; // no losses
    let available_principal = total_principal_atoms - net_impairment;
    let shares: u128 = 100_000;
    let total_shares: u128 = 100_000;

    let principal_portion = wide_mul_div_floor_u128(shares, available_principal, total_shares);
    assert!(principal_portion <= total_principal_atoms,
        "principal_portion ({principal_portion}) always <= total_principal_atoms ({total_principal_atoms})");

    // With impairment: available_principal < total_principal_atoms, so even stricter.
    let total_principal_atoms_2: u128 = 100_000;
    let net_impairment_2: u128 = 10_000;
    let available_principal_2 = total_principal_atoms_2 - net_impairment_2;
    let principal_portion_2 = wide_mul_div_floor_u128(shares, available_principal_2, total_shares);
    assert!(principal_portion_2 <= total_principal_atoms_2,
        "impaired case: principal_portion ({principal_portion_2}) <= total_principal_atoms ({total_principal_atoms_2})");
}

// ── LPVAULT-359 regression: insurance stub is tracked + teardown completes ───
/// LPVAULT-359 (HIGH — permanent fund lock-up).
///
/// THE BUG: a full LP-vault redemption pays the LP their fee_share slice of the
/// gross earnings and leaves the insurance (1 − fee_share) slice PHYSICALLY in the
/// vault (the vault decrements only by `atoms` = principal + LP-earnings, not by the
/// full `gross_consumed`). Pre-fix that stub was credited to NO header counter — it
/// sat as an un-owned vault residual. No withdrawal path can touch atoms that aren't
/// tracked in `insurance`/a domain budget, so the stub was undrainable, and
/// `CloseSlab` — gated on `vault==0 && insurance==0 && c_tot==0`
/// (v16_program.rs:8968) — was PERMANENTLY bricked for any vault that had ever
/// earned a fee. The market (and its rent) could never be reclaimed.
///
/// THE FIX (handle_execute_redemption, v16_program.rs ~12321): credit
/// `stub = gross_consumed − earnings_portion` to `header.insurance` AND the
/// redemption domain's per-domain insurance budget. Vault is UNCHANGED (the atoms
/// are already in it), the LP NAV is unchanged (it reads only the backing-domain
/// ledger), and `senior = c_tot + insurance + backing_provider_earnings_total`
/// stays `<= vault` (backing_provider_earnings_total fell by gross_consumed while
/// insurance rose by stub → net senior drop of earnings_portion, the LP payout). The
/// stub now has a withdrawable home (`WithdrawInsuranceAsset`, insurance_operator).
///
/// This test drives the full end-to-end teardown that was impossible pre-fix:
///   deposit → seed earnings → full redeem → drain stub → ResolveMarket → CloseSlab.
///
/// Numbers (fee_share_bps = 5_000): deposit 1_000_000, gross earnings 400_000,
///   lp_earnings = floor(400_000 * 5_000/10_000) = 200_000, NAV = 1_200_000,
///   full redeem pays 1_200_000, gross_consumed = ceil(200_000*10_000/5_000) =
///   400_000, stub = 400_000 − 200_000 = 200_000 → credited to insurance.
#[test]
fn lpvault359_redemption_stub_tracked_and_teardown_completes() {
    let mut env = setup_vault(0); // fee_share_bps = 5_000, immediate cooldown, asset-1 operator = admin
    let d = new_depositor(&mut env, DEPOSIT);
    seed_earnings(&mut env, 400_000);
    assert_eq!(vault_balance(&env), 1_400_000, "vault after deposit + earnings seed");

    // ── Full redeem → LP paid NAV; insurance stub credited (LPVAULT-359 fix). ──
    request(&mut env, &d, DEPOSIT).expect("request");
    env.svm.expire_blockhash();
    execute(&mut env, &d).expect("execute");
    assert_eq!(tok(&env.svm, d.dest), 1_200_000, "LP paid full NAV (principal + 50% of earnings)");
    assert_eq!(reg(&env).total_lp_shares_outstanding, 0, "all shares redeemed");

    // The 200_000 stub physically remains in the vault SPL account...
    assert_eq!(vault_balance(&env), 200_000, "insurance stub physically present in the vault");

    // ...and (THE FIX) is now TRACKED in header.insurance + the redemption domain's
    // budget, rather than sitting as an un-owned residual.
    let (_, group) = state::read_market(&env.svm.get_account(&env.market).unwrap().data).unwrap();
    assert_eq!(group.insurance, 200_000, "FIX: stub credited to header.insurance");
    assert_eq!(group.insurance_domain_budget_remaining_total, 200_000,
        "FIX: stub credited to insurance_domain_budget_remaining_total");
    assert_eq!(group.insurance_domain_budget[DOMAIN as usize], 200_000,
        "FIX: stub credited to the redemption domain's per-domain budget");
    assert_eq!(group.insurance_domain_spent[DOMAIN as usize], 0, "no spend on the domain budget");
    // RESIDUAL NEUTRALITY: vault minus every tracked claim is ZERO — nothing orphaned.
    // (Pre-fix this difference equalled the 200_000 stub: vault 200_000, but insurance,
    // c_tot, backing_provider_earnings_total, source_fresh_backing all 0.)
    assert_eq!(group.vault, 200_000, "header.vault still holds the stub atoms");
    assert_eq!(group.c_tot, 0, "no counterparty capital");
    assert_eq!(group.backing_provider_earnings_total, 0, "earnings pool fully consumed (gross_consumed)");
    assert_eq!(group.source_fresh_backing_total_num, 0, "all principal backing withdrawn");
    let residual = group.vault
        - (group.c_tot + group.insurance + group.backing_provider_earnings_total
            + group.source_fresh_backing_total_num / percolator::BOUND_SCALE);
    assert_eq!(residual, 0, "FIX: zero un-owned vault residual (stub is fully accounted)");

    // ── Drain the stub via WithdrawInsuranceAsset (insurance_operator = admin). ──
    // Requires Live mode (asset 1, domain 2). The greedy long-first debit drains
    // domain 2 fully because the entire stub lives there.
    let pid = env.program_id;
    let payer = env.payer.insecure_clone();
    let admin = env.admin.insecure_clone();
    let admin_insurance_dest = Pubkey::new_unique();
    set_token(&mut env.svm, admin_insurance_dest, env.collateral_mint, admin.pubkey(), 0);
    env.svm.expire_blockhash();
    send(&mut env.svm, pid, &payer, vec![(ProgInstruction::WithdrawInsuranceAsset {
        asset_index: APPEND_ASSET_INDEX, amount: 200_000,
    }, vec![
        AccountMeta::new(admin.pubkey(), true),                 // 0 operator (signer)
        AccountMeta::new(env.market, false),                    // 1 market
        AccountMeta::new(admin_insurance_dest, false),          // 2 dest token (operator-owned)
        AccountMeta::new(env.vault_token, false),               // 3 vault token
        AccountMeta::new_readonly(env.vault_authority, false),  // 4 vault authority PDA
        AccountMeta::new_readonly(spl_token::ID, false),        // 5 token program
    ])], &[&admin]).expect("WithdrawInsuranceAsset drains the stub (was undrainable pre-fix)");

    assert_eq!(tok(&env.svm, admin_insurance_dest), 200_000, "operator received the full stub");
    assert_eq!(vault_balance(&env), 0, "vault SPL balance drained to zero");
    let (_, group) = state::read_market(&env.svm.get_account(&env.market).unwrap().data).unwrap();
    assert_eq!(group.insurance, 0, "header.insurance drained to zero");
    assert_eq!(group.vault, 0, "header.vault drained to zero");
    assert_eq!(group.insurance_domain_budget_remaining_total, 0, "domain budget drained to zero");

    // ── ResolveMarket → terminal mode, then CloseSlab succeeds. ──
    env.svm.expire_blockhash();
    send(&mut env.svm, pid, &payer, vec![(ProgInstruction::ResolveMarket, vec![
        AccountMeta::new(admin.pubkey(), true),
        AccountMeta::new(env.market, false),
    ])], &[&admin]).expect("resolve market");
    let (_, group) = state::read_market(&env.svm.get_account(&env.market).unwrap().data).unwrap();
    assert_eq!(group.mode, MarketModeV16::Resolved, "market resolved (terminal mode 1)");

    // CloseSlab gates on vault==0 && insurance==0 && c_tot==0. Pre-fix the orphaned
    // 200_000 stub kept vault != 0 forever → EngineLockActive. Post-fix it succeeds.
    let close_dest = Pubkey::new_unique();
    set_token(&mut env.svm, close_dest, env.collateral_mint, admin.pubkey(), 0);
    env.svm.expire_blockhash();
    send(&mut env.svm, pid, &payer, vec![(ProgInstruction::CloseSlab, vec![
        AccountMeta::new(admin.pubkey(), true),                 // 0 admin (signer, writable, rent dest)
        AccountMeta::new(env.market, false),                    // 1 market
        AccountMeta::new(env.vault_token, false),               // 2 vault token
        AccountMeta::new_readonly(env.vault_authority, false),  // 3 vault authority PDA
        AccountMeta::new(close_dest, false),                    // 4 dest token (admin-owned)
        AccountMeta::new_readonly(spl_token::ID, false),        // 5 token program
    ])], &[&admin]).expect("CloseSlab succeeds — teardown unbricked by the LPVAULT-359 fix");

    // The market account is zeroed + lamports swept on a successful close.
    assert_eq!(env.svm.get_account(&env.market).map(|a| a.lamports).unwrap_or(0), 0,
        "market lamports swept — slab fully closed");
}
