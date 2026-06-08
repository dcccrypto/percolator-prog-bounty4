//! LP Vault DepositToLpVault integration tests (Phase 2.B Tier 3, Workstream 4B — Phase D).
//!
//! LiteSVM black-box. Exercises DepositToLpVault (tag 66, Option A inline
//! backing top-up): NAV from ledger counters (Note 2), round-DOWN pro-rata
//! shares, reject-on-zero (Note 1), depositor-signed collateral transfer +
//! registry-signed share mint, lazily-created backing-ledger PDA.
//!
//! MANDATORY Phase-D additions (per the pass directive):
//!   1. DIFFERENTIAL TEST `lp_deposit_backing_state_matches_top_up` — the
//!      BackingDomainLedger value counters + market vault total produced by
//!      LpDeposit(amount) are identical to an equivalent TopUpBackingBucket
//!      (amount) call. Mechanical drift safety-net.
//!   3. EXPIRY OVERFLOW — `lp_deposit_twice_no_expiry_overflow` does two
//!      deposits with the u64::MAX/2 sentinel under overflow-checks=true; any
//!      `expiry_slot + N` in the backing path would panic. (Audit in
//!      lp_vault_design.md §5.5 confirms zero such arithmetic.)
//!
//! Cross-reference: lp_vault_design.md §5.5; src/v16_program.rs
//! handle_deposit_to_lp_vault (mirrors handle_top_up_backing_bucket).

use litesvm::LiteSVM;
use percolator_prog::constants::LP_VAULT_BACKING_EXPIRY_SLOT;
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
// Asset 0 is the base asset (active at init, authority = admin). Following the
// v16_cu.rs pattern, we APPEND asset 1 via UpdateAssetLifecycle and bind the LP
// vault to its long-side domain (asset_index 1 → domain 1*2+0 = 2). This lets
// us set the backing authority to the registry PDA at append time.
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
    let mut data = vec![0u8; Mint::LEN];
    Mint::pack(
        Mint {
            mint_authority: COption::None,
            supply: 0,
            decimals: 0,
            is_initialized: true,
            freeze_authority: COption::None,
        },
        &mut data,
    )
    .unwrap();
    data
}

fn make_token_data(mint: Pubkey, owner: Pubkey, amount: u64) -> Vec<u8> {
    let mut data = vec![0u8; TokenAccount::LEN];
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
        &mut data,
    )
    .unwrap();
    data
}

struct Env {
    svm: LiteSVM,
    program_id: Pubkey,
    payer: Keypair,
    admin: Keypair,
    market: Pubkey,
    collateral_mint: Pubkey,
    vault_token: Pubkey,
}

fn set_token(svm: &mut LiteSVM, key: Pubkey, mint: Pubkey, owner: Pubkey, amount: u64) {
    svm.set_account(
        key,
        Account {
            lamports: 1_000_000_000,
            data: make_token_data(mint, owner, amount),
            owner: spl_token::ID,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();
}

fn send(
    svm: &mut LiteSVM,
    program_id: Pubkey,
    payer: &Keypair,
    ix: ProgInstruction,
    accounts: Vec<AccountMeta>,
    extra: &[&Keypair],
) -> Result<(), String> {
    let instruction = Instruction { program_id, accounts, data: ix.encode() };
    let mut signers = vec![payer];
    signers.extend_from_slice(extra);
    let tx = Transaction::new_signed_with_payer(
        &[
            ComputeBudgetInstruction::request_heap_frame(128 * 1024),
            ComputeBudgetInstruction::set_compute_unit_limit(1_400_000),
            instruction,
        ],
        Some(&payer.pubkey()),
        &signers,
        svm.latest_blockhash(),
    );
    svm.send_transaction(tx).map(|_| ()).map_err(|e| format!("{e:?}"))
}

fn init_market_ix() -> ProgInstruction {
    ProgInstruction::InitMarket {
        max_portfolio_assets: MAX_PORTFOLIO_ASSETS,
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
    }
}

/// Append asset 1 (asset_index == configured_slots == 1) with the given backing
/// authority. admin is the cfg.asset_authority (init default) so the append is
/// fee-free. configured_slots grows 1 → 2, enabling domain 2.
fn activate_asset_ix(backing_authority: Pubkey, admin: Pubkey) -> ProgInstruction {
    ProgInstruction::UpdateAssetLifecycle {
        action: ASSET_ACTION_ACTIVATE,
        asset_index: APPEND_ASSET_INDEX,
        now_slot: 1,
        initial_price: 100,
        insurance_authority: admin.to_bytes(),
        insurance_operator: admin.to_bytes(),
        backing_bucket_authority: backing_authority.to_bytes(),
        oracle_authority: admin.to_bytes(),
    }
}

/// Fresh market + collateral mint + program vault token account. Asset not yet
/// activated (caller activates with the chosen backing authority).
fn setup() -> Env {
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
    svm.set_account(
        collateral_mint,
        Account {
            lamports: 1_000_000_000,
            data: make_mint_data(),
            owner: spl_token::ID,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();
    svm.set_account(
        market,
        Account {
            lamports: 1_000_000_000,
            data: vec![0u8; state::market_account_len_for_capacity(MAX_PORTFOLIO_ASSETS as usize).unwrap()],
            owner: program_id,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();
    send(
        &mut svm,
        program_id,
        &payer,
        init_market_ix(),
        vec![
            AccountMeta::new(admin.pubkey(), true),
            AccountMeta::new(market, false),
            AccountMeta::new_readonly(collateral_mint, false),
        ],
        &[&admin],
    )
    .expect("init market");

    let (vault_authority, _) =
        Pubkey::find_program_address(&[b"vault", market.as_ref()], &program_id);
    let vault_token = canonical_vault_ata(&vault_authority, &collateral_mint);
    set_token(&mut svm, vault_token, collateral_mint, vault_authority, 0);

    Env { svm, program_id, payer, admin, market, collateral_mint, vault_token }
}

fn create_lp_vault(env: &mut Env, registry: Pubkey, mint: Pubkey) {
    let admin = env.admin.insecure_clone();
    send(
        &mut env.svm,
        env.program_id,
        &env.payer,
        ProgInstruction::CreateLpVault {
            fee_share_bps: 5_000,
            redemption_cooldown_slots: 100,
            oi_reservation_threshold_bps: 0,
            domain: DOMAIN,
        },
        vec![
            AccountMeta::new(admin.pubkey(), true),
            AccountMeta::new_readonly(env.market, false),
            AccountMeta::new(registry, false),
            AccountMeta::new(mint, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            AccountMeta::new_readonly(spl_token::ID, false),
        ],
        &[&admin],
    )
    .expect("create lp vault");
}

fn activate(env: &mut Env, backing_authority: Pubkey) -> Result<(), String> {
    let admin = env.admin.insecure_clone();
    send(
        &mut env.svm,
        env.program_id,
        &env.payer,
        activate_asset_ix(backing_authority, admin.pubkey()),
        vec![
            AccountMeta::new(admin.pubkey(), true),
            AccountMeta::new(env.market, false),
        ],
        &[&admin],
    )
}

#[allow(clippy::too_many_arguments)]
fn deposit_accounts(
    market: Pubkey,
    vault_token: Pubkey,
    registry: Pubkey,
    mint: Pubkey,
    lp_ata: Pubkey,
    source: Pubkey,
    ledger: Pubkey,
    depositor: Pubkey,
) -> Vec<AccountMeta> {
    vec![
        AccountMeta::new(depositor, true),
        AccountMeta::new(market, false),
        AccountMeta::new(registry, false),
        AccountMeta::new(mint, false),
        AccountMeta::new(lp_ata, false),
        AccountMeta::new(source, false),
        AccountMeta::new(vault_token, false),
        AccountMeta::new(ledger, false),
        AccountMeta::new_readonly(spl_token::ID, false),
        AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
    ]
}

/// Build a fully-set-up LP vault (created + activated with registry authority)
/// + a funded depositor. Returns (registry, mint, ledger, depositor, lp_ata, source).
fn ready_vault(env: &mut Env) -> (Pubkey, Pubkey, Pubkey, Keypair, Pubkey, Pubkey) {
    let (registry, _) = derive_lp_vault_registry(&env.program_id, &env.market);
    let (mint, _) = derive_lp_vault_mint(&env.program_id, &env.market);
    let (ledger, _) = derive_lp_backing_ledger(&env.program_id, &env.market, DOMAIN);
    // Append asset 1 with the registry PDA as its backing authority FIRST so
    // configured_slots == 2 (domain 2 valid), then create the vault on domain 2.
    activate(env, registry).expect("append asset 1 with registry authority");
    create_lp_vault(env, registry, mint);

    let depositor = Keypair::new();
    env.svm.airdrop(&depositor.pubkey(), 100_000_000_000).unwrap();
    let source = Pubkey::new_unique();
    set_token(&mut env.svm, source, env.collateral_mint, depositor.pubkey(), 10_000_000);
    let lp_ata = Pubkey::new_unique();
    set_token(&mut env.svm, lp_ata, mint, depositor.pubkey(), 0);
    (registry, mint, ledger, depositor, lp_ata, source)
}

fn token_amount(svm: &LiteSVM, key: Pubkey) -> u64 {
    let acct = svm.get_account(&key).expect("token account");
    TokenAccount::unpack(&acct.data).expect("token decode").amount
}

#[test]
fn deposit_happy_first_is_one_to_one() {
    let mut env = setup();
    let (registry, mint, ledger, depositor, lp_ata, source) = ready_vault(&mut env);

    send(
        &mut env.svm,
        env.program_id,
        &env.payer,
        ProgInstruction::DepositToLpVault { amount: DEPOSIT },
        deposit_accounts(env.market, env.vault_token, registry, mint, lp_ata, source, ledger, depositor.pubkey()),
        &[&depositor],
    )
    .expect("deposit");

    // First deposit mints 1:1.
    assert_eq!(token_amount(&env.svm, lp_ata), DEPOSIT as u64, "first deposit mints amount shares");
    // Collateral moved into the vault.
    assert_eq!(token_amount(&env.svm, env.vault_token), DEPOSIT as u64, "vault received collateral");
    assert_eq!(token_amount(&env.svm, source), 10_000_000 - DEPOSIT as u64, "source debited");

    // Registry supply mirrors the mint.
    let reg = state::read_lp_vault_registry(&env.svm.get_account(&registry).unwrap().data).unwrap();
    assert_eq!(reg.total_lp_shares_outstanding, DEPOSIT, "registry shares == minted");

    // Backing ledger recorded the principal.
    let led = state::read_backing_domain_ledger(&env.svm.get_account(&ledger).unwrap().data).unwrap();
    assert_eq!(led.total_principal_atoms, DEPOSIT);
    assert_eq!(led.total_deposited_atoms, DEPOSIT);
}

#[test]
fn deposit_rejects_zero_amount() {
    let mut env = setup();
    let (registry, mint, ledger, depositor, lp_ata, source) = ready_vault(&mut env);
    let res = send(
        &mut env.svm,
        env.program_id,
        &env.payer,
        ProgInstruction::DepositToLpVault { amount: 0 },
        deposit_accounts(env.market, env.vault_token, registry, mint, lp_ata, source, ledger, depositor.pubkey()),
        &[&depositor],
    );
    assert!(res.is_err(), "zero-amount deposit must reject: {res:?}");
}

#[test]
fn deposit_rejects_before_authority_handover() {
    // Note 5: after CreateLpVault but BEFORE the operator's UpdateAssetLifecycle
    // sets backing_bucket_authority = registry PDA, deposits fail closed.
    let mut env = setup();
    let (registry, _) = derive_lp_vault_registry(&env.program_id, &env.market);
    let (mint, _) = derive_lp_vault_mint(&env.program_id, &env.market);
    let (ledger, _) = derive_lp_backing_ledger(&env.program_id, &env.market, DOMAIN);
    // Append asset 1 with admin (NOT registry) as the backing authority, then
    // create the vault on domain 2. The registry is never made the authority.
    let admin_pk = env.admin.pubkey();
    activate(&mut env, admin_pk).expect("append asset 1 with admin authority");
    create_lp_vault(&mut env, registry, mint);

    let depositor = Keypair::new();
    env.svm.airdrop(&depositor.pubkey(), 100_000_000_000).unwrap();
    let source = Pubkey::new_unique();
    set_token(&mut env.svm, source, env.collateral_mint, depositor.pubkey(), 10_000_000);
    let lp_ata = Pubkey::new_unique();
    set_token(&mut env.svm, lp_ata, mint, depositor.pubkey(), 0);

    let res = send(
        &mut env.svm,
        env.program_id,
        &env.payer,
        ProgInstruction::DepositToLpVault { amount: DEPOSIT },
        deposit_accounts(env.market, env.vault_token, registry, mint, lp_ata, source, ledger, depositor.pubkey()),
        &[&depositor],
    );
    assert!(res.is_err(), "deposit before authority handover must fail closed (Note 5): {res:?}");
    assert_eq!(token_amount(&env.svm, env.vault_token), 0, "no collateral moved on rejected deposit");
}

#[test]
fn lp_deposit_twice_no_expiry_overflow() {
    // EXPIRY OVERFLOW guard: two deposits stamp the u64::MAX/2 sentinel on the
    // backing bucket. With overflow-checks=true, any `expiry_slot + N` in the
    // backing path would panic. Both deposits succeeding proves no such arith.
    assert_eq!(LP_VAULT_BACKING_EXPIRY_SLOT, u64::MAX / 2);
    let mut env = setup();
    let (registry, mint, ledger, depositor, lp_ata, source) = ready_vault(&mut env);
    for _ in 0..2 {
        send(
            &mut env.svm,
            env.program_id,
            &env.payer,
            ProgInstruction::DepositToLpVault { amount: DEPOSIT },
            deposit_accounts(env.market, env.vault_token, registry, mint, lp_ata, source, ledger, depositor.pubkey()),
            &[&depositor],
        )
        .expect("deposit (no expiry overflow)");
        // Fresh blockhash so the second (otherwise byte-identical) tx is not
        // rejected as AlreadyProcessed.
        env.svm.expire_blockhash();
    }
    // 1:1 each time, no earnings → 2x shares.
    assert_eq!(token_amount(&env.svm, lp_ata), 2 * DEPOSIT as u64);
    let led = state::read_backing_domain_ledger(&env.svm.get_account(&ledger).unwrap().data).unwrap();
    assert_eq!(led.total_principal_atoms, 2 * DEPOSIT);
}

#[test]
fn lp_deposit_backing_state_matches_top_up() {
    // DIFFERENTIAL drift safety-net: LpDeposit(amount)'s resulting
    // BackingDomainLedger value counters + market vault total are identical to
    // an equivalent TopUpBackingBucket(amount) call (same amount, same expiry
    // sentinel). If the two backing-top-up sequences ever drift, this fails.
    // Identity fields (authority, market_group — two different markets) legitimately
    // differ and are excluded from the comparison.

    // ── Path A: TopUpBackingBucket on a market whose backing authority = admin. ──
    let mut env_a = setup();
    let admin_a = env_a.admin.insecure_clone();
    activate(&mut env_a, admin_a.pubkey()).expect("activate A (admin authority)");
    // admin's source funds + a client-managed ledger account.
    let source_a = Pubkey::new_unique();
    set_token(&mut env_a.svm, source_a, env_a.collateral_mint, admin_a.pubkey(), 10_000_000);
    let ledger_a = Pubkey::new_unique();
    env_a.svm.set_account(
        ledger_a,
        Account {
            lamports: 1_000_000_000,
            data: vec![0u8; state::backing_domain_ledger_account_len()],
            owner: env_a.program_id,
            executable: false,
            rent_epoch: 0,
        },
    ).unwrap();
    send(
        &mut env_a.svm,
        env_a.program_id,
        &env_a.payer,
        ProgInstruction::TopUpBackingBucket { domain: DOMAIN, amount: DEPOSIT, expiry_slot: LP_VAULT_BACKING_EXPIRY_SLOT },
        vec![
            AccountMeta::new(admin_a.pubkey(), true),
            AccountMeta::new(env_a.market, false),
            AccountMeta::new(source_a, false),
            AccountMeta::new(env_a.vault_token, false),
            AccountMeta::new_readonly(spl_token::ID, false),
            AccountMeta::new(ledger_a, false),
        ],
        &[&admin_a],
    )
    .expect("top up backing bucket");
    let led_a = state::read_backing_domain_ledger(&env_a.svm.get_account(&ledger_a).unwrap().data).unwrap();
    let (_, group_a) = state::read_market(&env_a.svm.get_account(&env_a.market).unwrap().data).unwrap();
    let vault_a = group_a.vault;

    // ── Path B: LpDeposit on a vault whose backing authority = registry PDA. ──
    let mut env_b = setup();
    let (registry_b, mint_b, ledger_b, depositor_b, lp_ata_b, source_b) = ready_vault(&mut env_b);
    send(
        &mut env_b.svm,
        env_b.program_id,
        &env_b.payer,
        ProgInstruction::DepositToLpVault { amount: DEPOSIT },
        deposit_accounts(env_b.market, env_b.vault_token, registry_b, mint_b, lp_ata_b, source_b, ledger_b, depositor_b.pubkey()),
        &[&depositor_b],
    )
    .expect("lp deposit");
    let led_b = state::read_backing_domain_ledger(&env_b.svm.get_account(&ledger_b).unwrap().data).unwrap();
    let (_, group_b) = state::read_market(&env_b.svm.get_account(&env_b.market).unwrap().data).unwrap();
    let vault_b = group_b.vault;

    // Value counters identical (drift detector).
    assert_eq!(led_a.total_principal_atoms, led_b.total_principal_atoms, "principal drift");
    assert_eq!(led_a.total_deposited_atoms, led_b.total_deposited_atoms, "deposited drift");
    assert_eq!(led_a.total_principal_withdrawn_atoms, led_b.total_principal_withdrawn_atoms);
    assert_eq!(led_a.total_earnings_atoms, led_b.total_earnings_atoms, "earnings drift");
    assert_eq!(led_a.total_earnings_withdrawn_atoms, led_b.total_earnings_withdrawn_atoms);
    assert_eq!(led_a.cumulative_loss_atoms, led_b.cumulative_loss_atoms, "loss drift");
    assert_eq!(led_a.cumulative_recovery_atoms, led_b.cumulative_recovery_atoms, "recovery drift");
    assert_eq!(led_a.last_observed_bucket_earnings_atoms, led_b.last_observed_bucket_earnings_atoms);
    assert_eq!(led_a.last_observed_unavailable_principal_atoms, led_b.last_observed_unavailable_principal_atoms);
    assert_eq!(led_a.domain, led_b.domain, "domain");
    // Market vault total identical (the backing-bucket collateral effect).
    assert_eq!(vault_a, vault_b, "market vault total drift between TopUp and LpDeposit");
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
