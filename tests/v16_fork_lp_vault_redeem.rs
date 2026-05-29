//! LP Vault redeem-flow integration tests (Phase 2.B Tier 3, Workstream 4B — Phase E).
//!
//! RequestRedeemLpShares (tag 67) + ExecuteRedemption (tag 68), B-7 cooldown.
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
    let vault_token = Pubkey::new_unique();
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
    send(&mut env.svm, pid, &payer, vec![(ProgInstruction::RequestRedeemLpShares { lp_amount }, accts)], &[&kp])
}

fn execute(env: &mut Env, d: &Depositor) -> Result<(), String> {
    let pid = env.program_id;
    let payer = env.payer.insecure_clone();
    let accts = execute_accounts(env, d);
    send(&mut env.svm, pid, &payer, vec![(ProgInstruction::ExecuteRedemption, accts)], &[])
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
    send(&mut env_a.svm, pid_a, &payer_a, vec![(ProgInstruction::TopUpBackingBucket { domain: DOMAIN as u8, amount: DEPOSIT, expiry_slot: percolator_prog::constants::LP_VAULT_BACKING_EXPIRY_SLOT },
        vec![AccountMeta::new(admin_a.pubkey(), true), AccountMeta::new(env_a.market, false), AccountMeta::new(src_a, false), AccountMeta::new(env_a.vault_token, false), AccountMeta::new_readonly(spl_token::ID, false), AccountMeta::new(ledger_a, false)])], &[&admin_a]).expect("top up");
    env_a.svm.expire_blockhash();
    send(&mut env_a.svm, pid_a, &payer_a, vec![(ProgInstruction::WithdrawBackingBucket { domain: DOMAIN as u8, amount: DEPOSIT },
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
    let vault_token = Pubkey::new_unique();
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
