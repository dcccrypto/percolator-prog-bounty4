//! LP Vault data-layer unit tests (Phase 2.B Tier 3, Workstream 4B — Phase B).
//!
//! Covers the wrapper-side LP Vault account types + helpers added in the
//! `state` module: struct sizes/layout, PDA derivation determinism, and
//! read/write/validate round-trips for `LpVaultRegistryV16` and
//! `LpRedemptionV16`. Instruction handlers (Phases C-H) are tested
//! separately via LiteSVM.
//!
//! Cross-reference: `~/wrapper-engine-deep-audit/lp_vault_design.md` §4.

use percolator_prog::constants::{
    KIND_LP_REDEMPTION, KIND_LP_VAULT_REGISTRY, LP_VAULT_VERSION, TAG_CLOSE_LP_VAULT,
    TAG_CREATE_LP_VAULT, TAG_DEPOSIT_TO_LP_VAULT, TAG_EXECUTE_REDEMPTION, TAG_LP_VAULT_CRANK_FEES,
    TAG_REQUEST_REDEEM_LP_SHARES, TAG_SET_LP_VAULT_PAUSED,
};
use percolator_prog::state::{
    derive_lp_redemption, derive_lp_vault_mint, derive_lp_vault_registry, init_lp_redemption,
    init_lp_vault_registry, lp_redemption_account_len, lp_vault_registry_account_len,
    read_lp_redemption, read_lp_vault_registry, write_lp_vault_registry, LpRedemptionV16,
    LpVaultRegistryV16,
};
use solana_program::pubkey::Pubkey;

fn sample_registry() -> LpVaultRegistryV16 {
    LpVaultRegistryV16 {
        market_group: [1u8; 32],
        lp_mint: [2u8; 32],
        total_lp_shares_outstanding: 0,
        insurance_fee_snapshot_atoms: 0,
        fee_distribution_total_atoms: 0,
        epoch: 0,
        redemption_cooldown_slots: 100,
        fee_share_bps: 5_000,
        oi_reservation_threshold_bps: 0,
        domain: 0,
        paused: 0,
        version: LP_VAULT_VERSION,
        bump: 254,
        mint_bump: 253,
        _padding: [0u8; 6],
        _reserved: [0u8; 16],
    }
}

fn sample_redemption() -> LpRedemptionV16 {
    LpRedemptionV16 {
        registry: [3u8; 32],
        redeemer: [4u8; 32],
        shares: 1_000,
        request_slot: 42,
        version: LP_VAULT_VERSION,
        bump: 252,
        _padding: [0u8; 6],
    }
}

// ── Struct layout ───────────────────────────────────────────────────

#[test]
fn registry_struct_size_is_160() {
    assert_eq!(core::mem::size_of::<LpVaultRegistryV16>(), 160);
}

#[test]
fn redemption_struct_size_is_96() {
    assert_eq!(core::mem::size_of::<LpRedemptionV16>(), 96);
}

#[test]
fn account_lens_include_header() {
    // HEADER_LEN is 16; account len = header + struct.
    assert_eq!(lp_vault_registry_account_len(), 16 + 160);
    assert_eq!(lp_redemption_account_len(), 16 + 96);
}

/// v17 CHANGE (matrix rows 21-27): LP vault tags renumbered from 65-71 to 74-80 to avoid
/// collision with toly's UpdateAssetAuthority(65)/BatchTradeNoCpi(66)/... tags.
/// Each tag still encodes its own opcode byte in the ix decoder — function is preserved.
#[test]
fn lp_vault_tags_are_contiguous_74_to_80() {
    assert_eq!(TAG_CREATE_LP_VAULT, 74);
    assert_eq!(TAG_DEPOSIT_TO_LP_VAULT, 75);
    assert_eq!(TAG_REQUEST_REDEEM_LP_SHARES, 76);
    assert_eq!(TAG_EXECUTE_REDEMPTION, 77);
    assert_eq!(TAG_LP_VAULT_CRANK_FEES, 78);
    assert_eq!(TAG_SET_LP_VAULT_PAUSED, 79);
    assert_eq!(TAG_CLOSE_LP_VAULT, 80);
}

#[test]
fn lp_kinds_distinct() {
    assert_eq!(KIND_LP_VAULT_REGISTRY, 5);
    assert_eq!(KIND_LP_REDEMPTION, 6);
    assert_ne!(KIND_LP_VAULT_REGISTRY, KIND_LP_REDEMPTION);
}

// ── PDA derivation ──────────────────────────────────────────────────

#[test]
fn registry_pda_is_deterministic() {
    let prog = Pubkey::new_unique();
    let market = Pubkey::new_unique();
    let (a, ba) = derive_lp_vault_registry(&prog, &market);
    let (b, bb) = derive_lp_vault_registry(&prog, &market);
    assert_eq!(a, b);
    assert_eq!(ba, bb);
}

#[test]
fn registry_pda_differs_per_market() {
    let prog = Pubkey::new_unique();
    let m1 = Pubkey::new_unique();
    let m2 = Pubkey::new_unique();
    assert_ne!(
        derive_lp_vault_registry(&prog, &m1).0,
        derive_lp_vault_registry(&prog, &m2).0
    );
}

#[test]
fn mint_pda_differs_from_registry_pda() {
    let prog = Pubkey::new_unique();
    let market = Pubkey::new_unique();
    // Registry and mint use different seed prefixes → distinct PDAs.
    assert_ne!(
        derive_lp_vault_registry(&prog, &market).0,
        derive_lp_vault_mint(&prog, &market).0
    );
}

#[test]
fn redemption_pda_differs_per_redeemer() {
    let prog = Pubkey::new_unique();
    let registry = Pubkey::new_unique();
    let r1 = Pubkey::new_unique();
    let r2 = Pubkey::new_unique();
    assert_ne!(
        derive_lp_redemption(&prog, &registry, &r1).0,
        derive_lp_redemption(&prog, &registry, &r2).0
    );
}

// ── Registry read/write round-trip ──────────────────────────────────

#[test]
fn registry_init_read_round_trip() {
    let reg = sample_registry();
    let mut data = vec![0u8; lp_vault_registry_account_len()];
    init_lp_vault_registry(&mut data, &reg).unwrap();
    let got = read_lp_vault_registry(&data).unwrap();
    assert_eq!(got, reg);
}

#[test]
fn registry_write_after_init_round_trip() {
    let mut data = vec![0u8; lp_vault_registry_account_len()];
    init_lp_vault_registry(&mut data, &sample_registry()).unwrap();
    let mut reg = sample_registry();
    reg.total_lp_shares_outstanding = 12_345;
    reg.epoch = 7;
    write_lp_vault_registry(&mut data, &reg).unwrap();
    assert_eq!(read_lp_vault_registry(&data).unwrap(), reg);
}

#[test]
fn registry_double_init_rejected() {
    let mut data = vec![0u8; lp_vault_registry_account_len()];
    init_lp_vault_registry(&mut data, &sample_registry()).unwrap();
    assert!(init_lp_vault_registry(&mut data, &sample_registry()).is_err());
}

#[test]
fn registry_rejects_zero_market_group() {
    let mut reg = sample_registry();
    reg.market_group = [0u8; 32];
    let mut data = vec![0u8; lp_vault_registry_account_len()];
    assert!(init_lp_vault_registry(&mut data, &reg).is_err());
}

#[test]
fn registry_rejects_zero_mint() {
    let mut reg = sample_registry();
    reg.lp_mint = [0u8; 32];
    let mut data = vec![0u8; lp_vault_registry_account_len()];
    assert!(init_lp_vault_registry(&mut data, &reg).is_err());
}

#[test]
fn registry_rejects_fee_share_above_cap() {
    let mut reg = sample_registry();
    reg.fee_share_bps = 10_001;
    let mut data = vec![0u8; lp_vault_registry_account_len()];
    assert!(init_lp_vault_registry(&mut data, &reg).is_err());
}

#[test]
fn registry_rejects_bad_version() {
    let mut reg = sample_registry();
    reg.version = 99;
    let mut data = vec![0u8; lp_vault_registry_account_len()];
    assert!(init_lp_vault_registry(&mut data, &reg).is_err());
}

#[test]
fn registry_rejects_paused_out_of_range() {
    let mut reg = sample_registry();
    reg.paused = 2;
    let mut data = vec![0u8; lp_vault_registry_account_len()];
    assert!(init_lp_vault_registry(&mut data, &reg).is_err());
}

#[test]
fn registry_read_wrong_kind_rejected() {
    // A redemption-kind account must not read as a registry.
    let mut data = vec![0u8; lp_redemption_account_len().max(lp_vault_registry_account_len())];
    init_lp_redemption(&mut data, &sample_redemption()).unwrap();
    assert!(read_lp_vault_registry(&data).is_err());
}

// ── Redemption read/write round-trip ────────────────────────────────

#[test]
fn redemption_init_read_round_trip() {
    let red = sample_redemption();
    let mut data = vec![0u8; lp_redemption_account_len()];
    init_lp_redemption(&mut data, &red).unwrap();
    assert_eq!(read_lp_redemption(&data).unwrap(), red);
}

#[test]
fn redemption_rejects_zero_registry() {
    let mut red = sample_redemption();
    red.registry = [0u8; 32];
    let mut data = vec![0u8; lp_redemption_account_len()];
    assert!(init_lp_redemption(&mut data, &red).is_err());
}

#[test]
fn redemption_rejects_zero_redeemer() {
    let mut red = sample_redemption();
    red.redeemer = [0u8; 32];
    let mut data = vec![0u8; lp_redemption_account_len()];
    assert!(init_lp_redemption(&mut data, &red).is_err());
}
