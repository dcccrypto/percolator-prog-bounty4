//! Percolator v16 Solana wrapper.
//!
//! v16 is account-local: a market-group account stores `MarketGroupV16`, and
//! each trader/LP is an independently supplied `PortfolioAccountV16`. The
//! wrapper deliberately does not recreate the legacy global account slab.

#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

use alloc::vec::Vec;
use percolator::{
    v16_domain_count_for_market_slots, BackingBucketStatusV16, MarketModeV16,
    PermissionlessCrankActionV16, PermissionlessCrankRequestV16, RebalanceRequestV16, SideV16,
    SourceCreditStateV16, TradeRequestV16, V16Config, V16Error, BOUND_SCALE,
};
use solana_program::{
    account_info::AccountInfo,
    clock::Clock,
    declare_id,
    entrypoint::ProgramResult,
    instruction::{AccountMeta, Instruction as SolInstruction},
    program::{invoke, invoke_signed},
    program_error::ProgramError,
    program_pack::Pack,
    pubkey::Pubkey,
    rent::Rent,
    system_instruction,
    system_program,
    sysvar::Sysvar,
};

declare_id!("Perco1ator111111111111111111111111111111111");

pub mod constants {
    use core::mem::size_of;
    use percolator::{
        Market, MarketGroupV16HeaderAccount, PortfolioAccountV16Account,
        PortfolioSourceDomainV16Account,
    };

    pub const MAGIC: u64 = 0x5045_5243_5631_3600; // "PERCV16\0"
    pub const VERSION: u16 = 16;
    pub const KIND_MARKET: u8 = 1;
    pub const KIND_PORTFOLIO: u8 = 2;
    pub const KIND_BACKING_DOMAIN_LEDGER: u8 = 3;
    pub const KIND_INSURANCE_LEDGER: u8 = 4;

    pub const HEADER_LEN: usize = 16;
    pub const WRAPPER_CONFIG_LEN: usize = 624;
    pub const ASSET_ORACLE_PROFILE_LEN: usize = 368;
    pub const ASSET_ORACLE_WRAPPER_LEN: usize = 512;
    pub const MARKET_GROUP_LEN: usize = size_of::<MarketGroupV16HeaderAccount>();
    pub const MARKET_ASSET_SLOT_LEN: usize = size_of::<Market<[u8; ASSET_ORACLE_WRAPPER_LEN]>>();
    pub const PORTFOLIO_STATE_LEN: usize = size_of::<PortfolioAccountV16Account>();
    pub const PORTFOLIO_SOURCE_DOMAIN_LEN: usize = size_of::<PortfolioSourceDomainV16Account>();
    pub const MARKET_GROUP_OFF: usize = HEADER_LEN + WRAPPER_CONFIG_LEN;
    pub const MIN_MARKET_ACCOUNT_LEN: usize = MARKET_GROUP_OFF + MARKET_GROUP_LEN;
    pub const DEFAULT_MARKET_SLOT_CAPACITY: usize = 1;
    pub const MARKET_ACCOUNT_LEN: usize =
        MARKET_GROUP_OFF + MARKET_GROUP_LEN + DEFAULT_MARKET_SLOT_CAPACITY * MARKET_ASSET_SLOT_LEN;
    pub const PORTFOLIO_ACCOUNT_LEN: usize = HEADER_LEN
        + PORTFOLIO_STATE_LEN
        + DEFAULT_MARKET_SLOT_CAPACITY * 2 * PORTFOLIO_SOURCE_DOMAIN_LEN;
    pub const MAX_MATCHER_TAIL_ACCOUNTS: usize = 32;
    pub const MATCHER_ABI_VERSION: u32 = 3;
    pub const MATCHER_CONTEXT_MIN_LEN: usize = 64;
    pub const ORACLE_LEG_CAP: usize = 3;
    pub const ORACLE_MODE_MANUAL: u8 = 0;
    pub const ORACLE_MODE_HYBRID_AFTER_HOURS: u8 = 1;
    pub const ORACLE_MODE_EWMA_MARK: u8 = 2;
    pub const ORACLE_MODE_AUTH_MARK: u8 = 3;
    pub const ORACLE_LEG_FLAG_DIVIDE_LEG2: u8 = 1 << 0;
    pub const ORACLE_LEG_FLAG_DIVIDE_LEG3: u8 = 1 << 1;
    pub const ORACLE_LEG_FLAGS_MASK: u8 = ORACLE_LEG_FLAG_DIVIDE_LEG2 | ORACLE_LEG_FLAG_DIVIDE_LEG3;
    pub const SWITCHBOARD_RESULT_SCALE: u128 = 1_000_000_000_000;
    pub const DEFAULT_MARK_EWMA_HALFLIFE_SLOTS: u64 = 600;
    pub const MAX_DYNAMIC_TRADE_FEE_BPS: u64 = 10_000;
    pub const MIN_INSURANCE_WITHDRAW_FLOOR_UNITS: u128 = 10;
    pub const MAX_PERMISSIONLESS_RESOLVE_STALE_SLOTS: u64 = 6_480_000;
    pub const MAX_FORCE_CLOSE_DELAY_SLOTS: u64 = 10_000_000;
    /// Fork B-11 upper bound on per-asset `max_staleness_secs`. v16 baseline
    /// has no upper bound — an operator could configure unbounded oracle
    /// staleness, which loosens the trade / mark / liquidation gates beyond
    /// what fork wants to permit. The 24-hour cap is the fork's audited
    /// envelope (see wrapper_design_B11_oracle_staleness_cap.md). Bound is
    /// enforced at every site that ingests `max_staleness_secs` from
    /// instruction args before storage.
    pub const MAX_ORACLE_STALENESS_SECS: u64 = 86_400;
    // v16 exposes up to 64 market slots, but one portfolio may only carry the
    // largest active-leg count that fits the audited stale-trade and crank CU
    // envelope. Additional markets remain usable through separate portfolios.
    pub const WRAPPER_MAX_PORTFOLIO_ASSETS: u16 = 14;

    // ── Fork LP Vault (Phase 2.B Tier 3, Workstream 4B) ──────────────────
    // Account kinds 1-4 are MARKET / PORTFOLIO / BACKING_DOMAIN_LEDGER /
    // INSURANCE_LEDGER. LP Vault adds two new kinds.
    pub const KIND_LP_VAULT_REGISTRY: u8 = 5;
    pub const KIND_LP_REDEMPTION: u8 = 6;

    /// PDA seeds: registry = ["lp_vault", market_group]; mint =
    /// ["lp_vault_mint", market_group]; redemption =
    /// ["lp_redemption", registry, redeemer].
    pub const LP_VAULT_REGISTRY_SEED: &[u8] = b"lp_vault";
    pub const LP_VAULT_MINT_SEED: &[u8] = b"lp_vault_mint";
    pub const LP_REDEMPTION_SEED: &[u8] = b"lp_redemption";
    /// LP Vault's backing-domain ledger PDA: ["lp_backing_ledger", market, domain_le].
    /// Deterministic (vs TopUpBackingBucket's client-managed ledger) so deposit /
    /// redeem / crank all address the same `BackingDomainLedgerAccountV16`.
    pub const LP_BACKING_LEDGER_SEED: &[u8] = b"lp_backing_ledger";
    /// LP Vault's shared redemption-escrow SPL token account PDA:
    /// ["lp_escrow", market]. Owned by the registry PDA, holds the LP-mint
    /// shares of all pending (requested-but-not-executed) redemptions. Invariant
    /// I12: escrow balance == Σ(outstanding LpRedemption.shares).
    pub const LP_ESCROW_SEED: &[u8] = b"lp_escrow";

    pub const LP_VAULT_VERSION: u8 = 1;

    /// Expiry slot stamped on the LP Vault's backing bucket. LP-provided backing
    /// is permanent until redeemed, so it must never "expire". A large sentinel
    /// (NOT u64::MAX) is used so that any future `expiry_slot + N` arithmetic
    /// cannot overflow — verified 2026-05-27 that ALL expiry comparisons in the
    /// engine + wrapper backing path are bare `<= / >= / < / > / == / !=` with NO
    /// addition (lp_vault_design.md §5.5 expiry audit). `u64::MAX / 2` is ~4.6e18
    /// slots ≈ billions of years at Solana's slot rate, and leaves `u64::MAX/2`
    /// of headroom above it.
    pub const LP_VAULT_BACKING_EXPIRY_SLOT: u64 = u64::MAX / 2;

    /// LP Vault instruction tags. The synced wrapper baseline uses tags
    /// 0-64 densely; 65-71 is the first free contiguous block. (BREAKING_
    /// CHANGES.md §4's "unused 38-49" snapshot is stale — see
    /// lp_vault_design.md tag-allocation correction.)
    pub const TAG_CREATE_LP_VAULT: u8 = 65;
    pub const TAG_DEPOSIT_TO_LP_VAULT: u8 = 66;
    pub const TAG_REQUEST_REDEEM_LP_SHARES: u8 = 67;
    pub const TAG_EXECUTE_REDEMPTION: u8 = 68;
    pub const TAG_LP_VAULT_CRANK_FEES: u8 = 69;
    pub const TAG_SET_LP_VAULT_PAUSED: u8 = 70;
    pub const TAG_CLOSE_LP_VAULT: u8 = 71;

    // ── Fork NFT / B-3 TransferPortfolioOwnership (Phase 2.B Tier 3 / A.4) ─
    // Tags 72-73 are the first block after LP Vault's 65-71.
    pub const KIND_NFT_REGISTRY: u8 = 7;

    /// Per-market NFT-program-id registry.
    /// PDA seeds: `["nft_registry", market_group]`.
    pub const NFT_REGISTRY_SEED: &[u8] = b"nft_registry";
    pub const NFT_REGISTRY_VERSION: u8 = 1;

    /// SHARED SEED CONTRACT with the percolator-nft program — that program
    /// signs B-3 with `find_program_address([NFT_MINT_AUTHORITY_SEED],
    /// nft_program_id)`.  If the NFT program's mint-authority seed ever
    /// changes, this derivation + the B-3 signer check silently break;
    /// the cross-ref comment and the derived-PDA test in
    /// `tests/v16_fork_b3_nft_cpi.rs` are the guard.  See
    /// `percolator-nft/src/state_v16.rs MINT_AUTHORITY_SEED`.
    pub const NFT_MINT_AUTHORITY_SEED: &[u8] = b"mint_authority";

    /// B-3 TransferPortfolioOwnership: tag 72.
    /// Wire: tag(72) + new_owner[32] + asset_index(2 LE).
    pub const TAG_TRANSFER_PORTFOLIO_OWNERSHIP: u8 = 72;

    /// SetNftProgramId: tag 73.
    /// Wire: tag(73) + nft_program_id[32].
    pub const TAG_SET_NFT_PROGRAM_ID: u8 = 73;
}

pub mod error {
    use percolator::V16Error;
    use solana_program::program_error::ProgramError;

    #[derive(Clone, Debug, Eq, PartialEq)]
    pub enum PercolatorError {
        InvalidMagic,
        InvalidVersion,
        AlreadyInitialized,
        NotInitialized,
        InvalidAccountKind,
        InvalidAccountLen,
        ExpectedSigner,
        ExpectedWritable,
        Unauthorized,
        InvalidInstruction,
        InvalidMint,
        InvalidTokenAccount,
        InvalidVaultAccount,
        InvalidTokenProgram,
        EngineInvalidConfig,
        EngineArithmeticOverflow,
        EngineProvenanceMismatch,
        EngineHiddenLeg,
        EngineInvalidLeg,
        EngineStale,
        EngineBStale,
        EngineLockActive,
        EngineNonProgress,
        EngineRecoveryRequired,
        EngineCounterOverflow,
        EngineCounterUnderflow,
        OracleInvalid,
        OracleStale,
        OracleConfTooWide,
        InvalidOracleKey,
        // ── Fork LP Vault (Phase 2.B Tier 3, Workstream 4B) ──────────────
        // Appended at the end so existing discriminants (0-29) are unshifted.
        // Actual Custom() codes: 30-41 in enum order below.
        LpVaultAlreadyExists,        // Custom(30)
        LpVaultNotFound,             // Custom(31)
        LpVaultPaused,               // Custom(32)
        LpVaultSharesOutstanding,    // Custom(33)
        LpVaultZeroAmount,           // Custom(34)
        LpVaultInsufficientShares,   // Custom(35)
        LpVaultCooldownActive,       // Custom(36)
        LpVaultOiReservationViolated, // Custom(37)
        LpVaultNoFeesToCrank,        // Custom(38)
        LpVaultSupplyMismatch,       // Custom(39)
        LpVaultAuthorityMismatch,    // Custom(40)
        LpVaultZeroSharesMinted,     // Custom(41) — Note 1 round-to-zero reject
        // ── Fork NFT / B-3 TransferPortfolioOwnership (Phase 2.B Tier 3 / A.4)
        // Appended after LP Vault variants so existing codes 0-41 are unshifted.
        // Actual Custom() codes: 42-46 in enum order below.
        NftRegistryNotFound,         // Custom(42) — registry uninitialized for this market
        NftPortfolioNotTransferable, // Custom(43) — leg gating: locked/stale/closed/mid-close
        NftTransferSelfOrZero,       // Custom(44) — new_owner == current or zero
        NftInvalidMintAuthority,     // Custom(45) — signer != derive_nft_mint_authority(reg)
        NftPortfolioProvenance,      // Custom(46) — layout/version/provenance mismatch
    }

    impl From<PercolatorError> for ProgramError {
        fn from(value: PercolatorError) -> Self {
            ProgramError::Custom(value as u32)
        }
    }

    pub fn map_v16_error(err: V16Error) -> ProgramError {
        let mapped = match err {
            V16Error::InvalidConfig => PercolatorError::EngineInvalidConfig,
            V16Error::ArithmeticOverflow => PercolatorError::EngineArithmeticOverflow,
            V16Error::ProvenanceMismatch => PercolatorError::EngineProvenanceMismatch,
            V16Error::HiddenLeg => PercolatorError::EngineHiddenLeg,
            V16Error::InvalidLeg => PercolatorError::EngineInvalidLeg,
            V16Error::Stale => PercolatorError::EngineStale,
            V16Error::BStale => PercolatorError::EngineBStale,
            V16Error::LockActive => PercolatorError::EngineLockActive,
            V16Error::NonProgress => PercolatorError::EngineNonProgress,
            V16Error::RecoveryRequired => PercolatorError::EngineRecoveryRequired,
            V16Error::CounterOverflow => PercolatorError::EngineCounterOverflow,
            V16Error::CounterUnderflow => PercolatorError::EngineCounterUnderflow,
        };
        mapped.into()
    }
}

pub mod state {
    use crate::{
        constants::{
            ASSET_ORACLE_PROFILE_LEN, ASSET_ORACLE_WRAPPER_LEN, HEADER_LEN,
            KIND_BACKING_DOMAIN_LEDGER, KIND_INSURANCE_LEDGER, KIND_LP_REDEMPTION,
            KIND_LP_VAULT_REGISTRY, KIND_MARKET, KIND_NFT_REGISTRY, KIND_PORTFOLIO,
            LP_VAULT_VERSION, MAGIC, MARKET_GROUP_LEN, MARKET_GROUP_OFF,
            MIN_MARKET_ACCOUNT_LEN, NFT_MINT_AUTHORITY_SEED, NFT_REGISTRY_SEED,
            NFT_REGISTRY_VERSION, ORACLE_LEG_CAP, ORACLE_LEG_FLAGS_MASK,
            ORACLE_MODE_AUTH_MARK, ORACLE_MODE_EWMA_MARK, ORACLE_MODE_HYBRID_AFTER_HOURS,
            ORACLE_MODE_MANUAL, PORTFOLIO_ACCOUNT_LEN, PORTFOLIO_SOURCE_DOMAIN_LEN,
            PORTFOLIO_STATE_LEN, VERSION, WRAPPER_CONFIG_LEN,
        },
        error::PercolatorError,
    };
    #[cfg(not(target_os = "solana"))]
    use alloc::boxed::Box;
    #[cfg(not(target_os = "solana"))]
    use alloc::vec::Vec;
    use percolator::{
        v16_domain_count_for_market_slots, AssetStateV16, EngineAssetSlotV16Account, Market,
        MarketGroupV16HeaderAccount, MarketGroupV16ViewMut, MarketModeV16,
        PortfolioAccountV16Account, PortfolioSourceDomainV16Account, PortfolioV16ViewMut,
        ProvenanceHeaderV16, V16Config, V16Error, V16PodU64,
    };
    #[cfg(not(target_os = "solana"))]
    use percolator::{
        CloseProgressLedgerV16, HealthCertV16, MarketGroupV16, PortfolioAccountV16,
        PortfolioLegV16, ResolvedPayoutReceiptV16, V16_MAX_PORTFOLIO_ASSETS_N,
    };
    use solana_program::program_error::ProgramError;
    use solana_program::pubkey::Pubkey;

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
    pub struct WrapperConfigV16 {
        pub admin: [u8; 32],
        pub collateral_mint: [u8; 32],
        pub secondary_collateral_mint: [u8; 32],
        pub base_unit_authority: [u8; 32],
        pub maintenance_fee_per_slot: u128,
        pub permissionless_market_init_fee: u128,
        pub trade_fee_base_bps: u64,
        pub permissionless_resolve_stale_slots: u64,
        pub force_close_delay_slots: u64,
        pub last_good_oracle_slot: u64,
        pub insurance_authority: [u8; 32],
        pub insurance_operator: [u8; 32],
        pub backing_bucket_authority: [u8; 32],
        pub asset_authority: [u8; 32],
        pub mark_authority: [u8; 32],
        pub insurance_withdraw_deposit_remaining: u128,
        pub insurance_withdraw_max_bps: u16,
        pub liquidation_cranker_fee_share_bps: u16,
        pub maintenance_cranker_fee_share_bps: u16,
        pub backing_trade_fee_bps_long: u16,
        pub unit_scale: u32,
        pub conf_filter_bps: u16,
        pub backing_trade_fee_bps_short: u16,
        pub insurance_withdraw_deposits_only: u8,
        pub oracle_mode: u8,
        pub oracle_leg_count: u8,
        pub oracle_leg_flags: u8,
        pub invert: u8,
        pub _padding0: u8,
        pub free_market_slot_count: u16,
        pub insurance_withdraw_cooldown_slots: u64,
        pub last_insurance_withdraw_slot: u64,
        pub max_staleness_secs: u64,
        pub hybrid_soft_stale_slots: u64,
        pub mark_ewma_e6: u64,
        pub mark_ewma_last_slot: u64,
        pub mark_ewma_halflife_slots: u64,
        pub mark_min_fee: u64,
        pub oracle_target_price_e6: u64,
        pub oracle_target_publish_time: i64,
        pub oracle_leg_feeds: [[u8; 32]; ORACLE_LEG_CAP],
        pub oracle_leg_prices_e6: [u64; ORACLE_LEG_CAP],
        pub oracle_leg_publish_times: [i64; ORACLE_LEG_CAP],
        pub backing_trade_fee_policy_count: u16,
        pub backing_trade_fee_insurance_share_bps_long: u16,
        pub backing_trade_fee_insurance_share_bps_short: u16,
        pub fee_redirect_to_market_0_bps: u16,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
    pub struct AssetOracleProfileV16 {
        pub oracle_mode: u8,
        pub oracle_leg_count: u8,
        pub oracle_leg_flags: u8,
        pub invert: u8,
        pub unit_scale: u32,
        pub conf_filter_bps: u16,
        pub backing_trade_fee_bps_long: u16,
        pub backing_trade_fee_bps_short: u16,
        pub backing_trade_fee_insurance_share_bps_long: u16,
        pub backing_trade_fee_insurance_share_bps_short: u16,
        pub _padding0: [u8; 6],
        pub insurance_authority: [u8; 32],
        pub insurance_operator: [u8; 32],
        pub backing_bucket_authority: [u8; 32],
        pub oracle_authority: [u8; 32],
        pub max_staleness_secs: u64,
        pub hybrid_soft_stale_slots: u64,
        pub mark_ewma_e6: u64,
        pub mark_ewma_last_slot: u64,
        pub mark_ewma_halflife_slots: u64,
        pub mark_min_fee: u64,
        pub oracle_target_price_e6: u64,
        pub oracle_target_publish_time: i64,
        pub last_good_oracle_slot: u64,
        pub oracle_leg_feeds: [[u8; 32]; ORACLE_LEG_CAP],
        pub oracle_leg_prices_e6: [u64; ORACLE_LEG_CAP],
        pub oracle_leg_publish_times: [i64; ORACLE_LEG_CAP],
    }

    /// Aggregate backing-domain accounting for an authority-controlled vault.
    /// This intentionally contains no per-depositor state; external authority
    /// programs can use these monotonic counters to run their own subledgers.
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
    pub struct BackingDomainLedgerAccountV16 {
        pub market_group: [u8; 32],
        pub authority: [u8; 32],
        pub total_principal_atoms: u128,
        pub total_deposited_atoms: u128,
        pub total_principal_withdrawn_atoms: u128,
        pub total_earnings_atoms: u128,
        pub total_earnings_withdrawn_atoms: u128,
        pub last_observed_bucket_earnings_atoms: u128,
        pub cumulative_loss_atoms: u128,
        pub cumulative_recovery_atoms: u128,
        pub last_observed_unavailable_principal_atoms: u128,
        pub domain: u16,
        pub _padding: [u8; 14],
    }

    /// Aggregate insurance accounting for an authority-controlled vault.
    /// This is not a user account and does not assign shares.
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
    pub struct InsuranceLedgerAccountV16 {
        pub market_group: [u8; 32],
        pub authority: [u8; 32],
        pub total_principal_atoms: u128,
        pub total_deposited_atoms: u128,
        pub total_withdrawn_atoms: u128,
        pub cumulative_profit_atoms: u128,
        pub cumulative_loss_atoms: u128,
        pub last_observed_insurance_atoms: u128,
    }

    /// Fork LP Vault registry — the per-(market_group) overlay that runs a
    /// per-depositor share layer on top of v16's `BackingDomainLedgerAccountV16`
    /// (Phase 2.B Tier 3, Workstream 4B; see lp_vault_design.md).
    ///
    /// LP shares themselves live in a standard SPL Token-2022 mint
    /// (`lp_mint`, authority = this registry PDA). This struct holds the
    /// vault config + bookkeeping. NAV is computed from the bound backing
    /// domain's `BackingDomainLedgerAccountV16` counters
    /// (`percolator::lp_vault::lp_vault_nav_atoms`), NEVER from a raw token
    /// balance — sign-off Note 2 donation-inflation defense.
    ///
    /// Layout pins every u128 to a 16-byte-aligned offset so the byte image
    /// is identical on host (u128 align 16) and SBF (u128 align 8) — the
    /// same discipline `BackingDomainLedgerAccountV16` uses. Size 160 bytes.
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
    pub struct LpVaultRegistryV16 {
        pub market_group: [u8; 32],                // 0..32
        pub lp_mint: [u8; 32],                     // 32..64
        pub total_lp_shares_outstanding: u128,     // 64..80
        pub insurance_fee_snapshot_atoms: u128,    // 80..96
        pub fee_distribution_total_atoms: u128,    // 96..112
        pub epoch: u64,                            // 112..120
        pub redemption_cooldown_slots: u64,        // 120..128
        pub fee_share_bps: u16,                    // 128..130
        pub oi_reservation_threshold_bps: u16,     // 130..132
        pub domain: u16,                           // 132..134
        pub paused: u8,                            // 134
        pub version: u8,                           // 135
        pub bump: u8,                              // 136
        pub mint_bump: u8,                         // 137
        pub _padding: [u8; 6],                     // 138..144
        pub _reserved: [u8; 16],                   // 144..160
    }

    /// Fork LP Vault queued-redemption escrow — one per (registry, redeemer)
    /// in-flight request. Shares are burned-to-escrow at RequestRedeem and
    /// finalized at ExecuteRedemption after the registry's cooldown. Size 96.
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
    pub struct LpRedemptionV16 {
        pub registry: [u8; 32],   // 0..32
        pub redeemer: [u8; 32],   // 32..64
        pub shares: u128,         // 64..80
        pub request_slot: u64,    // 80..88
        pub version: u8,          // 88
        pub bump: u8,             // 89
        pub _padding: [u8; 6],    // 90..96
    }

    const _: () = assert!(core::mem::size_of::<LpVaultRegistryV16>() == 160);
    const _: () = assert!(core::mem::size_of::<LpRedemptionV16>() == 96);

    pub type AssetOracleStorageV16 = [u8; ASSET_ORACLE_WRAPPER_LEN];
    pub type MarketViewMutV16<'a> = MarketGroupV16ViewMut<'a, AssetOracleStorageV16>;

    #[inline]
    fn read_u16(data: &[u8], off: usize) -> Result<u16, ProgramError> {
        let bytes: [u8; 2] = data
            .get(off..off + 2)
            .ok_or(PercolatorError::InvalidAccountLen)?
            .try_into()
            .unwrap();
        Ok(u16::from_le_bytes(bytes))
    }

    #[inline]
    fn read_u64(data: &[u8], off: usize) -> Result<u64, ProgramError> {
        let bytes: [u8; 8] = data
            .get(off..off + 8)
            .ok_or(PercolatorError::InvalidAccountLen)?
            .try_into()
            .unwrap();
        Ok(u64::from_le_bytes(bytes))
    }

    #[inline]
    fn write_header(data: &mut [u8], kind: u8) -> Result<(), ProgramError> {
        if data.len() < HEADER_LEN {
            return Err(PercolatorError::InvalidAccountLen.into());
        }
        data[0..8].copy_from_slice(&MAGIC.to_le_bytes());
        data[8..10].copy_from_slice(&VERSION.to_le_bytes());
        data[10] = kind;
        for b in data[11..HEADER_LEN].iter_mut() {
            *b = 0;
        }
        Ok(())
    }

    #[inline]
    fn check_header(data: &[u8], kind: u8) -> Result<(), ProgramError> {
        if data.len() < HEADER_LEN {
            return Err(PercolatorError::InvalidAccountLen.into());
        }
        if read_u64(data, 0)? != MAGIC {
            return Err(PercolatorError::NotInitialized.into());
        }
        if read_u16(data, 8)? != VERSION {
            return Err(PercolatorError::InvalidVersion.into());
        }
        if data[10] != kind {
            return Err(PercolatorError::InvalidAccountKind.into());
        }
        Ok(())
    }

    #[inline]
    pub fn check_portfolio_kind(data: &[u8]) -> Result<(), ProgramError> {
        check_header(data, KIND_PORTFOLIO)
    }

    #[inline]
    pub fn is_initialized(data: &[u8]) -> bool {
        data.len() >= HEADER_LEN && read_u64(data, 0).ok() == Some(MAGIC)
    }

    pub const fn backing_domain_ledger_account_len() -> usize {
        HEADER_LEN + core::mem::size_of::<BackingDomainLedgerAccountV16>()
    }

    pub const fn insurance_ledger_account_len() -> usize {
        HEADER_LEN + core::mem::size_of::<InsuranceLedgerAccountV16>()
    }

    #[inline]
    fn validate_backing_domain_ledger(
        ledger: &BackingDomainLedgerAccountV16,
    ) -> Result<(), ProgramError> {
        if ledger.market_group == [0u8; 32]
            || ledger.authority == [0u8; 32]
            || ledger._padding != [0u8; 14]
        {
            return Err(ProgramError::InvalidAccountData);
        }
        Ok(())
    }

    #[inline]
    pub fn read_backing_domain_ledger(
        data: &[u8],
    ) -> Result<BackingDomainLedgerAccountV16, ProgramError> {
        if data.len() < backing_domain_ledger_account_len() {
            return Err(PercolatorError::InvalidAccountLen.into());
        }
        check_header(data, KIND_BACKING_DOMAIN_LEDGER)?;
        let bytes = data
            .get(HEADER_LEN..backing_domain_ledger_account_len())
            .ok_or(PercolatorError::InvalidAccountLen)?;
        let ledger = bytemuck::pod_read_unaligned(bytes);
        validate_backing_domain_ledger(&ledger)?;
        Ok(ledger)
    }

    #[inline]
    pub fn write_backing_domain_ledger(
        data: &mut [u8],
        ledger: &BackingDomainLedgerAccountV16,
    ) -> Result<(), ProgramError> {
        if data.len() < backing_domain_ledger_account_len() {
            return Err(PercolatorError::InvalidAccountLen.into());
        }
        check_header(data, KIND_BACKING_DOMAIN_LEDGER)?;
        validate_backing_domain_ledger(ledger)?;
        data.get_mut(HEADER_LEN..backing_domain_ledger_account_len())
            .ok_or(PercolatorError::InvalidAccountLen)?
            .copy_from_slice(bytemuck::bytes_of(ledger));
        Ok(())
    }

    #[inline]
    pub fn init_backing_domain_ledger(
        data: &mut [u8],
        ledger: &BackingDomainLedgerAccountV16,
    ) -> Result<(), ProgramError> {
        if data.len() < backing_domain_ledger_account_len() {
            return Err(PercolatorError::InvalidAccountLen.into());
        }
        if is_initialized(data) {
            return Err(PercolatorError::AlreadyInitialized.into());
        }
        for b in data.iter_mut() {
            *b = 0;
        }
        write_header(data, KIND_BACKING_DOMAIN_LEDGER)?;
        write_backing_domain_ledger(data, ledger)
    }

    #[inline]
    fn validate_insurance_ledger(ledger: &InsuranceLedgerAccountV16) -> Result<(), ProgramError> {
        if ledger.market_group == [0u8; 32] || ledger.authority == [0u8; 32] {
            return Err(ProgramError::InvalidAccountData);
        }
        Ok(())
    }

    #[inline]
    pub fn read_insurance_ledger(data: &[u8]) -> Result<InsuranceLedgerAccountV16, ProgramError> {
        if data.len() < insurance_ledger_account_len() {
            return Err(PercolatorError::InvalidAccountLen.into());
        }
        check_header(data, KIND_INSURANCE_LEDGER)?;
        let bytes = data
            .get(HEADER_LEN..insurance_ledger_account_len())
            .ok_or(PercolatorError::InvalidAccountLen)?;
        let ledger = bytemuck::pod_read_unaligned(bytes);
        validate_insurance_ledger(&ledger)?;
        Ok(ledger)
    }

    #[inline]
    pub fn write_insurance_ledger(
        data: &mut [u8],
        ledger: &InsuranceLedgerAccountV16,
    ) -> Result<(), ProgramError> {
        if data.len() < insurance_ledger_account_len() {
            return Err(PercolatorError::InvalidAccountLen.into());
        }
        check_header(data, KIND_INSURANCE_LEDGER)?;
        validate_insurance_ledger(ledger)?;
        data.get_mut(HEADER_LEN..insurance_ledger_account_len())
            .ok_or(PercolatorError::InvalidAccountLen)?
            .copy_from_slice(bytemuck::bytes_of(ledger));
        Ok(())
    }

    #[inline]
    pub fn init_insurance_ledger(
        data: &mut [u8],
        ledger: &InsuranceLedgerAccountV16,
    ) -> Result<(), ProgramError> {
        if data.len() < insurance_ledger_account_len() {
            return Err(PercolatorError::InvalidAccountLen.into());
        }
        if is_initialized(data) {
            return Err(PercolatorError::AlreadyInitialized.into());
        }
        for b in data.iter_mut() {
            *b = 0;
        }
        write_header(data, KIND_INSURANCE_LEDGER)?;
        write_insurance_ledger(data, ledger)
    }

    // ── Fork LP Vault account helpers (Phase 2.B Tier 3, Workstream 4B) ──

    pub const fn lp_vault_registry_account_len() -> usize {
        HEADER_LEN + core::mem::size_of::<LpVaultRegistryV16>()
    }

    pub const fn lp_redemption_account_len() -> usize {
        HEADER_LEN + core::mem::size_of::<LpRedemptionV16>()
    }

    #[inline]
    fn validate_lp_vault_registry(reg: &LpVaultRegistryV16) -> Result<(), ProgramError> {
        if reg.market_group == [0u8; 32]
            || reg.lp_mint == [0u8; 32]
            || reg.version != LP_VAULT_VERSION
            || reg.paused > 1
            || reg.fee_share_bps > 10_000
            || reg.oi_reservation_threshold_bps > 10_000
            || reg._padding != [0u8; 6]
            || reg._reserved != [0u8; 16]
        {
            return Err(ProgramError::InvalidAccountData);
        }
        Ok(())
    }

    #[inline]
    pub fn read_lp_vault_registry(data: &[u8]) -> Result<LpVaultRegistryV16, ProgramError> {
        if data.len() < lp_vault_registry_account_len() {
            return Err(PercolatorError::InvalidAccountLen.into());
        }
        check_header(data, KIND_LP_VAULT_REGISTRY)?;
        let bytes = data
            .get(HEADER_LEN..lp_vault_registry_account_len())
            .ok_or(PercolatorError::InvalidAccountLen)?;
        let reg = bytemuck::pod_read_unaligned(bytes);
        validate_lp_vault_registry(&reg)?;
        Ok(reg)
    }

    #[inline]
    pub fn write_lp_vault_registry(
        data: &mut [u8],
        reg: &LpVaultRegistryV16,
    ) -> Result<(), ProgramError> {
        if data.len() < lp_vault_registry_account_len() {
            return Err(PercolatorError::InvalidAccountLen.into());
        }
        check_header(data, KIND_LP_VAULT_REGISTRY)?;
        validate_lp_vault_registry(reg)?;
        data.get_mut(HEADER_LEN..lp_vault_registry_account_len())
            .ok_or(PercolatorError::InvalidAccountLen)?
            .copy_from_slice(bytemuck::bytes_of(reg));
        Ok(())
    }

    #[inline]
    pub fn init_lp_vault_registry(
        data: &mut [u8],
        reg: &LpVaultRegistryV16,
    ) -> Result<(), ProgramError> {
        if data.len() < lp_vault_registry_account_len() {
            return Err(PercolatorError::InvalidAccountLen.into());
        }
        if is_initialized(data) {
            return Err(PercolatorError::AlreadyInitialized.into());
        }
        for b in data.iter_mut() {
            *b = 0;
        }
        write_header(data, KIND_LP_VAULT_REGISTRY)?;
        write_lp_vault_registry(data, reg)
    }

    #[inline]
    fn validate_lp_redemption(red: &LpRedemptionV16) -> Result<(), ProgramError> {
        if red.registry == [0u8; 32]
            || red.redeemer == [0u8; 32]
            || red.version != LP_VAULT_VERSION
            || red._padding != [0u8; 6]
        {
            return Err(ProgramError::InvalidAccountData);
        }
        Ok(())
    }

    #[inline]
    pub fn read_lp_redemption(data: &[u8]) -> Result<LpRedemptionV16, ProgramError> {
        if data.len() < lp_redemption_account_len() {
            return Err(PercolatorError::InvalidAccountLen.into());
        }
        check_header(data, KIND_LP_REDEMPTION)?;
        let bytes = data
            .get(HEADER_LEN..lp_redemption_account_len())
            .ok_or(PercolatorError::InvalidAccountLen)?;
        let red = bytemuck::pod_read_unaligned(bytes);
        validate_lp_redemption(&red)?;
        Ok(red)
    }

    #[inline]
    pub fn write_lp_redemption(data: &mut [u8], red: &LpRedemptionV16) -> Result<(), ProgramError> {
        if data.len() < lp_redemption_account_len() {
            return Err(PercolatorError::InvalidAccountLen.into());
        }
        check_header(data, KIND_LP_REDEMPTION)?;
        validate_lp_redemption(red)?;
        data.get_mut(HEADER_LEN..lp_redemption_account_len())
            .ok_or(PercolatorError::InvalidAccountLen)?
            .copy_from_slice(bytemuck::bytes_of(red));
        Ok(())
    }

    #[inline]
    pub fn init_lp_redemption(data: &mut [u8], red: &LpRedemptionV16) -> Result<(), ProgramError> {
        if data.len() < lp_redemption_account_len() {
            return Err(PercolatorError::InvalidAccountLen.into());
        }
        if is_initialized(data) {
            return Err(PercolatorError::AlreadyInitialized.into());
        }
        for b in data.iter_mut() {
            *b = 0;
        }
        write_header(data, KIND_LP_REDEMPTION)?;
        write_lp_redemption(data, red)
    }

    /// LP Vault Registry PDA: `["lp_vault", market_group]`.
    pub fn derive_lp_vault_registry(program_id: &Pubkey, market_group: &Pubkey) -> (Pubkey, u8) {
        Pubkey::find_program_address(
            &[crate::constants::LP_VAULT_REGISTRY_SEED, market_group.as_ref()],
            program_id,
        )
    }

    /// LP Vault Mint PDA: `["lp_vault_mint", market_group]`. Mint authority
    /// is the LP Vault Registry PDA.
    pub fn derive_lp_vault_mint(program_id: &Pubkey, market_group: &Pubkey) -> (Pubkey, u8) {
        Pubkey::find_program_address(
            &[crate::constants::LP_VAULT_MINT_SEED, market_group.as_ref()],
            program_id,
        )
    }

    /// LP redemption escrow PDA: `["lp_redemption", registry, redeemer]`.
    pub fn derive_lp_redemption(
        program_id: &Pubkey,
        registry: &Pubkey,
        redeemer: &Pubkey,
    ) -> (Pubkey, u8) {
        Pubkey::find_program_address(
            &[
                crate::constants::LP_REDEMPTION_SEED,
                registry.as_ref(),
                redeemer.as_ref(),
            ],
            program_id,
        )
    }

    /// LP Vault backing-domain ledger PDA: `["lp_backing_ledger", market, domain_le]`.
    pub fn derive_lp_backing_ledger(
        program_id: &Pubkey,
        market_group: &Pubkey,
        domain: u16,
    ) -> (Pubkey, u8) {
        Pubkey::find_program_address(
            &[
                crate::constants::LP_BACKING_LEDGER_SEED,
                market_group.as_ref(),
                &domain.to_le_bytes(),
            ],
            program_id,
        )
    }

    /// LP Vault shared redemption-escrow token account PDA: `["lp_escrow", market]`.
    pub fn derive_lp_escrow(program_id: &Pubkey, market_group: &Pubkey) -> (Pubkey, u8) {
        Pubkey::find_program_address(
            &[crate::constants::LP_ESCROW_SEED, market_group.as_ref()],
            program_id,
        )
    }

    /// Consume an LpRedemption PDA by zeroing its header magic — the
    /// DOUBLE-EXECUTE REPLAY GUARD. Keyed on PDA DATA, not lamports: an
    /// attacker can re-fund lamports to a closed account within the same tx
    /// (GC runs end-of-tx), but ONLY the program can rewrite the magic. After
    /// this, `read_lp_redemption` (→ `check_header`) returns `NotInitialized`,
    /// so any replayed `ExecuteRedemption` — even a second instruction in the
    /// SAME transaction — rejects before any payout.
    pub fn consume_lp_redemption(data: &mut [u8]) -> Result<(), ProgramError> {
        if data.len() < HEADER_LEN {
            return Err(PercolatorError::InvalidAccountLen.into());
        }
        // Zero the 8-byte MAGIC; check_header then fails NotInitialized.
        data[0..8].fill(0);
        Ok(())
    }

    // ── Fork NFT / B-3 TransferPortfolioOwnership (Phase 2.B Tier 3 / A.4) ─
    //
    // `NftRegistryV16` is a per-market PDA (seeds `["nft_registry",
    // market_group]`) that stores which NFT program ID is authorised to invoke
    // `TransferPortfolioOwnership` for portfolios in that market.  It mirrors
    // the `LpVaultRegistryV16` admin pattern exactly: a single admin-gated
    // `SetNftProgramId` instruction creates-or-updates the registry, and B-3
    // reads it with FAIL-CLOSED semantics (uninitialized = reject).
    //
    // Design: nft_design.md §7 / §7.0 — confirmed fits, no improvisation.

    /// Per-market NFT-program registry.  Size 72 bytes.
    ///
    /// Stored as `[HEADER_LEN=16][NftRegistryV16 POD=72]`.
    ///
    /// All fields are align-1 so the byte image is identical on host and SBF
    /// (same discipline as `BackingDomainLedgerAccountV16`).
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
    pub struct NftRegistryV16 {
        pub market_group: [u8; 32],   // 0..32
        pub nft_program_id: [u8; 32], // 32..64
        pub version: u8,              // 64
        pub bump: u8,                 // 65
        pub _padding: [u8; 6],        // 66..72
    }

    const _: () = assert!(core::mem::size_of::<NftRegistryV16>() == 72);

    pub const fn nft_registry_account_len() -> usize {
        HEADER_LEN + core::mem::size_of::<NftRegistryV16>()
    }

    #[inline]
    fn validate_nft_registry(reg: &NftRegistryV16) -> Result<(), ProgramError> {
        if reg.market_group == [0u8; 32]
            || reg.nft_program_id == [0u8; 32]
            || reg.version != NFT_REGISTRY_VERSION
            || reg._padding != [0u8; 6]
        {
            return Err(ProgramError::InvalidAccountData);
        }
        Ok(())
    }

    /// Read+validate the NftRegistryV16 from account data.
    ///
    /// FAIL-CLOSED gate: an uninitialized account (MAGIC absent) fails
    /// `check_header` with `NotInitialized` before any NFT-related logic runs.
    /// Never returns Ok for an account that was not written by
    /// `init_nft_registry`.
    #[inline]
    pub fn read_nft_registry(data: &[u8]) -> Result<NftRegistryV16, ProgramError> {
        if data.len() < nft_registry_account_len() {
            return Err(PercolatorError::InvalidAccountLen.into());
        }
        check_header(data, KIND_NFT_REGISTRY)?; // FAIL-CLOSED
        let bytes = data
            .get(HEADER_LEN..nft_registry_account_len())
            .ok_or(PercolatorError::InvalidAccountLen)?;
        let reg = bytemuck::pod_read_unaligned(bytes);
        validate_nft_registry(&reg)?;
        Ok(reg)
    }

    #[inline]
    pub fn write_nft_registry(
        data: &mut [u8],
        reg: &NftRegistryV16,
    ) -> Result<(), ProgramError> {
        if data.len() < nft_registry_account_len() {
            return Err(PercolatorError::InvalidAccountLen.into());
        }
        check_header(data, KIND_NFT_REGISTRY)?;
        validate_nft_registry(reg)?;
        data.get_mut(HEADER_LEN..nft_registry_account_len())
            .ok_or(PercolatorError::InvalidAccountLen)?
            .copy_from_slice(bytemuck::bytes_of(reg));
        Ok(())
    }

    #[inline]
    pub fn init_nft_registry(
        data: &mut [u8],
        reg: &NftRegistryV16,
    ) -> Result<(), ProgramError> {
        if data.len() < nft_registry_account_len() {
            return Err(PercolatorError::InvalidAccountLen.into());
        }
        if is_initialized(data) {
            return Err(PercolatorError::AlreadyInitialized.into());
        }
        for b in data.iter_mut() {
            *b = 0;
        }
        write_header(data, KIND_NFT_REGISTRY)?;
        write_nft_registry(data, reg)
    }

    /// NFT Registry PDA: `["nft_registry", market_group]`.
    pub fn derive_nft_registry(program_id: &Pubkey, market_group: &Pubkey) -> (Pubkey, u8) {
        Pubkey::find_program_address(
            &[NFT_REGISTRY_SEED, market_group.as_ref()],
            program_id,
        )
    }

    /// NFT mint-authority PDA: `["mint_authority", nft_program_id]` — i.e.
    /// `find_program_address([NFT_MINT_AUTHORITY_SEED], nft_program_id)`.
    ///
    /// SHARED CONTRACT with the percolator-nft program: the NFT program signs
    /// B-3 CPI with `invoke_signed([NFT_MINT_AUTHORITY_SEED, &[bump]])` using
    /// this PDA. A valid signer is cryptographic proof the call came from
    /// `nft_program_id` (a program can only sign its own PDAs via
    /// `invoke_signed`). See `constants::NFT_MINT_AUTHORITY_SEED`.
    pub fn derive_nft_mint_authority(nft_program_id: &Pubkey) -> (Pubkey, u8) {
        Pubkey::find_program_address(
            &[NFT_MINT_AUTHORITY_SEED],
            nft_program_id,
        )
    }

    #[inline]
    fn map_account_wire_error(_: V16Error) -> ProgramError {
        ProgramError::InvalidAccountData
    }

    #[inline]
    fn read_wrapper_config_from_bytes(data: &[u8]) -> Result<WrapperConfigV16, ProgramError> {
        let bytes = data
            .get(HEADER_LEN..HEADER_LEN + WRAPPER_CONFIG_LEN)
            .ok_or(PercolatorError::InvalidAccountLen)?;
        let config = bytemuck::pod_read_unaligned(bytes);
        validate_wrapper_config(&config)?;
        Ok(config)
    }

    #[cfg(not(target_os = "solana"))]
    fn read_wrapper_config_boxed_from_bytes(
        data: &[u8],
    ) -> Result<Box<WrapperConfigV16>, ProgramError> {
        let bytes = data
            .get(HEADER_LEN..HEADER_LEN + WRAPPER_CONFIG_LEN)
            .ok_or(PercolatorError::InvalidAccountLen)?;
        let mut boxed = Box::<WrapperConfigV16>::new_uninit();
        unsafe {
            core::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                boxed.as_mut_ptr() as *mut u8,
                WRAPPER_CONFIG_LEN,
            );
            let boxed = boxed.assume_init();
            validate_wrapper_config(boxed.as_ref())?;
            Ok(boxed)
        }
    }

    #[inline]
    fn validate_wrapper_config(config: &WrapperConfigV16) -> Result<(), ProgramError> {
        if config.collateral_mint == [0u8; 32]
            || (config.secondary_collateral_mint != [0u8; 32]
                && config.secondary_collateral_mint == config.collateral_mint)
        {
            return Err(ProgramError::InvalidAccountData);
        }
        if !insurance_withdraw_policy_shape_ok(
            config.insurance_withdraw_max_bps,
            config.insurance_withdraw_deposits_only,
            config.insurance_withdraw_cooldown_slots,
        ) || config.liquidation_cranker_fee_share_bps > 10_000
            || config.maintenance_cranker_fee_share_bps > 10_000
            || !backing_trade_fee_policy_shape_ok(
                config.backing_trade_fee_bps_long,
                config.backing_trade_fee_insurance_share_bps_long,
            )
            || !backing_trade_fee_policy_shape_ok(
                config.backing_trade_fee_bps_short,
                config.backing_trade_fee_insurance_share_bps_short,
            )
            || config.conf_filter_bps > 10_000
            || config.invert > 1
            || config._padding0 != 0
            || config.fee_redirect_to_market_0_bps > 10_000
            || config.oracle_leg_count as usize > ORACLE_LEG_CAP
            || (config.oracle_leg_flags & !ORACLE_LEG_FLAGS_MASK) != 0
        {
            return Err(ProgramError::InvalidAccountData);
        }
        let base_backing_fee_policy_count = (config.backing_trade_fee_bps_long != 0) as u16
            + (config.backing_trade_fee_bps_short != 0) as u16;
        if config.backing_trade_fee_policy_count < base_backing_fee_policy_count {
            return Err(ProgramError::InvalidAccountData);
        }

        match config.oracle_mode {
            ORACLE_MODE_MANUAL => {
                if config.oracle_leg_count != 0 || config.oracle_leg_flags != 0 {
                    return Err(ProgramError::InvalidAccountData);
                }
            }
            ORACLE_MODE_HYBRID_AFTER_HOURS => {
                if config.oracle_leg_count == 0
                    || config.max_staleness_secs == 0
                    || config.max_staleness_secs > crate::constants::MAX_ORACLE_STALENESS_SECS
                    || config.hybrid_soft_stale_slots == 0
                    || !valid_engine_oracle_price(config.mark_ewma_e6)
                    || !valid_engine_oracle_price(config.oracle_target_price_e6)
                    || !crate::oracle_v16::oracle_leg_config_ok(
                        config.oracle_leg_count,
                        config.oracle_leg_flags,
                        &config.oracle_leg_feeds,
                    )
                {
                    return Err(ProgramError::InvalidAccountData);
                }
            }
            ORACLE_MODE_EWMA_MARK => {
                if config.oracle_leg_count != 0
                    || config.oracle_leg_flags != 0
                    || config.invert != 0
                    || config.unit_scale != 0
                    || config.conf_filter_bps != 0
                    || config.max_staleness_secs != 0
                    || config.hybrid_soft_stale_slots != 0
                    || !valid_engine_oracle_price(config.mark_ewma_e6)
                    || !valid_engine_oracle_price(config.oracle_target_price_e6)
                    || config.mark_ewma_halflife_slots == 0
                    || config.oracle_leg_feeds.iter().any(|f| *f != [0u8; 32])
                    || config.oracle_leg_prices_e6.iter().any(|p| *p != 0)
                    || config.oracle_leg_publish_times.iter().any(|t| *t != 0)
                {
                    return Err(ProgramError::InvalidAccountData);
                }
            }
            ORACLE_MODE_AUTH_MARK => {
                if config.oracle_leg_count != 0
                    || config.oracle_leg_flags != 0
                    || config.invert != 0
                    || config.unit_scale != 0
                    || config.conf_filter_bps != 0
                    || config.max_staleness_secs != 0
                    || config.hybrid_soft_stale_slots != 0
                    || !valid_engine_oracle_price(config.mark_ewma_e6)
                    || !valid_engine_oracle_price(config.oracle_target_price_e6)
                    || config.mark_ewma_e6 != config.oracle_target_price_e6
                    || config.mark_ewma_halflife_slots != 0
                    || config.mark_min_fee != 0
                    || config.oracle_leg_feeds.iter().any(|f| *f != [0u8; 32])
                    || config.oracle_leg_prices_e6.iter().any(|p| *p != 0)
                    || config.oracle_leg_publish_times.iter().any(|t| *t != 0)
                {
                    return Err(ProgramError::InvalidAccountData);
                }
            }
            _ => return Err(ProgramError::InvalidAccountData),
        }

        Ok(())
    }

    #[inline]
    pub(super) fn insurance_withdraw_policy_shape_ok(
        max_bps: u16,
        deposits_only: u8,
        cooldown_slots: u64,
    ) -> bool {
        if max_bps > 10_000 || deposits_only > 1 {
            return false;
        }
        if max_bps == 0 || deposits_only != 0 {
            return true;
        }
        max_bps < 10_000 && cooldown_slots != 0
    }

    #[inline]
    pub(crate) fn backing_trade_fee_policy_shape_ok(
        fee_bps: u16,
        insurance_share_bps: u16,
    ) -> bool {
        fee_bps <= 10_000
            && insurance_share_bps <= 10_000
            && (fee_bps != 0 || insurance_share_bps == 0)
    }

    #[inline]
    fn valid_engine_oracle_price(price: u64) -> bool {
        price != 0 && price <= percolator::MAX_ORACLE_PRICE
    }

    #[inline]
    pub fn validate_asset_oracle_profile(
        profile: &AssetOracleProfileV16,
    ) -> Result<(), ProgramError> {
        if profile.conf_filter_bps > 10_000
            || !backing_trade_fee_policy_shape_ok(
                profile.backing_trade_fee_bps_long,
                profile.backing_trade_fee_insurance_share_bps_long,
            )
            || !backing_trade_fee_policy_shape_ok(
                profile.backing_trade_fee_bps_short,
                profile.backing_trade_fee_insurance_share_bps_short,
            )
            || profile.invert > 1
            || profile._padding0 != [0u8; 6]
            || profile.oracle_leg_count as usize > ORACLE_LEG_CAP
            || (profile.oracle_leg_flags & !ORACLE_LEG_FLAGS_MASK) != 0
        {
            return Err(ProgramError::InvalidAccountData);
        }

        match profile.oracle_mode {
            ORACLE_MODE_MANUAL => {
                if profile.oracle_leg_count != 0
                    || profile.oracle_leg_flags != 0
                    || profile.oracle_leg_feeds.iter().any(|f| *f != [0u8; 32])
                    || profile.oracle_leg_prices_e6.iter().any(|p| *p != 0)
                    || profile.oracle_leg_publish_times.iter().any(|t| *t != 0)
                {
                    return Err(ProgramError::InvalidAccountData);
                }
            }
            ORACLE_MODE_HYBRID_AFTER_HOURS => {
                if profile.oracle_leg_count == 0
                    || profile.max_staleness_secs == 0
                    || profile.max_staleness_secs > crate::constants::MAX_ORACLE_STALENESS_SECS
                    || profile.hybrid_soft_stale_slots == 0
                    || !valid_engine_oracle_price(profile.mark_ewma_e6)
                    || !valid_engine_oracle_price(profile.oracle_target_price_e6)
                    || profile.mark_ewma_halflife_slots == 0
                    || !crate::oracle_v16::oracle_leg_config_ok(
                        profile.oracle_leg_count,
                        profile.oracle_leg_flags,
                        &profile.oracle_leg_feeds,
                    )
                {
                    return Err(ProgramError::InvalidAccountData);
                }
            }
            ORACLE_MODE_EWMA_MARK => {
                if profile.oracle_leg_count != 0
                    || profile.oracle_leg_flags != 0
                    || profile.invert != 0
                    || profile.unit_scale != 0
                    || profile.conf_filter_bps != 0
                    || profile.max_staleness_secs != 0
                    || profile.hybrid_soft_stale_slots != 0
                    || !valid_engine_oracle_price(profile.mark_ewma_e6)
                    || !valid_engine_oracle_price(profile.oracle_target_price_e6)
                    || profile.mark_ewma_halflife_slots == 0
                    || profile.oracle_leg_feeds.iter().any(|f| *f != [0u8; 32])
                    || profile.oracle_leg_prices_e6.iter().any(|p| *p != 0)
                    || profile.oracle_leg_publish_times.iter().any(|t| *t != 0)
                {
                    return Err(ProgramError::InvalidAccountData);
                }
            }
            ORACLE_MODE_AUTH_MARK => {
                if profile.oracle_leg_count != 0
                    || profile.oracle_leg_flags != 0
                    || profile.invert != 0
                    || profile.unit_scale != 0
                    || profile.conf_filter_bps != 0
                    || profile.max_staleness_secs != 0
                    || profile.hybrid_soft_stale_slots != 0
                    || !valid_engine_oracle_price(profile.mark_ewma_e6)
                    || !valid_engine_oracle_price(profile.oracle_target_price_e6)
                    || profile.mark_ewma_e6 != profile.oracle_target_price_e6
                    || profile.mark_ewma_halflife_slots != 0
                    || profile.mark_min_fee != 0
                    || profile.oracle_leg_feeds.iter().any(|f| *f != [0u8; 32])
                    || profile.oracle_leg_prices_e6.iter().any(|p| *p != 0)
                    || profile.oracle_leg_publish_times.iter().any(|t| *t != 0)
                {
                    return Err(ProgramError::InvalidAccountData);
                }
            }
            _ => return Err(ProgramError::InvalidAccountData),
        }

        Ok(())
    }

    #[inline]
    pub fn manual_asset_oracle_profile(initial_price: u64, slot: u64) -> AssetOracleProfileV16 {
        AssetOracleProfileV16 {
            oracle_mode: ORACLE_MODE_MANUAL,
            oracle_leg_count: 0,
            oracle_leg_flags: 0,
            invert: 0,
            unit_scale: 0,
            conf_filter_bps: 0,
            backing_trade_fee_bps_long: 0,
            backing_trade_fee_bps_short: 0,
            backing_trade_fee_insurance_share_bps_long: 0,
            backing_trade_fee_insurance_share_bps_short: 0,
            _padding0: [0u8; 6],
            insurance_authority: [0u8; 32],
            insurance_operator: [0u8; 32],
            backing_bucket_authority: [0u8; 32],
            oracle_authority: [0u8; 32],
            max_staleness_secs: 0,
            hybrid_soft_stale_slots: 0,
            mark_ewma_e6: initial_price,
            mark_ewma_last_slot: slot,
            mark_ewma_halflife_slots: crate::constants::DEFAULT_MARK_EWMA_HALFLIFE_SLOTS,
            mark_min_fee: 0,
            oracle_target_price_e6: initial_price,
            oracle_target_publish_time: 0,
            last_good_oracle_slot: slot,
            oracle_leg_feeds: [[0u8; 32]; ORACLE_LEG_CAP],
            oracle_leg_prices_e6: [0u64; ORACLE_LEG_CAP],
            oracle_leg_publish_times: [0i64; ORACLE_LEG_CAP],
        }
    }

    pub fn asset_oracle_profile_from_config(config: &WrapperConfigV16) -> AssetOracleProfileV16 {
        AssetOracleProfileV16 {
            oracle_mode: config.oracle_mode,
            oracle_leg_count: config.oracle_leg_count,
            oracle_leg_flags: config.oracle_leg_flags,
            invert: config.invert,
            unit_scale: config.unit_scale,
            conf_filter_bps: config.conf_filter_bps,
            backing_trade_fee_bps_long: config.backing_trade_fee_bps_long,
            backing_trade_fee_bps_short: config.backing_trade_fee_bps_short,
            backing_trade_fee_insurance_share_bps_long: config
                .backing_trade_fee_insurance_share_bps_long,
            backing_trade_fee_insurance_share_bps_short: config
                .backing_trade_fee_insurance_share_bps_short,
            _padding0: [0u8; 6],
            insurance_authority: config.insurance_authority,
            insurance_operator: config.insurance_operator,
            backing_bucket_authority: config.backing_bucket_authority,
            oracle_authority: config.mark_authority,
            max_staleness_secs: config.max_staleness_secs,
            hybrid_soft_stale_slots: config.hybrid_soft_stale_slots,
            mark_ewma_e6: config.mark_ewma_e6,
            mark_ewma_last_slot: config.mark_ewma_last_slot,
            mark_ewma_halflife_slots: config.mark_ewma_halflife_slots,
            mark_min_fee: config.mark_min_fee,
            oracle_target_price_e6: config.oracle_target_price_e6,
            oracle_target_publish_time: config.oracle_target_publish_time,
            last_good_oracle_slot: config.last_good_oracle_slot,
            oracle_leg_feeds: config.oracle_leg_feeds,
            oracle_leg_prices_e6: config.oracle_leg_prices_e6,
            oracle_leg_publish_times: config.oracle_leg_publish_times,
        }
    }

    #[inline]
    fn write_wrapper_config_to_bytes(
        data: &mut [u8],
        config: &WrapperConfigV16,
    ) -> Result<(), ProgramError> {
        validate_wrapper_config(config)?;
        data.get_mut(HEADER_LEN..HEADER_LEN + WRAPPER_CONFIG_LEN)
            .ok_or(PercolatorError::InvalidAccountLen)?
            .copy_from_slice(bytemuck::bytes_of(config));
        Ok(())
    }

    #[inline]
    pub fn market_account_len_for_capacity(capacity: usize) -> Result<usize, ProgramError> {
        let dynamic_len = MarketGroupV16HeaderAccount::dynamic_market_group_account_len::<
            AssetOracleStorageV16,
        >(capacity)
        .map_err(map_account_wire_error)?;
        MARKET_GROUP_OFF
            .checked_add(dynamic_len)
            .ok_or(PercolatorError::InvalidAccountLen.into())
    }

    #[inline]
    pub fn portfolio_account_len_for_market_slots(
        max_market_slots: usize,
    ) -> Result<usize, ProgramError> {
        let market_slots =
            u32::try_from(max_market_slots).map_err(|_| PercolatorError::InvalidAccountLen)?;
        let domains =
            v16_domain_count_for_market_slots(market_slots).map_err(map_account_wire_error)?;
        HEADER_LEN
            .checked_add(PORTFOLIO_STATE_LEN)
            .and_then(|v| {
                domains
                    .checked_mul(PORTFOLIO_SOURCE_DOMAIN_LEN)
                    .and_then(|d| v.checked_add(d))
            })
            .ok_or(PercolatorError::InvalidAccountLen.into())
    }

    #[inline]
    pub fn market_slot_capacity(data: &[u8]) -> Result<usize, ProgramError> {
        if data.len() < MARKET_GROUP_OFF + MARKET_GROUP_LEN {
            return Err(PercolatorError::InvalidAccountLen.into());
        }
        MarketGroupV16HeaderAccount::dynamic_asset_slot_capacity_from_account_len::<
            AssetOracleStorageV16,
        >(data.len() - MARKET_GROUP_OFF)
        .map_err(map_account_wire_error)
    }

    #[inline]
    fn validate_market_dynamic_len(data: &[u8]) -> Result<usize, ProgramError> {
        let capacity = market_slot_capacity(data)?;
        MarketGroupV16HeaderAccount::validate_dynamic_market_group_account_len::<
            AssetOracleStorageV16,
        >(data.len() - MARKET_GROUP_OFF, capacity)
        .map_err(map_account_wire_error)?;
        Ok(capacity)
    }

    #[inline]
    fn dynamic_slot_offset(asset_index: usize) -> Result<usize, ProgramError> {
        Ok(MARKET_GROUP_OFF
            + MarketGroupV16HeaderAccount::dynamic_asset_slot_offset::<AssetOracleStorageV16>(
                asset_index,
            )
            .map_err(map_account_wire_error)?)
    }

    #[inline]
    fn asset_slot_range(asset_index: usize) -> Result<core::ops::Range<usize>, ProgramError> {
        let start = dynamic_slot_offset(asset_index)?
            .checked_add(ASSET_ORACLE_WRAPPER_LEN)
            .ok_or(PercolatorError::InvalidAccountLen)?;
        Ok(start..start + core::mem::size_of::<EngineAssetSlotV16Account>())
    }

    #[inline]
    fn asset_oracle_profile_range(
        data: &[u8],
        asset_index: usize,
    ) -> Result<core::ops::Range<usize>, ProgramError> {
        let capacity = market_slot_capacity(data)?;
        if asset_index >= capacity {
            return Err(PercolatorError::InvalidInstruction.into());
        }
        let start = dynamic_slot_offset(asset_index)?;
        Ok(start..start + ASSET_ORACLE_PROFILE_LEN)
    }

    pub fn read_asset_oracle_profile(
        data: &[u8],
        asset_index: usize,
    ) -> Result<AssetOracleProfileV16, ProgramError> {
        check_header(data, KIND_MARKET)?;
        let range = asset_oracle_profile_range(data, asset_index)?;
        let bytes = data.get(range).ok_or(PercolatorError::InvalidAccountLen)?;
        let profile: AssetOracleProfileV16 = bytemuck::pod_read_unaligned(bytes);
        validate_asset_oracle_profile(&profile)?;
        Ok(profile)
    }

    pub fn read_market_config_mode_and_capacity(
        data: &[u8],
    ) -> Result<(WrapperConfigV16, MarketModeV16, usize, usize), ProgramError> {
        check_header(data, KIND_MARKET)?;
        validate_market_dynamic_len(data)?;
        let config = read_wrapper_config_from_bytes(data)?;
        let header = market_header(data)?;
        let engine_config = header
            .config
            .try_to_runtime()
            .map_err(map_account_wire_error)?;
        Ok((
            config,
            decode_market_mode(header.mode)?,
            engine_config.max_market_slots as usize,
            header.asset_slot_capacity.get() as usize,
        ))
    }

    pub fn write_asset_oracle_profile(
        data: &mut [u8],
        asset_index: usize,
        profile: &AssetOracleProfileV16,
    ) -> Result<(), ProgramError> {
        check_header(data, KIND_MARKET)?;
        validate_asset_oracle_profile(profile)?;
        let range = asset_oracle_profile_range(data, asset_index)?;
        data.get_mut(range)
            .ok_or(PercolatorError::InvalidAccountLen)?
            .copy_from_slice(bytemuck::bytes_of(profile));
        Ok(())
    }

    pub fn write_wrapper_config(
        data: &mut [u8],
        config: &WrapperConfigV16,
    ) -> Result<(), ProgramError> {
        check_header(data, KIND_MARKET)?;
        write_wrapper_config_to_bytes(data, config)
    }

    pub fn market_view_mut(
        data: &mut [u8],
    ) -> Result<(WrapperConfigV16, MarketViewMutV16<'_>), ProgramError> {
        if data.len() < MIN_MARKET_ACCOUNT_LEN {
            return Err(PercolatorError::InvalidAccountLen.into());
        }
        check_header(data, KIND_MARKET)?;
        let config = read_wrapper_config_from_bytes(data)?;
        let capacity = validate_market_dynamic_len(data)?;
        let state_data = data
            .get_mut(MARKET_GROUP_OFF..)
            .ok_or(PercolatorError::InvalidAccountLen)?;
        let header_len = core::mem::size_of::<MarketGroupV16HeaderAccount>();
        let (header_bytes, market_bytes) = state_data.split_at_mut(header_len);
        let header = bytemuck::try_from_bytes_mut::<MarketGroupV16HeaderAccount>(header_bytes)
            .map_err(|_| ProgramError::InvalidAccountData)?;
        let configured = header.config.max_market_slots.get() as usize;
        if configured == 0 || configured > capacity {
            return Err(ProgramError::InvalidAccountData);
        }
        let used_len = capacity
            .checked_mul(core::mem::size_of::<Market<AssetOracleStorageV16>>())
            .ok_or(PercolatorError::InvalidAccountLen)?;
        let markets_bytes = market_bytes
            .get_mut(..used_len)
            .ok_or(PercolatorError::InvalidAccountLen)?;
        let markets =
            bytemuck::try_cast_slice_mut::<u8, Market<AssetOracleStorageV16>>(markets_bytes)
                .map_err(|_| ProgramError::InvalidAccountData)?;
        Ok((config, MarketGroupV16ViewMut::new(header, markets)))
    }

    pub fn activate_dynamic_asset_slot(
        data: &mut [u8],
        asset_index: usize,
        now_slot: u64,
        initial_price: u64,
        insurance_authority: [u8; 32],
        insurance_operator: [u8; 32],
        backing_bucket_authority: [u8; 32],
        oracle_authority: [u8; 32],
    ) -> Result<AssetOracleProfileV16, ProgramError> {
        if insurance_authority == [0u8; 32]
            || insurance_operator == [0u8; 32]
            || backing_bucket_authority == [0u8; 32]
            || oracle_authority == [0u8; 32]
        {
            return Err(PercolatorError::InvalidInstruction.into());
        }
        check_header(data, KIND_MARKET)?;
        let capacity = validate_market_dynamic_len(data)?;
        if asset_index >= capacity || asset_index > u32::MAX as usize {
            return Err(PercolatorError::InvalidInstruction.into());
        }
        let mut header = *market_header(data)?;
        let engine_config = header
            .config
            .try_to_runtime()
            .map_err(map_account_wire_error)?;
        let old_n = engine_config.max_market_slots as usize;
        if asset_index != old_n {
            return Err(PercolatorError::InvalidInstruction.into());
        }
        let new_n = asset_index
            .checked_add(1)
            .ok_or(PercolatorError::EngineArithmeticOverflow)?;
        let mut slot = *asset_slot_wire(data, asset_index)?;
        header
            .grow_asset_slot_capacity_not_atomic(capacity as u32, new_n as u32)
            .map_err(map_account_wire_error)?;
        header
            .activate_empty_asset_slot_not_atomic(
                asset_index as u32,
                &mut slot,
                initial_price,
                now_slot,
            )
            .map_err(map_account_wire_error)?;
        slot.insurance_domain_budget_long = percolator::V16PodU128::new(0);
        slot.insurance_domain_budget_short = percolator::V16PodU128::new(0);
        slot.insurance_domain_spent_long = percolator::V16PodU128::new(0);
        slot.insurance_domain_spent_short = percolator::V16PodU128::new(0);
        *market_header_mut(data)? = header;
        *asset_slot_wire_mut(data, asset_index)? = slot;
        let mut profile = manual_asset_oracle_profile(initial_price, now_slot);
        profile.insurance_authority = insurance_authority;
        profile.insurance_operator = insurance_operator;
        profile.backing_bucket_authority = backing_bucket_authority;
        profile.oracle_authority = oracle_authority;
        Ok(profile)
    }

    fn init_asset_oracle_profiles(
        data: &mut [u8],
        profile: &AssetOracleProfileV16,
    ) -> Result<(), ProgramError> {
        validate_asset_oracle_profile(profile)?;
        let bytes = bytemuck::bytes_of(profile);
        let capacity = market_slot_capacity(data)?;
        let mut i = 0usize;
        while i < capacity {
            let range = asset_oracle_profile_range(data, i)?;
            data.get_mut(range)
                .ok_or(PercolatorError::InvalidAccountLen)?
                .copy_from_slice(bytes);
            i += 1;
        }
        Ok(())
    }

    #[inline]
    fn market_header(data: &[u8]) -> Result<&MarketGroupV16HeaderAccount, ProgramError> {
        let bytes = data
            .get(
                MARKET_GROUP_OFF
                    ..MARKET_GROUP_OFF + core::mem::size_of::<MarketGroupV16HeaderAccount>(),
            )
            .ok_or(PercolatorError::InvalidAccountLen)?;
        bytemuck::try_from_bytes(bytes).map_err(|_| ProgramError::InvalidAccountData)
    }

    #[inline]
    fn market_header_mut(
        data: &mut [u8],
    ) -> Result<&mut MarketGroupV16HeaderAccount, ProgramError> {
        let bytes = data
            .get_mut(
                MARKET_GROUP_OFF
                    ..MARKET_GROUP_OFF + core::mem::size_of::<MarketGroupV16HeaderAccount>(),
            )
            .ok_or(PercolatorError::InvalidAccountLen)?;
        bytemuck::try_from_bytes_mut(bytes).map_err(|_| ProgramError::InvalidAccountData)
    }

    #[inline]
    fn asset_slot_wire(
        data: &[u8],
        asset_index: usize,
    ) -> Result<&EngineAssetSlotV16Account, ProgramError> {
        let range = asset_slot_range(asset_index)?;
        let bytes = data.get(range).ok_or(PercolatorError::InvalidAccountLen)?;
        bytemuck::try_from_bytes(bytes).map_err(|_| ProgramError::InvalidAccountData)
    }

    #[inline]
    fn asset_slot_wire_mut(
        data: &mut [u8],
        asset_index: usize,
    ) -> Result<&mut EngineAssetSlotV16Account, ProgramError> {
        let range = asset_slot_range(asset_index)?;
        let bytes = data
            .get_mut(range)
            .ok_or(PercolatorError::InvalidAccountLen)?;
        bytemuck::try_from_bytes_mut(bytes).map_err(|_| ProgramError::InvalidAccountData)
    }

    #[inline]
    pub fn portfolio_wire(data: &[u8]) -> Result<&PortfolioAccountV16Account, ProgramError> {
        let bytes = data
            .get(HEADER_LEN..HEADER_LEN + core::mem::size_of::<PortfolioAccountV16Account>())
            .ok_or(PercolatorError::InvalidAccountLen)?;
        bytemuck::try_from_bytes(bytes).map_err(|_| ProgramError::InvalidAccountData)
    }

    #[inline]
    pub fn portfolio_wire_mut(
        data: &mut [u8],
    ) -> Result<&mut PortfolioAccountV16Account, ProgramError> {
        let bytes = data
            .get_mut(HEADER_LEN..HEADER_LEN + core::mem::size_of::<PortfolioAccountV16Account>())
            .ok_or(PercolatorError::InvalidAccountLen)?;
        bytemuck::try_from_bytes_mut(bytes).map_err(|_| ProgramError::InvalidAccountData)
    }

    #[inline]
    fn decode_market_mode(v: u8) -> Result<MarketModeV16, ProgramError> {
        match v {
            0 => Ok(MarketModeV16::Live),
            1 => Ok(MarketModeV16::Resolved),
            2 => Ok(MarketModeV16::Recovery),
            _ => Err(ProgramError::InvalidAccountData),
        }
    }

    #[cfg(not(target_os = "solana"))]
    fn market_slots_from_wire(
        data: &[u8],
        capacity: usize,
        slot_count: usize,
    ) -> Result<Vec<EngineAssetSlotV16Account>, ProgramError> {
        if slot_count > capacity {
            return Err(PercolatorError::InvalidAccountLen.into());
        }
        let mut slots = Vec::with_capacity(slot_count);
        let mut i = 0usize;
        while i < slot_count {
            slots.push(*asset_slot_wire(data, i)?);
            i += 1;
        }
        Ok(slots)
    }

    #[cfg(not(target_os = "solana"))]
    fn market_from_wire_boxed(
        data: &[u8],
        read_full_capacity: bool,
    ) -> Result<Box<MarketGroupV16>, ProgramError> {
        let wire = market_header(data)?;
        let capacity = validate_market_dynamic_len(data)?;
        let configured = wire.config.max_market_slots.get() as usize;
        if configured > capacity {
            return Err(ProgramError::InvalidAccountData);
        }
        let slot_count = if read_full_capacity {
            capacity
        } else {
            configured
        };
        let slots = market_slots_from_wire(data, capacity, slot_count)?;
        let mut decode_wire = *wire;
        if !read_full_capacity {
            // Instruction handlers operate on the append-only configured prefix.
            // The allocated tail is not part of the live engine state until an
            // asset append activates exactly that slot, so hot paths must not
            // deserialize or serialize it.
            decode_wire.asset_slot_capacity =
                percolator::V16PodU32::new(wire.config.max_market_slots.get());
        }
        let group = decode_wire
            .try_to_runtime_with_market_slots(&slots)
            .map_err(map_account_wire_error)?;
        Ok(Box::new(group))
    }

    #[cfg(not(target_os = "solana"))]
    fn write_market_wire(data: &mut [u8], group: &MarketGroupV16) -> Result<(), ProgramError> {
        let capacity = validate_market_dynamic_len(data)?;
        if capacity < group.config.max_market_slots as usize {
            return Err(ProgramError::InvalidAccountData);
        }
        *market_header_mut(data)? =
            MarketGroupV16HeaderAccount::from_runtime_with_capacity(group, capacity)
                .map_err(map_account_wire_error)?;
        let mut i = 0;
        let n = group.config.max_market_slots as usize;
        while i < n {
            *asset_slot_wire_mut(data, i)? =
                EngineAssetSlotV16Account::from_runtime_group_slot(group, i)
                    .map_err(map_account_wire_error)?;
            i += 1;
        }
        Ok(())
    }

    #[cfg(not(target_os = "solana"))]
    fn portfolio_source_domain_capacity(data: &[u8]) -> Result<usize, ProgramError> {
        if data.len() < HEADER_LEN + PORTFOLIO_STATE_LEN {
            return Err(PercolatorError::InvalidAccountLen.into());
        }
        let trailing = data.len() - HEADER_LEN - PORTFOLIO_STATE_LEN;
        if trailing % PORTFOLIO_SOURCE_DOMAIN_LEN != 0 {
            return Err(ProgramError::InvalidAccountData);
        }
        Ok(trailing / PORTFOLIO_SOURCE_DOMAIN_LEN)
    }

    #[cfg(not(target_os = "solana"))]
    fn portfolio_source_domain_range(
        domain: usize,
    ) -> Result<core::ops::Range<usize>, ProgramError> {
        let offset = domain
            .checked_mul(PORTFOLIO_SOURCE_DOMAIN_LEN)
            .and_then(|v| v.checked_add(HEADER_LEN + PORTFOLIO_STATE_LEN))
            .ok_or(PercolatorError::InvalidAccountLen)?;
        Ok(offset..offset + PORTFOLIO_SOURCE_DOMAIN_LEN)
    }

    #[cfg(not(target_os = "solana"))]
    fn portfolio_source_domain_wire(
        data: &[u8],
        domain: usize,
    ) -> Result<&PortfolioSourceDomainV16Account, ProgramError> {
        let capacity = portfolio_source_domain_capacity(data)?;
        if domain >= capacity {
            return Err(PercolatorError::InvalidAccountLen.into());
        }
        let bytes = data
            .get(portfolio_source_domain_range(domain)?)
            .ok_or(PercolatorError::InvalidAccountLen)?;
        bytemuck::try_from_bytes(bytes).map_err(|_| ProgramError::InvalidAccountData)
    }

    #[cfg(not(target_os = "solana"))]
    fn portfolio_source_domain_wire_mut(
        data: &mut [u8],
        domain: usize,
    ) -> Result<&mut PortfolioSourceDomainV16Account, ProgramError> {
        let capacity = portfolio_source_domain_capacity(data)?;
        if domain >= capacity {
            return Err(PercolatorError::InvalidAccountLen.into());
        }
        let bytes = data
            .get_mut(portfolio_source_domain_range(domain)?)
            .ok_or(PercolatorError::InvalidAccountLen)?;
        bytemuck::try_from_bytes_mut(bytes).map_err(|_| ProgramError::InvalidAccountData)
    }

    #[cfg(not(target_os = "solana"))]
    fn portfolio_source_domains_from_wire(
        data: &[u8],
        source_domain_count: Option<usize>,
    ) -> Result<Vec<PortfolioSourceDomainV16Account>, ProgramError> {
        let capacity = portfolio_source_domain_capacity(data)?;
        if let Some(count) = source_domain_count {
            if count > capacity {
                return Err(PercolatorError::InvalidAccountLen.into());
            }
            let mut out = Vec::with_capacity(count);
            let mut d = 0usize;
            while d < count {
                out.push(*portfolio_source_domain_wire(data, d)?);
                d += 1;
            }
            return Ok(out);
        }
        #[cfg(target_os = "solana")]
        let capacity = {
            let mut used = 0usize;
            let mut d = 0usize;
            while d < capacity {
                let source = *portfolio_source_domain_wire(data, d)?;
                if bytemuck::bytes_of(&source).iter().any(|b| *b != 0) {
                    used = d.checked_add(1).ok_or(PercolatorError::InvalidAccountLen)?;
                }
                d += 1;
            }
            used
        };
        let mut out = Vec::with_capacity(capacity);
        let mut d = 0usize;
        while d < capacity {
            out.push(*portfolio_source_domain_wire(data, d)?);
            d += 1;
        }
        Ok(out)
    }

    #[inline]
    #[cfg(not(target_os = "solana"))]
    fn decode_bool(v: u8) -> Result<bool, ProgramError> {
        match v {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(ProgramError::InvalidAccountData),
        }
    }

    #[cfg(not(target_os = "solana"))]
    pub fn empty_portfolio_boxed(
        header: ProvenanceHeaderV16,
        last_fee_slot: u64,
        source_domain_count: usize,
    ) -> Result<Box<PortfolioAccountV16>, ProgramError> {
        let mut source_claim_market_id = Vec::with_capacity(source_domain_count);
        let mut source_claim_bound_num = Vec::with_capacity(source_domain_count);
        let mut source_claim_liened_num = Vec::with_capacity(source_domain_count);
        let mut source_claim_counterparty_liened_num = Vec::with_capacity(source_domain_count);
        let mut source_claim_insurance_liened_num = Vec::with_capacity(source_domain_count);
        let mut source_lien_effective_reserved = Vec::with_capacity(source_domain_count);
        let mut source_lien_counterparty_backing_num = Vec::with_capacity(source_domain_count);
        let mut source_lien_insurance_backing_num = Vec::with_capacity(source_domain_count);
        let mut source_lien_fee_last_slot = Vec::with_capacity(source_domain_count);
        let mut source_claim_impaired_num = Vec::with_capacity(source_domain_count);
        let mut source_lien_impaired_effective_reserved = Vec::with_capacity(source_domain_count);
        let mut d = 0usize;
        while d < source_domain_count {
            source_claim_market_id.push(0);
            source_claim_bound_num.push(0);
            source_claim_liened_num.push(0);
            source_claim_counterparty_liened_num.push(0);
            source_claim_insurance_liened_num.push(0);
            source_lien_effective_reserved.push(0);
            source_lien_counterparty_backing_num.push(0);
            source_lien_insurance_backing_num.push(0);
            source_lien_fee_last_slot.push(0);
            source_claim_impaired_num.push(0);
            source_lien_impaired_effective_reserved.push(0);
            d += 1;
        }

        let mut boxed = Box::<PortfolioAccountV16>::new_uninit();
        let ptr = boxed.as_mut_ptr();
        unsafe {
            core::ptr::addr_of_mut!((*ptr).provenance_header).write(header);
            core::ptr::addr_of_mut!((*ptr).owner).write(header.owner);
            core::ptr::addr_of_mut!((*ptr).capital).write(0);
            core::ptr::addr_of_mut!((*ptr).pnl).write(0);
            core::ptr::addr_of_mut!((*ptr).reserved_pnl).write(0);
            core::ptr::addr_of_mut!((*ptr).source_claim_market_id).write(source_claim_market_id);
            core::ptr::addr_of_mut!((*ptr).source_claim_bound_num).write(source_claim_bound_num);
            core::ptr::addr_of_mut!((*ptr).source_claim_liened_num).write(source_claim_liened_num);
            core::ptr::addr_of_mut!((*ptr).source_claim_counterparty_liened_num)
                .write(source_claim_counterparty_liened_num);
            core::ptr::addr_of_mut!((*ptr).source_claim_insurance_liened_num)
                .write(source_claim_insurance_liened_num);
            core::ptr::addr_of_mut!((*ptr).source_lien_effective_reserved)
                .write(source_lien_effective_reserved);
            core::ptr::addr_of_mut!((*ptr).source_lien_counterparty_backing_num)
                .write(source_lien_counterparty_backing_num);
            core::ptr::addr_of_mut!((*ptr).source_lien_insurance_backing_num)
                .write(source_lien_insurance_backing_num);
            core::ptr::addr_of_mut!((*ptr).source_lien_fee_last_slot)
                .write(source_lien_fee_last_slot);
            core::ptr::addr_of_mut!((*ptr).source_claim_impaired_num)
                .write(source_claim_impaired_num);
            core::ptr::addr_of_mut!((*ptr).source_lien_impaired_effective_reserved)
                .write(source_lien_impaired_effective_reserved);
            core::ptr::addr_of_mut!((*ptr).fee_credits).write(0);
            core::ptr::addr_of_mut!((*ptr).cancel_deposit_escrow).write(0);
            core::ptr::addr_of_mut!((*ptr).last_fee_slot).write(last_fee_slot);
            core::ptr::addr_of_mut!((*ptr).active_bitmap).write(percolator::active_bitmap_empty());
            core::ptr::addr_of_mut!((*ptr).legs)
                .write([PortfolioLegV16::EMPTY; V16_MAX_PORTFOLIO_ASSETS_N]);
            core::ptr::addr_of_mut!((*ptr).health_cert).write(HealthCertV16 {
                certified_equity: 0,
                certified_initial_req: 0,
                certified_maintenance_req: 0,
                certified_liq_deficit: 0,
                certified_worst_case_loss: 0,
                cert_oracle_epoch: 0,
                cert_funding_epoch: 0,
                cert_risk_epoch: 0,
                cert_asset_set_epoch: 0,
                active_bitmap_at_cert: percolator::active_bitmap_empty(),
                valid: false,
            });
            core::ptr::addr_of_mut!((*ptr).stale_state).write(false);
            core::ptr::addr_of_mut!((*ptr).b_stale_state).write(false);
            core::ptr::addr_of_mut!((*ptr).rebalance_lock).write(false);
            core::ptr::addr_of_mut!((*ptr).liquidation_lock).write(false);
            core::ptr::addr_of_mut!((*ptr).close_progress).write(CloseProgressLedgerV16::EMPTY);
            core::ptr::addr_of_mut!((*ptr).resolved_payout_receipt)
                .write(ResolvedPayoutReceiptV16::EMPTY);
            Ok(boxed.assume_init())
        }
    }

    #[cfg(not(target_os = "solana"))]
    fn portfolio_from_wire_boxed(
        data: &[u8],
        source_domain_count: Option<usize>,
    ) -> Result<Box<PortfolioAccountV16>, ProgramError> {
        let wire = portfolio_wire(data)?;
        let source_domains = portfolio_source_domains_from_wire(data, source_domain_count)?;
        let header = wire
            .provenance_header
            .try_to_runtime()
            .map_err(map_account_wire_error)?;
        let mut account =
            empty_portfolio_boxed(header, wire.last_fee_slot.get(), source_domains.len())?;
        account.owner = wire.owner;
        account.capital = wire.capital.get();
        account.pnl = wire.pnl.get();
        account.reserved_pnl = wire.reserved_pnl.get();
        account.fee_credits = wire.fee_credits.get();
        account.cancel_deposit_escrow = wire.cancel_deposit_escrow.get();
        account.active_bitmap = wire.active_bitmap.map(|v| v.get());
        let mut i = 0usize;
        while i < V16_MAX_PORTFOLIO_ASSETS_N {
            account.legs[i] = wire.legs[i]
                .try_to_runtime()
                .map_err(map_account_wire_error)?;
            i += 1;
        }
        account.health_cert = wire
            .health_cert
            .try_to_runtime()
            .map_err(map_account_wire_error)?;
        account.stale_state = decode_bool(wire.stale_state)?;
        account.b_stale_state = decode_bool(wire.b_stale_state)?;
        account.rebalance_lock = decode_bool(wire.rebalance_lock)?;
        account.liquidation_lock = decode_bool(wire.liquidation_lock)?;
        account.close_progress = wire
            .close_progress
            .try_to_runtime()
            .map_err(map_account_wire_error)?;
        account.resolved_payout_receipt = wire
            .resolved_payout_receipt
            .try_to_runtime()
            .map_err(map_account_wire_error)?;
        let mut d = 0usize;
        while d < source_domains.len() {
            let source = source_domains[d];
            account.source_claim_market_id[d] = source.source_claim_market_id.get();
            account.source_claim_bound_num[d] = source.source_claim_bound_num.get();
            account.source_claim_liened_num[d] = source.source_claim_liened_num.get();
            account.source_claim_counterparty_liened_num[d] =
                source.source_claim_counterparty_liened_num.get();
            account.source_claim_insurance_liened_num[d] =
                source.source_claim_insurance_liened_num.get();
            account.source_lien_effective_reserved[d] = source.source_lien_effective_reserved.get();
            account.source_lien_counterparty_backing_num[d] =
                source.source_lien_counterparty_backing_num.get();
            account.source_lien_insurance_backing_num[d] =
                source.source_lien_insurance_backing_num.get();
            account.source_lien_fee_last_slot[d] = source.source_lien_fee_last_slot.get();
            account.source_claim_impaired_num[d] = source.source_claim_impaired_num.get();
            account.source_lien_impaired_effective_reserved[d] =
                source.source_lien_impaired_effective_reserved.get();
            d += 1;
        }
        Ok(account)
    }

    #[cfg(not(target_os = "solana"))]
    fn write_portfolio_wire(
        data: &mut [u8],
        account: &PortfolioAccountV16,
    ) -> Result<(), ProgramError> {
        let domain_capacity = portfolio_source_domain_capacity(data)?;
        if domain_capacity < account.source_claim_market_id.len() {
            return Err(PercolatorError::InvalidAccountLen.into());
        }
        *portfolio_wire_mut(data)? = PortfolioAccountV16Account::from_runtime(account);
        let source_domains = PortfolioAccountV16Account::source_domains_from_runtime(account)
            .map_err(map_account_wire_error)?;
        let mut d = 0usize;
        while d < source_domains.len() {
            *portfolio_source_domain_wire_mut(data, d)? = source_domains[d];
            d += 1;
        }
        Ok(())
    }

    pub fn portfolio_view_mut_for_market_slots(
        data: &mut [u8],
        max_market_slots: usize,
    ) -> Result<PortfolioV16ViewMut<'_>, ProgramError> {
        check_header(data, KIND_PORTFOLIO)?;
        let market_slots =
            u32::try_from(max_market_slots).map_err(|_| PercolatorError::InvalidAccountLen)?;
        let domain_count =
            v16_domain_count_for_market_slots(market_slots).map_err(map_account_wire_error)?;
        let required = HEADER_LEN
            .checked_add(PORTFOLIO_STATE_LEN)
            .and_then(|v| {
                domain_count
                    .checked_mul(PORTFOLIO_SOURCE_DOMAIN_LEN)
                    .and_then(|d| v.checked_add(d))
            })
            .ok_or(PercolatorError::InvalidAccountLen)?;
        if data.len() < required {
            return Err(PercolatorError::InvalidAccountLen.into());
        }
        let body = data
            .get_mut(HEADER_LEN..)
            .ok_or(PercolatorError::InvalidAccountLen)?;
        let (portfolio_bytes, source_bytes) = body.split_at_mut(PORTFOLIO_STATE_LEN);
        let header = bytemuck::try_from_bytes_mut::<PortfolioAccountV16Account>(portfolio_bytes)
            .map_err(|_| ProgramError::InvalidAccountData)?;
        let source_len = domain_count
            .checked_mul(PORTFOLIO_SOURCE_DOMAIN_LEN)
            .ok_or(PercolatorError::InvalidAccountLen)?;
        let source_bytes = source_bytes
            .get_mut(..source_len)
            .ok_or(PercolatorError::InvalidAccountLen)?;
        let source_domains =
            bytemuck::try_cast_slice_mut::<u8, PortfolioSourceDomainV16Account>(source_bytes)
                .map_err(|_| ProgramError::InvalidAccountData)?;
        Ok(PortfolioV16ViewMut::new(header, source_domains))
    }

    pub fn init_market_account_zero_copy(
        data: &mut [u8],
        config: &WrapperConfigV16,
        engine_config: V16Config,
        market_group_id: [u8; 32],
        initial_price: u64,
        init_slot: u64,
    ) -> Result<(), ProgramError> {
        if data.len() < MIN_MARKET_ACCOUNT_LEN {
            return Err(PercolatorError::InvalidAccountLen.into());
        }
        if is_initialized(data) {
            return Err(PercolatorError::AlreadyInitialized.into());
        }
        for b in data.iter_mut() {
            *b = 0;
        }
        write_header(data, KIND_MARKET)?;
        write_wrapper_config_to_bytes(data, config)?;
        let base_profile = asset_oracle_profile_from_config(config);
        init_asset_oracle_profiles(data, &base_profile)?;
        let capacity = market_slot_capacity(data)?;
        if capacity > u32::MAX as usize {
            return Err(PercolatorError::InvalidAccountLen.into());
        }
        let mut header = MarketGroupV16HeaderAccount::new_dynamic(
            market_group_id,
            engine_config,
            capacity as u32,
            init_slot,
        )
        .map_err(map_account_wire_error)?;
        let configured = header.config.max_market_slots.get() as usize;
        if configured == 0 || configured > capacity {
            return Err(ProgramError::InvalidAccountData);
        }
        let next_market_id = (configured as u64)
            .checked_add(1)
            .ok_or(PercolatorError::EngineArithmeticOverflow)?;
        header.next_market_id = V16PodU64::new(next_market_id);
        *market_header_mut(data)? = header;

        let mut i = 0usize;
        while i < configured {
            let market_id = (i as u64)
                .checked_add(1)
                .ok_or(PercolatorError::EngineArithmeticOverflow)?;
            let mut asset = AssetStateV16::default();
            asset.market_id = market_id;
            asset.raw_oracle_target_price = initial_price;
            asset.effective_price = initial_price;
            asset.fund_px_last = initial_price;
            asset.slot_last = init_slot;
            let mut slot = EngineAssetSlotV16Account::empty_for_market(market_id);
            slot.asset = percolator::AssetStateV16Account::from_runtime(&asset);
            slot.insurance_domain_budget_long = percolator::V16PodU128::new(0);
            slot.insurance_domain_budget_short = percolator::V16PodU128::new(0);
            slot.insurance_domain_spent_long = percolator::V16PodU128::new(0);
            slot.insurance_domain_spent_short = percolator::V16PodU128::new(0);
            *asset_slot_wire_mut(data, i)? = slot;
            i += 1;
        }

        let (_, group) = market_view_mut(data)?;
        group.validate_shape().map_err(map_account_wire_error)?;
        Ok(())
    }

    #[cfg(not(target_os = "solana"))]
    pub fn init_market_account(
        data: &mut [u8],
        config: &WrapperConfigV16,
        group: &MarketGroupV16,
    ) -> Result<(), ProgramError> {
        if data.len() < MIN_MARKET_ACCOUNT_LEN {
            return Err(PercolatorError::InvalidAccountLen.into());
        }
        if is_initialized(data) {
            return Err(PercolatorError::AlreadyInitialized.into());
        }
        for b in data.iter_mut() {
            *b = 0;
        }
        write_header(data, KIND_MARKET)?;
        write_wrapper_config_to_bytes(data, config)?;
        let base_profile = asset_oracle_profile_from_config(config);
        init_asset_oracle_profiles(data, &base_profile)?;
        write_market_wire(data, group)?;
        Ok(())
    }

    #[cfg(not(target_os = "solana"))]
    pub fn read_market(data: &[u8]) -> Result<(WrapperConfigV16, MarketGroupV16), ProgramError> {
        if data.len() < MIN_MARKET_ACCOUNT_LEN {
            return Err(PercolatorError::InvalidAccountLen.into());
        }
        check_header(data, KIND_MARKET)?;
        let config = read_wrapper_config_from_bytes(data)?;
        Ok((config, *market_from_wire_boxed(data, true)?))
    }

    #[cfg(not(target_os = "solana"))]
    pub fn read_market_boxed(
        data: &[u8],
    ) -> Result<(Box<WrapperConfigV16>, Box<MarketGroupV16>), ProgramError> {
        if data.len() < MIN_MARKET_ACCOUNT_LEN {
            return Err(PercolatorError::InvalidAccountLen.into());
        }
        check_header(data, KIND_MARKET)?;
        let config = read_wrapper_config_boxed_from_bytes(data)?;
        Ok((config, market_from_wire_boxed(data, false)?))
    }

    pub fn read_market_trade_preflight(
        data: &[u8],
        asset_index: usize,
    ) -> Result<(WrapperConfigV16, MarketModeV16, u64, u64, u64), ProgramError> {
        if data.len() < MIN_MARKET_ACCOUNT_LEN {
            return Err(PercolatorError::InvalidAccountLen.into());
        }
        check_header(data, KIND_MARKET)?;
        let config = read_wrapper_config_from_bytes(data)?;
        let wire = market_header(data)?;
        let engine_config = wire
            .config
            .try_to_runtime()
            .map_err(map_account_wire_error)?;
        if asset_index >= engine_config.max_market_slots as usize {
            return Err(PercolatorError::InvalidInstruction.into());
        }
        let slot = asset_slot_wire(data, asset_index)?;
        Ok((
            config,
            decode_market_mode(wire.mode)?,
            wire.current_slot.get(),
            slot.asset.effective_price.get(),
            engine_config.max_trading_fee_bps,
        ))
    }

    #[cfg(not(target_os = "solana"))]
    pub fn write_market(
        data: &mut [u8],
        config: &WrapperConfigV16,
        group: &MarketGroupV16,
    ) -> Result<(), ProgramError> {
        if data.len() < MIN_MARKET_ACCOUNT_LEN {
            return Err(PercolatorError::InvalidAccountLen.into());
        }
        check_header(data, KIND_MARKET)?;
        write_wrapper_config_to_bytes(data, config)?;
        if config.oracle_mode != ORACLE_MODE_MANUAL {
            let base_profile = asset_oracle_profile_from_config(config);
            write_asset_oracle_profile(data, 0, &base_profile)?;
        }
        write_market_wire(data, group)?;
        Ok(())
    }

    #[cfg(not(target_os = "solana"))]
    pub fn init_portfolio_account(
        data: &mut [u8],
        account: &PortfolioAccountV16,
    ) -> Result<(), ProgramError> {
        if data.len() < PORTFOLIO_ACCOUNT_LEN {
            return Err(PercolatorError::InvalidAccountLen.into());
        }
        if is_initialized(data) {
            return Err(PercolatorError::AlreadyInitialized.into());
        }
        for b in data.iter_mut() {
            *b = 0;
        }
        write_header(data, KIND_PORTFOLIO)?;
        write_portfolio_wire(data, account)
    }

    pub fn init_portfolio_account_zero_copy(
        data: &mut [u8],
        market_group_id: [u8; 32],
        portfolio_account_id: [u8; 32],
        owner: [u8; 32],
        last_fee_slot: u64,
        max_market_slots: usize,
    ) -> Result<(), ProgramError> {
        let required = portfolio_account_len_for_market_slots(max_market_slots)?;
        if data.len() < required {
            return Err(PercolatorError::InvalidAccountLen.into());
        }
        if is_initialized(data) {
            return Err(PercolatorError::AlreadyInitialized.into());
        }
        for b in data.iter_mut() {
            *b = 0;
        }
        write_header(data, KIND_PORTFOLIO)?;
        let header = ProvenanceHeaderV16::new(market_group_id, portfolio_account_id, owner);
        let wire = portfolio_wire_mut(data)?;
        wire.provenance_header = percolator::ProvenanceHeaderV16Account::from_runtime(&header);
        wire.owner = owner;
        wire.last_fee_slot = percolator::V16PodU64::new(last_fee_slot);
        let empty_leg =
            percolator::PortfolioLegV16Account::from_runtime(&percolator::PortfolioLegV16::EMPTY);
        for leg in wire.legs.iter_mut() {
            *leg = empty_leg;
        }
        Ok(())
    }

    #[cfg(not(target_os = "solana"))]
    pub fn read_portfolio(data: &[u8]) -> Result<PortfolioAccountV16, ProgramError> {
        if data.len() < PORTFOLIO_ACCOUNT_LEN {
            return Err(PercolatorError::InvalidAccountLen.into());
        }
        check_header(data, KIND_PORTFOLIO)?;
        Ok(*portfolio_from_wire_boxed(data, None)?)
    }

    #[cfg(not(target_os = "solana"))]
    pub fn read_portfolio_boxed(data: &[u8]) -> Result<Box<PortfolioAccountV16>, ProgramError> {
        if data.len() < PORTFOLIO_ACCOUNT_LEN {
            return Err(PercolatorError::InvalidAccountLen.into());
        }
        check_header(data, KIND_PORTFOLIO)?;
        portfolio_from_wire_boxed(data, None)
    }

    #[cfg(not(target_os = "solana"))]
    pub fn read_portfolio_boxed_for_market_slots(
        data: &[u8],
        max_market_slots: usize,
    ) -> Result<Box<PortfolioAccountV16>, ProgramError> {
        if data.len() < PORTFOLIO_ACCOUNT_LEN {
            return Err(PercolatorError::InvalidAccountLen.into());
        }
        check_header(data, KIND_PORTFOLIO)?;
        let domains = v16_domain_count_for_market_slots(
            u32::try_from(max_market_slots).map_err(|_| PercolatorError::InvalidAccountLen)?,
        )
        .map_err(map_account_wire_error)?;
        portfolio_from_wire_boxed(data, Some(domains))
    }

    pub fn read_portfolio_owner_preflight(
        data: &[u8],
    ) -> Result<(ProvenanceHeaderV16, [u8; 32]), ProgramError> {
        if data.len() < PORTFOLIO_ACCOUNT_LEN {
            return Err(PercolatorError::InvalidAccountLen.into());
        }
        check_header(data, KIND_PORTFOLIO)?;
        let wire = portfolio_wire(data)?;
        let header = wire
            .provenance_header
            .try_to_runtime()
            .map_err(map_account_wire_error)?;
        if header.owner != wire.owner {
            return Err(ProgramError::InvalidAccountData);
        }
        Ok((header, wire.owner))
    }

    #[cfg(not(target_os = "solana"))]
    pub fn write_portfolio(
        data: &mut [u8],
        account: &PortfolioAccountV16,
    ) -> Result<(), ProgramError> {
        if data.len() < PORTFOLIO_ACCOUNT_LEN {
            return Err(PercolatorError::InvalidAccountLen.into());
        }
        check_header(data, KIND_PORTFOLIO)?;
        write_portfolio_wire(data, account)
    }

    pub const fn alignment_note() -> usize {
        1
    }

    pub const fn wrapper_config_len_for_test() -> usize {
        WRAPPER_CONFIG_LEN
    }
}

pub mod ix {
    use alloc::vec::Vec;
    use solana_program::program_error::ProgramError;

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum Instruction {
        InitMarket {
            max_portfolio_assets: u16,
            h_min: u64,
            h_max: u64,
            initial_price: u64,
            min_nonzero_mm_req: u128,
            min_nonzero_im_req: u128,
            maintenance_margin_bps: u64,
            initial_margin_bps: u64,
            max_trading_fee_bps: u64,
            trade_fee_base_bps: u64,
            liquidation_fee_bps: u64,
            liquidation_fee_cap: u128,
            min_liquidation_abs: u128,
            max_price_move_bps_per_slot: u64,
            max_accrual_dt_slots: u64,
            max_abs_funding_e9_per_slot: u64,
            min_funding_lifetime_slots: u64,
            max_account_b_settlement_chunks: u64,
            max_bankrupt_close_chunks: u64,
            max_bankrupt_close_lifetime_slots: u64,
            public_b_chunk_atoms: u128,
            maintenance_fee_per_slot: u128,
        },
        InitPortfolio,
        Deposit {
            amount: u128,
        },
        Withdraw {
            amount: u128,
        },
        PermissionlessCrank {
            action: u8,
            asset_index: u16,
            now_slot: u64,
            funding_rate_e9: i128,
            close_q: u128,
            fee_bps: u64,
            recovery_reason: u8,
        },
        TradeNoCpi {
            asset_index: u16,
            size_q: i128,
            exec_price: u64,
            fee_bps: u64,
        },
        TradeCpi {
            asset_index: u16,
            size_q: i128,
            fee_bps: u64,
            limit_price: u64,
        },
        ClosePortfolio,
        TopUpInsurance {
            amount: u128,
        },
        TopUpInsuranceDomain {
            domain: u8,
            amount: u128,
        },
        CloseSlab,
        ResolveMarket,
        WithdrawInsuranceLimited {
            amount: u128,
        },
        TopUpBackingBucket {
            domain: u8,
            amount: u128,
            expiry_slot: u64,
        },
        WithdrawBackingBucket {
            domain: u8,
            amount: u128,
        },
        ConvertReleasedPnl {
            amount: u128,
        },
        CloseResolved {
            fee_rate_per_slot: u128,
        },
        UpdateAuthority {
            kind: u8,
            new_pubkey: [u8; 32],
        },
        UpdateInsurancePolicy {
            max_bps: u16,
            deposits_only: u8,
            cooldown_slots: u64,
        },
        UpdateLiquidationFeePolicy {
            cranker_share_bps: u16,
        },
        UpdateMaintenanceFeePolicy {
            cranker_share_bps: u16,
        },
        UpdateBackingFeePolicy {
            domain: u8,
            fee_bps: u16,
            insurance_share_bps: u16,
        },
        UpdateTradeFeePolicy {
            trade_fee_base_bps: u64,
        },
        UpdateFeeRedirectPolicy {
            redirect_bps: u16,
        },
        UpdateMarketInitFeePolicy {
            min_init_fee: u128,
        },
        WithdrawBackingBucketEarnings {
            domain: u8,
            amount: u128,
        },
        SyncBackingDomainLedger {
            domain: u8,
        },
        SyncInsuranceLedger,
        ConfigurePermissionlessResolve {
            stale_slots: u64,
            force_close_delay_slots: u64,
        },
        ResolveStalePermissionless {
            now_slot: u64,
        },
        ConfigureHybridOracle {
            asset_index: u16,
            now_slot: u64,
            now_unix_ts: i64,
            oracle_leg_count: u8,
            oracle_leg_flags: u8,
            max_staleness_secs: u64,
            hybrid_soft_stale_slots: u64,
            mark_ewma_halflife_slots: u64,
            mark_min_fee: u64,
            invert: u8,
            unit_scale: u32,
            conf_filter_bps: u16,
            oracle_leg_feeds: [[u8; 32]; 3],
        },
        ConfigureEwmaMark {
            asset_index: u16,
            now_slot: u64,
            initial_mark_e6: u64,
            mark_ewma_halflife_slots: u64,
            mark_min_fee: u64,
        },
        PushEwmaMark {
            asset_index: u16,
            now_slot: u64,
            mark_e6: u64,
        },
        ConfigureAuthMark {
            asset_index: u16,
            now_slot: u64,
            initial_mark_e6: u64,
        },
        PushAuthMark {
            asset_index: u16,
            now_slot: u64,
            mark_e6: u64,
        },
        ForceCloseAbandonedAsset {
            asset_index: u16,
            now_slot: u64,
            close_q: u128,
        },
        UpdateAssetLifecycle {
            action: u8,
            asset_index: u16,
            now_slot: u64,
            initial_price: u64,
            insurance_authority: [u8; 32],
            insurance_operator: [u8; 32],
            backing_bucket_authority: [u8; 32],
            oracle_authority: [u8; 32],
        },
        WithdrawInsurance {
            amount: u128,
        },
        WithdrawInsuranceDomain {
            domain: u8,
            amount: u128,
        },
        CureAndCancelClose {
            optional_deposit: u128,
        },
        ForfeitRecoveryLeg {
            asset_index: u16,
            b_delta_budget: u128,
        },
        RebalanceReduce {
            asset_index: u16,
            reduce_q: u128,
        },
        FinalizeResetSide {
            asset_index: u16,
            side: u8,
        },
        ClaimResolvedPayoutTopup,
        RefineResolvedUnreceiptedBound {
            decrease_num: u128,
        },
        SyncMaintenanceFee {
            now_slot: u64,
        },
        UpdateBaseUnitMints {
            primary_mint: [u8; 32],
            secondary_mint: [u8; 32],
        },
        SwapSecondaryForPrimary {
            amount: u128,
        },
        // ── Fork LP Vault (Phase 2.B Tier 3, Workstream 4B) — tags 65-71 ──
        CreateLpVault {
            fee_share_bps: u16,
            redemption_cooldown_slots: u64,
            oi_reservation_threshold_bps: u16,
            domain: u16,
        },
        DepositToLpVault {
            amount: u128,
        },
        RequestRedeemLpShares {
            lp_amount: u128,
        },
        ExecuteRedemption,
        LpVaultCrankFees,
        SetLpVaultPaused {
            paused: u8,
        },
        CloseLpVault,
        // ── Fork NFT / B-3 (Phase 2.B Tier 3 / A.4) — tags 72-73 ──────────
        /// B-3 TransferPortfolioOwnership (tag 72).
        ///
        /// CPI-only: called by the NFT program's `ExecuteTransferHook` handler
        /// with the mint-authority PDA as the signer.  Reassigns
        /// `portfolio.owner` AND `portfolio.provenance_header.owner` atomically
        /// (Guardrail 2 — dual write).  Guarded by all 6 §7 guardrails.
        ///
        /// Wire: `tag(72) + new_owner[32] + asset_index(2 LE)`.
        TransferPortfolioOwnership {
            new_owner: [u8; 32],
            asset_index: u16,
        },
        /// SetNftProgramId (tag 73).
        ///
        /// Admin-gated.  Creates (or updates) the per-market `NftRegistryV16`
        /// PDA with the authorised NFT program ID.  Must be run for a market
        /// before NFT transfers work there.  Re-pointing is rare-and-deliberate;
        /// it freezes in-flight transfers for NFTs minted under the old program
        /// ID (no code change needed — fail-closed on mismatched signer).
        ///
        /// Wire: `tag(73) + nft_program_id[32]`.
        SetNftProgramId {
            nft_program_id: [u8; 32],
        },
    }

    impl Instruction {
        pub fn decode(input: &[u8]) -> Result<Self, ProgramError> {
            let (&tag, mut rest) = input
                .split_first()
                .ok_or(ProgramError::InvalidInstructionData)?;
            let ix = match tag {
                0 => Self::InitMarket {
                    max_portfolio_assets: read_u16(&mut rest)?,
                    h_min: read_u64(&mut rest)?,
                    h_max: read_u64(&mut rest)?,
                    initial_price: read_u64(&mut rest)?,
                    min_nonzero_mm_req: read_u128(&mut rest)?,
                    min_nonzero_im_req: read_u128(&mut rest)?,
                    maintenance_margin_bps: read_u64(&mut rest)?,
                    initial_margin_bps: read_u64(&mut rest)?,
                    max_trading_fee_bps: read_u64(&mut rest)?,
                    trade_fee_base_bps: read_u64(&mut rest)?,
                    liquidation_fee_bps: read_u64(&mut rest)?,
                    liquidation_fee_cap: read_u128(&mut rest)?,
                    min_liquidation_abs: read_u128(&mut rest)?,
                    max_price_move_bps_per_slot: read_u64(&mut rest)?,
                    max_accrual_dt_slots: read_u64(&mut rest)?,
                    max_abs_funding_e9_per_slot: read_u64(&mut rest)?,
                    min_funding_lifetime_slots: read_u64(&mut rest)?,
                    max_account_b_settlement_chunks: read_u64(&mut rest)?,
                    max_bankrupt_close_chunks: read_u64(&mut rest)?,
                    max_bankrupt_close_lifetime_slots: read_u64(&mut rest)?,
                    public_b_chunk_atoms: read_u128(&mut rest)?,
                    maintenance_fee_per_slot: read_u128(&mut rest)?,
                },
                1 => Self::InitPortfolio,
                3 => Self::Deposit {
                    amount: read_u128(&mut rest)?,
                },
                4 => Self::Withdraw {
                    amount: read_u128(&mut rest)?,
                },
                5 => Self::PermissionlessCrank {
                    action: read_u8(&mut rest)?,
                    asset_index: read_u16(&mut rest)?,
                    now_slot: read_u64(&mut rest)?,
                    funding_rate_e9: read_i128(&mut rest)?,
                    close_q: read_u128(&mut rest)?,
                    fee_bps: read_u64(&mut rest)?,
                    recovery_reason: read_u8(&mut rest)?,
                },
                6 => Self::TradeNoCpi {
                    asset_index: read_u16(&mut rest)?,
                    size_q: read_i128(&mut rest)?,
                    exec_price: read_u64(&mut rest)?,
                    fee_bps: read_u64(&mut rest)?,
                },
                10 => Self::TradeCpi {
                    asset_index: read_u16(&mut rest)?,
                    size_q: read_i128(&mut rest)?,
                    fee_bps: read_u64(&mut rest)?,
                    limit_price: read_u64(&mut rest)?,
                },
                8 => Self::ClosePortfolio,
                9 => Self::TopUpInsurance {
                    amount: read_u128(&mut rest)?,
                },
                56 => Self::TopUpInsuranceDomain {
                    domain: read_u8(&mut rest)?,
                    amount: read_u128(&mut rest)?,
                },
                13 => Self::CloseSlab,
                19 => Self::ResolveMarket,
                23 => Self::WithdrawInsuranceLimited {
                    amount: read_u128(&mut rest)?,
                },
                24 => Self::TopUpBackingBucket {
                    domain: read_u8(&mut rest)?,
                    amount: read_u128(&mut rest)?,
                    expiry_slot: read_u64(&mut rest)?,
                },
                50 => Self::WithdrawBackingBucket {
                    domain: read_u8(&mut rest)?,
                    amount: read_u128(&mut rest)?,
                },
                28 => Self::ConvertReleasedPnl {
                    amount: read_u128(&mut rest)?,
                },
                30 => Self::CloseResolved {
                    fee_rate_per_slot: read_u128(&mut rest)?,
                },
                32 => Self::UpdateAuthority {
                    kind: read_u8(&mut rest)?,
                    new_pubkey: read_bytes32(&mut rest)?,
                },
                33 => Self::UpdateInsurancePolicy {
                    max_bps: read_u16(&mut rest)?,
                    deposits_only: read_u8(&mut rest)?,
                    cooldown_slots: read_u64(&mut rest)?,
                },
                37 => Self::UpdateLiquidationFeePolicy {
                    cranker_share_bps: read_u16(&mut rest)?,
                },
                49 => Self::UpdateMaintenanceFeePolicy {
                    cranker_share_bps: read_u16(&mut rest)?,
                },
                51 => Self::UpdateBackingFeePolicy {
                    domain: read_u8(&mut rest)?,
                    fee_bps: read_u16(&mut rest)?,
                    insurance_share_bps: read_u16(&mut rest)?,
                },
                55 => Self::UpdateTradeFeePolicy {
                    trade_fee_base_bps: read_u64(&mut rest)?,
                },
                58 => Self::UpdateFeeRedirectPolicy {
                    redirect_bps: read_u16(&mut rest)?,
                },
                59 => Self::UpdateMarketInitFeePolicy {
                    min_init_fee: read_u128(&mut rest)?,
                },
                60 => Self::UpdateBaseUnitMints {
                    primary_mint: read_bytes32(&mut rest)?,
                    secondary_mint: read_bytes32(&mut rest)?,
                },
                61 => Self::SwapSecondaryForPrimary {
                    amount: read_u128(&mut rest)?,
                },
                62 => Self::ConfigureAuthMark {
                    asset_index: read_u16(&mut rest)?,
                    now_slot: read_u64(&mut rest)?,
                    initial_mark_e6: read_u64(&mut rest)?,
                },
                63 => Self::PushAuthMark {
                    asset_index: read_u16(&mut rest)?,
                    now_slot: read_u64(&mut rest)?,
                    mark_e6: read_u64(&mut rest)?,
                },
                64 => Self::ForceCloseAbandonedAsset {
                    asset_index: read_u16(&mut rest)?,
                    now_slot: read_u64(&mut rest)?,
                    close_q: read_u128(&mut rest)?,
                },
                52 => Self::WithdrawBackingBucketEarnings {
                    domain: read_u8(&mut rest)?,
                    amount: read_u128(&mut rest)?,
                },
                53 => Self::SyncBackingDomainLedger {
                    domain: read_u8(&mut rest)?,
                },
                54 => Self::SyncInsuranceLedger,
                38 => Self::ConfigurePermissionlessResolve {
                    stale_slots: read_u64(&mut rest)?,
                    force_close_delay_slots: read_u64(&mut rest)?,
                },
                39 => Self::ResolveStalePermissionless {
                    now_slot: read_u64(&mut rest)?,
                },
                34 => Self::ConfigureHybridOracle {
                    asset_index: read_u16(&mut rest)?,
                    now_slot: read_u64(&mut rest)?,
                    now_unix_ts: read_i64(&mut rest)?,
                    oracle_leg_count: read_u8(&mut rest)?,
                    oracle_leg_flags: read_u8(&mut rest)?,
                    max_staleness_secs: read_u64(&mut rest)?,
                    hybrid_soft_stale_slots: read_u64(&mut rest)?,
                    mark_ewma_halflife_slots: read_u64(&mut rest)?,
                    mark_min_fee: read_u64(&mut rest)?,
                    invert: read_u8(&mut rest)?,
                    unit_scale: read_u32(&mut rest)?,
                    conf_filter_bps: read_u16(&mut rest)?,
                    oracle_leg_feeds: [
                        read_bytes32(&mut rest)?,
                        read_bytes32(&mut rest)?,
                        read_bytes32(&mut rest)?,
                    ],
                },
                35 => Self::ConfigureEwmaMark {
                    asset_index: read_u16(&mut rest)?,
                    now_slot: read_u64(&mut rest)?,
                    initial_mark_e6: read_u64(&mut rest)?,
                    mark_ewma_halflife_slots: read_u64(&mut rest)?,
                    mark_min_fee: read_u64(&mut rest)?,
                },
                36 => Self::PushEwmaMark {
                    asset_index: read_u16(&mut rest)?,
                    now_slot: read_u64(&mut rest)?,
                    mark_e6: read_u64(&mut rest)?,
                },
                40 => Self::UpdateAssetLifecycle {
                    action: read_u8(&mut rest)?,
                    asset_index: read_u16(&mut rest)?,
                    now_slot: read_u64(&mut rest)?,
                    initial_price: read_u64(&mut rest)?,
                    insurance_authority: read_bytes32(&mut rest)?,
                    insurance_operator: read_bytes32(&mut rest)?,
                    backing_bucket_authority: read_bytes32(&mut rest)?,
                    oracle_authority: read_bytes32(&mut rest)?,
                },
                41 => Self::WithdrawInsurance {
                    amount: read_u128(&mut rest)?,
                },
                57 => Self::WithdrawInsuranceDomain {
                    domain: read_u8(&mut rest)?,
                    amount: read_u128(&mut rest)?,
                },
                42 => Self::CureAndCancelClose {
                    optional_deposit: read_u128(&mut rest)?,
                },
                43 => Self::ForfeitRecoveryLeg {
                    asset_index: read_u16(&mut rest)?,
                    b_delta_budget: read_u128(&mut rest)?,
                },
                44 => Self::RebalanceReduce {
                    asset_index: read_u16(&mut rest)?,
                    reduce_q: read_u128(&mut rest)?,
                },
                45 => Self::FinalizeResetSide {
                    asset_index: read_u16(&mut rest)?,
                    side: read_u8(&mut rest)?,
                },
                46 => Self::ClaimResolvedPayoutTopup,
                47 => Self::RefineResolvedUnreceiptedBound {
                    decrease_num: read_u128(&mut rest)?,
                },
                48 => Self::SyncMaintenanceFee {
                    now_slot: read_u64(&mut rest)?,
                },
                65 => Self::CreateLpVault {
                    fee_share_bps: read_u16(&mut rest)?,
                    redemption_cooldown_slots: read_u64(&mut rest)?,
                    oi_reservation_threshold_bps: read_u16(&mut rest)?,
                    domain: read_u16(&mut rest)?,
                },
                66 => Self::DepositToLpVault {
                    amount: read_u128(&mut rest)?,
                },
                67 => Self::RequestRedeemLpShares {
                    lp_amount: read_u128(&mut rest)?,
                },
                68 => Self::ExecuteRedemption,
                69 => Self::LpVaultCrankFees,
                70 => Self::SetLpVaultPaused {
                    paused: read_u8(&mut rest)?,
                },
                71 => Self::CloseLpVault,
                72 => {
                    let new_owner = read_bytes32(&mut rest)?;
                    let asset_index = read_u16(&mut rest)?;
                    Self::TransferPortfolioOwnership { new_owner, asset_index }
                }
                73 => {
                    let nft_program_id = read_bytes32(&mut rest)?;
                    Self::SetNftProgramId { nft_program_id }
                }
                _ => return Err(ProgramError::InvalidInstructionData),
            };
            if !rest.is_empty() {
                return Err(ProgramError::InvalidInstructionData);
            }
            Ok(ix)
        }

        pub fn encode(&self) -> Vec<u8> {
            let mut out = Vec::new();
            match *self {
                Self::InitMarket {
                    max_portfolio_assets,
                    h_min,
                    h_max,
                    initial_price,
                    min_nonzero_mm_req,
                    min_nonzero_im_req,
                    maintenance_margin_bps,
                    initial_margin_bps,
                    max_trading_fee_bps,
                    trade_fee_base_bps,
                    liquidation_fee_bps,
                    liquidation_fee_cap,
                    min_liquidation_abs,
                    max_price_move_bps_per_slot,
                    max_accrual_dt_slots,
                    max_abs_funding_e9_per_slot,
                    min_funding_lifetime_slots,
                    max_account_b_settlement_chunks,
                    max_bankrupt_close_chunks,
                    max_bankrupt_close_lifetime_slots,
                    public_b_chunk_atoms,
                    maintenance_fee_per_slot,
                } => {
                    out.push(0);
                    push_u16(&mut out, max_portfolio_assets);
                    push_u64(&mut out, h_min);
                    push_u64(&mut out, h_max);
                    push_u64(&mut out, initial_price);
                    push_u128(&mut out, min_nonzero_mm_req);
                    push_u128(&mut out, min_nonzero_im_req);
                    push_u64(&mut out, maintenance_margin_bps);
                    push_u64(&mut out, initial_margin_bps);
                    push_u64(&mut out, max_trading_fee_bps);
                    push_u64(&mut out, trade_fee_base_bps);
                    push_u64(&mut out, liquidation_fee_bps);
                    push_u128(&mut out, liquidation_fee_cap);
                    push_u128(&mut out, min_liquidation_abs);
                    push_u64(&mut out, max_price_move_bps_per_slot);
                    push_u64(&mut out, max_accrual_dt_slots);
                    push_u64(&mut out, max_abs_funding_e9_per_slot);
                    push_u64(&mut out, min_funding_lifetime_slots);
                    push_u64(&mut out, max_account_b_settlement_chunks);
                    push_u64(&mut out, max_bankrupt_close_chunks);
                    push_u64(&mut out, max_bankrupt_close_lifetime_slots);
                    push_u128(&mut out, public_b_chunk_atoms);
                    push_u128(&mut out, maintenance_fee_per_slot);
                }
                Self::InitPortfolio => out.push(1),
                Self::Deposit { amount } => {
                    out.push(3);
                    push_u128(&mut out, amount);
                }
                Self::Withdraw { amount } => {
                    out.push(4);
                    push_u128(&mut out, amount);
                }
                Self::PermissionlessCrank {
                    action,
                    asset_index,
                    now_slot,
                    funding_rate_e9,
                    close_q,
                    fee_bps,
                    recovery_reason,
                } => {
                    out.push(5);
                    out.push(action);
                    push_u16(&mut out, asset_index);
                    push_u64(&mut out, now_slot);
                    push_i128(&mut out, funding_rate_e9);
                    push_u128(&mut out, close_q);
                    push_u64(&mut out, fee_bps);
                    out.push(recovery_reason);
                }
                Self::TradeNoCpi {
                    asset_index,
                    size_q,
                    exec_price,
                    fee_bps,
                } => {
                    out.push(6);
                    push_u16(&mut out, asset_index);
                    push_i128(&mut out, size_q);
                    push_u64(&mut out, exec_price);
                    push_u64(&mut out, fee_bps);
                }
                Self::TradeCpi {
                    asset_index,
                    size_q,
                    fee_bps,
                    limit_price,
                } => {
                    out.push(10);
                    push_u16(&mut out, asset_index);
                    push_i128(&mut out, size_q);
                    push_u64(&mut out, fee_bps);
                    push_u64(&mut out, limit_price);
                }
                Self::ClosePortfolio => out.push(8),
                Self::TopUpInsurance { amount } => {
                    out.push(9);
                    push_u128(&mut out, amount);
                }
                Self::TopUpInsuranceDomain { domain, amount } => {
                    out.push(56);
                    out.push(domain);
                    push_u128(&mut out, amount);
                }
                Self::CloseSlab => out.push(13),
                Self::ResolveMarket => out.push(19),
                Self::WithdrawInsuranceLimited { amount } => {
                    out.push(23);
                    push_u128(&mut out, amount);
                }
                Self::TopUpBackingBucket {
                    domain,
                    amount,
                    expiry_slot,
                } => {
                    out.push(24);
                    out.push(domain);
                    push_u128(&mut out, amount);
                    push_u64(&mut out, expiry_slot);
                }
                Self::WithdrawBackingBucket { domain, amount } => {
                    out.push(50);
                    out.push(domain);
                    push_u128(&mut out, amount);
                }
                Self::ConvertReleasedPnl { amount } => {
                    out.push(28);
                    push_u128(&mut out, amount);
                }
                Self::CloseResolved { fee_rate_per_slot } => {
                    out.push(30);
                    push_u128(&mut out, fee_rate_per_slot);
                }
                Self::UpdateAuthority { kind, new_pubkey } => {
                    out.push(32);
                    out.push(kind);
                    out.extend_from_slice(&new_pubkey);
                }
                Self::UpdateInsurancePolicy {
                    max_bps,
                    deposits_only,
                    cooldown_slots,
                } => {
                    out.push(33);
                    push_u16(&mut out, max_bps);
                    out.push(deposits_only);
                    push_u64(&mut out, cooldown_slots);
                }
                Self::UpdateLiquidationFeePolicy { cranker_share_bps } => {
                    out.push(37);
                    push_u16(&mut out, cranker_share_bps);
                }
                Self::UpdateMaintenanceFeePolicy { cranker_share_bps } => {
                    out.push(49);
                    push_u16(&mut out, cranker_share_bps);
                }
                Self::UpdateBackingFeePolicy {
                    domain,
                    fee_bps,
                    insurance_share_bps,
                } => {
                    out.push(51);
                    out.push(domain);
                    push_u16(&mut out, fee_bps);
                    push_u16(&mut out, insurance_share_bps);
                }
                Self::UpdateTradeFeePolicy { trade_fee_base_bps } => {
                    out.push(55);
                    push_u64(&mut out, trade_fee_base_bps);
                }
                Self::UpdateFeeRedirectPolicy { redirect_bps } => {
                    out.push(58);
                    push_u16(&mut out, redirect_bps);
                }
                Self::UpdateMarketInitFeePolicy { min_init_fee } => {
                    out.push(59);
                    push_u128(&mut out, min_init_fee);
                }
                Self::UpdateBaseUnitMints {
                    primary_mint,
                    secondary_mint,
                } => {
                    out.push(60);
                    out.extend_from_slice(&primary_mint);
                    out.extend_from_slice(&secondary_mint);
                }
                Self::SwapSecondaryForPrimary { amount } => {
                    out.push(61);
                    push_u128(&mut out, amount);
                }
                Self::WithdrawBackingBucketEarnings { domain, amount } => {
                    out.push(52);
                    out.push(domain);
                    push_u128(&mut out, amount);
                }
                Self::SyncBackingDomainLedger { domain } => {
                    out.push(53);
                    out.push(domain);
                }
                Self::SyncInsuranceLedger => out.push(54),
                Self::ConfigurePermissionlessResolve {
                    stale_slots,
                    force_close_delay_slots,
                } => {
                    out.push(38);
                    push_u64(&mut out, stale_slots);
                    push_u64(&mut out, force_close_delay_slots);
                }
                Self::ResolveStalePermissionless { now_slot } => {
                    out.push(39);
                    push_u64(&mut out, now_slot);
                }
                Self::ConfigureHybridOracle {
                    asset_index,
                    now_slot,
                    now_unix_ts,
                    oracle_leg_count,
                    oracle_leg_flags,
                    max_staleness_secs,
                    hybrid_soft_stale_slots,
                    mark_ewma_halflife_slots,
                    mark_min_fee,
                    invert,
                    unit_scale,
                    conf_filter_bps,
                    oracle_leg_feeds,
                } => {
                    out.push(34);
                    push_u16(&mut out, asset_index);
                    push_u64(&mut out, now_slot);
                    push_i64(&mut out, now_unix_ts);
                    out.push(oracle_leg_count);
                    out.push(oracle_leg_flags);
                    push_u64(&mut out, max_staleness_secs);
                    push_u64(&mut out, hybrid_soft_stale_slots);
                    push_u64(&mut out, mark_ewma_halflife_slots);
                    push_u64(&mut out, mark_min_fee);
                    out.push(invert);
                    push_u32(&mut out, unit_scale);
                    push_u16(&mut out, conf_filter_bps);
                    for feed in oracle_leg_feeds {
                        out.extend_from_slice(&feed);
                    }
                }
                Self::ConfigureEwmaMark {
                    asset_index,
                    now_slot,
                    initial_mark_e6,
                    mark_ewma_halflife_slots,
                    mark_min_fee,
                } => {
                    out.push(35);
                    push_u16(&mut out, asset_index);
                    push_u64(&mut out, now_slot);
                    push_u64(&mut out, initial_mark_e6);
                    push_u64(&mut out, mark_ewma_halflife_slots);
                    push_u64(&mut out, mark_min_fee);
                }
                Self::PushEwmaMark {
                    asset_index,
                    now_slot,
                    mark_e6,
                } => {
                    out.push(36);
                    push_u16(&mut out, asset_index);
                    push_u64(&mut out, now_slot);
                    push_u64(&mut out, mark_e6);
                }
                Self::ConfigureAuthMark {
                    asset_index,
                    now_slot,
                    initial_mark_e6,
                } => {
                    out.push(62);
                    push_u16(&mut out, asset_index);
                    push_u64(&mut out, now_slot);
                    push_u64(&mut out, initial_mark_e6);
                }
                Self::PushAuthMark {
                    asset_index,
                    now_slot,
                    mark_e6,
                } => {
                    out.push(63);
                    push_u16(&mut out, asset_index);
                    push_u64(&mut out, now_slot);
                    push_u64(&mut out, mark_e6);
                }
                Self::ForceCloseAbandonedAsset {
                    asset_index,
                    now_slot,
                    close_q,
                } => {
                    out.push(64);
                    push_u16(&mut out, asset_index);
                    push_u64(&mut out, now_slot);
                    push_u128(&mut out, close_q);
                }
                Self::UpdateAssetLifecycle {
                    action,
                    asset_index,
                    now_slot,
                    initial_price,
                    insurance_authority,
                    insurance_operator,
                    backing_bucket_authority,
                    oracle_authority,
                } => {
                    out.push(40);
                    out.push(action);
                    push_u16(&mut out, asset_index);
                    push_u64(&mut out, now_slot);
                    push_u64(&mut out, initial_price);
                    out.extend_from_slice(&insurance_authority);
                    out.extend_from_slice(&insurance_operator);
                    out.extend_from_slice(&backing_bucket_authority);
                    out.extend_from_slice(&oracle_authority);
                }
                Self::WithdrawInsurance { amount } => {
                    out.push(41);
                    push_u128(&mut out, amount);
                }
                Self::WithdrawInsuranceDomain { domain, amount } => {
                    out.push(57);
                    out.push(domain);
                    push_u128(&mut out, amount);
                }
                Self::CureAndCancelClose { optional_deposit } => {
                    out.push(42);
                    push_u128(&mut out, optional_deposit);
                }
                Self::ForfeitRecoveryLeg {
                    asset_index,
                    b_delta_budget,
                } => {
                    out.push(43);
                    push_u16(&mut out, asset_index);
                    push_u128(&mut out, b_delta_budget);
                }
                Self::RebalanceReduce {
                    asset_index,
                    reduce_q,
                } => {
                    out.push(44);
                    push_u16(&mut out, asset_index);
                    push_u128(&mut out, reduce_q);
                }
                Self::FinalizeResetSide { asset_index, side } => {
                    out.push(45);
                    push_u16(&mut out, asset_index);
                    out.push(side);
                }
                Self::ClaimResolvedPayoutTopup => out.push(46),
                Self::RefineResolvedUnreceiptedBound { decrease_num } => {
                    out.push(47);
                    push_u128(&mut out, decrease_num);
                }
                Self::SyncMaintenanceFee { now_slot } => {
                    out.push(48);
                    push_u64(&mut out, now_slot);
                }
                Self::CreateLpVault {
                    fee_share_bps,
                    redemption_cooldown_slots,
                    oi_reservation_threshold_bps,
                    domain,
                } => {
                    out.push(65);
                    push_u16(&mut out, fee_share_bps);
                    push_u64(&mut out, redemption_cooldown_slots);
                    push_u16(&mut out, oi_reservation_threshold_bps);
                    push_u16(&mut out, domain);
                }
                Self::DepositToLpVault { amount } => {
                    out.push(66);
                    push_u128(&mut out, amount);
                }
                Self::RequestRedeemLpShares { lp_amount } => {
                    out.push(67);
                    push_u128(&mut out, lp_amount);
                }
                Self::ExecuteRedemption => {
                    out.push(68);
                }
                Self::LpVaultCrankFees => {
                    out.push(69);
                }
                Self::SetLpVaultPaused { paused } => {
                    out.push(70);
                    out.push(paused);
                }
                Self::CloseLpVault => {
                    out.push(71);
                }
                Self::TransferPortfolioOwnership { new_owner, asset_index } => {
                    out.push(72);
                    out.extend_from_slice(&new_owner);
                    push_u16(&mut out, asset_index);
                }
                Self::SetNftProgramId { nft_program_id } => {
                    out.push(73);
                    out.extend_from_slice(&nft_program_id);
                }
            }
            out
        }
    }

    fn read_u8(input: &mut &[u8]) -> Result<u8, ProgramError> {
        let (&v, rest) = input
            .split_first()
            .ok_or(ProgramError::InvalidInstructionData)?;
        *input = rest;
        Ok(v)
    }

    fn read_u64(input: &mut &[u8]) -> Result<u64, ProgramError> {
        if input.len() < 8 {
            return Err(ProgramError::InvalidInstructionData);
        }
        let (bytes, rest) = input.split_at(8);
        *input = rest;
        Ok(u64::from_le_bytes(bytes.try_into().unwrap()))
    }

    fn read_u16(input: &mut &[u8]) -> Result<u16, ProgramError> {
        if input.len() < 2 {
            return Err(ProgramError::InvalidInstructionData);
        }
        let (bytes, rest) = input.split_at(2);
        *input = rest;
        Ok(u16::from_le_bytes(bytes.try_into().unwrap()))
    }

    fn read_u32(input: &mut &[u8]) -> Result<u32, ProgramError> {
        if input.len() < 4 {
            return Err(ProgramError::InvalidInstructionData);
        }
        let (bytes, rest) = input.split_at(4);
        *input = rest;
        Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
    }

    fn read_u128(input: &mut &[u8]) -> Result<u128, ProgramError> {
        if input.len() < 16 {
            return Err(ProgramError::InvalidInstructionData);
        }
        let (bytes, rest) = input.split_at(16);
        *input = rest;
        Ok(u128::from_le_bytes(bytes.try_into().unwrap()))
    }

    fn read_i128(input: &mut &[u8]) -> Result<i128, ProgramError> {
        if input.len() < 16 {
            return Err(ProgramError::InvalidInstructionData);
        }
        let (bytes, rest) = input.split_at(16);
        *input = rest;
        Ok(i128::from_le_bytes(bytes.try_into().unwrap()))
    }

    fn read_i64(input: &mut &[u8]) -> Result<i64, ProgramError> {
        if input.len() < 8 {
            return Err(ProgramError::InvalidInstructionData);
        }
        let (bytes, rest) = input.split_at(8);
        *input = rest;
        Ok(i64::from_le_bytes(bytes.try_into().unwrap()))
    }

    fn read_bytes32(input: &mut &[u8]) -> Result<[u8; 32], ProgramError> {
        if input.len() < 32 {
            return Err(ProgramError::InvalidInstructionData);
        }
        let (bytes, rest) = input.split_at(32);
        *input = rest;
        Ok(bytes.try_into().unwrap())
    }

    fn push_u64(out: &mut Vec<u8>, v: u64) {
        out.extend_from_slice(&v.to_le_bytes());
    }

    fn push_u16(out: &mut Vec<u8>, v: u16) {
        out.extend_from_slice(&v.to_le_bytes());
    }

    fn push_u32(out: &mut Vec<u8>, v: u32) {
        out.extend_from_slice(&v.to_le_bytes());
    }

    fn push_u128(out: &mut Vec<u8>, v: u128) {
        out.extend_from_slice(&v.to_le_bytes());
    }

    fn push_i128(out: &mut Vec<u8>, v: i128) {
        out.extend_from_slice(&v.to_le_bytes());
    }

    fn push_i64(out: &mut Vec<u8>, v: i64) {
        out.extend_from_slice(&v.to_le_bytes());
    }
}

pub mod matcher_abi {
    use crate::constants::MATCHER_ABI_VERSION;
    use solana_program::program_error::ProgramError;

    pub const FLAG_VALID: u32 = 1;
    pub const FLAG_PARTIAL_OK: u32 = 2;
    pub const FLAG_REJECTED: u32 = 4;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct MatcherReturn {
        pub abi_version: u32,
        pub flags: u32,
        pub exec_price_e6: u64,
        pub exec_size: i128,
        pub req_id: u64,
        pub lp_account_id: u64,
        pub oracle_price_e6: u64,
        pub asset_index: u64,
    }

    pub fn read_matcher_return(ctx: &[u8]) -> Result<MatcherReturn, ProgramError> {
        if ctx.len() < 64 {
            return Err(ProgramError::InvalidAccountData);
        }
        Ok(MatcherReturn {
            abi_version: u32::from_le_bytes(ctx[0..4].try_into().unwrap()),
            flags: u32::from_le_bytes(ctx[4..8].try_into().unwrap()),
            exec_price_e6: u64::from_le_bytes(ctx[8..16].try_into().unwrap()),
            exec_size: i128::from_le_bytes(ctx[16..32].try_into().unwrap()),
            req_id: u64::from_le_bytes(ctx[32..40].try_into().unwrap()),
            lp_account_id: u64::from_le_bytes(ctx[40..48].try_into().unwrap()),
            oracle_price_e6: u64::from_le_bytes(ctx[48..56].try_into().unwrap()),
            asset_index: u64::from_le_bytes(ctx[56..64].try_into().unwrap()),
        })
    }

    pub fn validate_matcher_return(
        ret: &MatcherReturn,
        lp_account_id: u64,
        asset_index: u16,
        oracle_price_e6: u64,
        req_size: i128,
        req_id: u64,
    ) -> Result<(), ProgramError> {
        if ret.abi_version != MATCHER_ABI_VERSION {
            return Err(ProgramError::InvalidAccountData);
        }
        const KNOWN_FLAGS: u32 = FLAG_VALID | FLAG_PARTIAL_OK | FLAG_REJECTED;
        if (ret.flags & !KNOWN_FLAGS) != 0
            || (ret.flags & FLAG_VALID) == 0
            || (ret.flags & FLAG_REJECTED) != 0
        {
            return Err(ProgramError::InvalidAccountData);
        }
        if ret.lp_account_id != lp_account_id
            || ret.oracle_price_e6 != oracle_price_e6
            || ret.asset_index != asset_index as u64
            || ret.req_id != req_id
            || ret.exec_price_e6 == 0
        {
            return Err(ProgramError::InvalidAccountData);
        }
        if ret.exec_size == 0 {
            if (ret.flags & FLAG_PARTIAL_OK) == 0 || ret.exec_price_e6 != oracle_price_e6 {
                return Err(ProgramError::InvalidAccountData);
            }
            return Ok(());
        }
        if ret.exec_size == i128::MIN || req_size == i128::MIN || req_size == 0 {
            return Err(ProgramError::InvalidAccountData);
        }
        if ret.exec_size.signum() != req_size.signum() {
            return Err(ProgramError::InvalidAccountData);
        }
        if ret.exec_size.unsigned_abs() > req_size.unsigned_abs() {
            return Err(ProgramError::InvalidAccountData);
        }
        if ret.exec_size.unsigned_abs() < req_size.unsigned_abs()
            && (ret.flags & FLAG_PARTIAL_OK) == 0
        {
            return Err(ProgramError::InvalidAccountData);
        }
        Ok(())
    }
}

pub mod oracle_v16 {
    use crate::{
        constants::{
            ORACLE_LEG_CAP, ORACLE_LEG_FLAGS_MASK, ORACLE_LEG_FLAG_DIVIDE_LEG2,
            ORACLE_LEG_FLAG_DIVIDE_LEG3, ORACLE_MODE_AUTH_MARK, ORACLE_MODE_EWMA_MARK,
            ORACLE_MODE_HYBRID_AFTER_HOURS, ORACLE_MODE_MANUAL, SWITCHBOARD_RESULT_SCALE,
        },
        error::PercolatorError,
        state::{AssetOracleProfileV16, WrapperConfigV16},
    };
    use borsh::BorshDeserialize;
    use pythnet_sdk::messages::PriceFeedMessage;
    use solana_program::{account_info::AccountInfo, program_error::ProgramError, pubkey::Pubkey};

    pub const PYTH_RECEIVER_PROGRAM_ID: Pubkey = Pubkey::new_from_array([
        0x0c, 0xb7, 0xfa, 0xbb, 0x52, 0xf7, 0xa6, 0x48, 0xbb, 0x5b, 0x31, 0x7d, 0x9a, 0x01, 0x8b,
        0x90, 0x57, 0xcb, 0x02, 0x47, 0x74, 0xfa, 0xfe, 0x01, 0xe6, 0xc4, 0xdf, 0x98, 0xcc, 0x38,
        0x58, 0x81,
    ]);
    pub const SWITCHBOARD_ON_DEMAND_MAINNET_PROGRAM_ID: Pubkey =
        solana_program::pubkey!("SBondMDrcV3K4kxZR1HNVT7osZxAHVHgYXL5Ze1oMUv");
    pub const SWITCHBOARD_ON_DEMAND_DEVNET_PROGRAM_ID: Pubkey =
        solana_program::pubkey!("Aio4gaXjXzJNVLtzwtNVmSqGKpANtXhybbkhtAC94ji2");
    pub const CHAINLINK_STORE_PROGRAM_ID: Pubkey =
        solana_program::pubkey!("HEvSKofvBgfaexv23kMabbYqxasxU3mQ4ibBMEmJWHny");
    const PRICE_UPDATE_V2_MIN_LEN: usize = 134;
    const OFF_VERIFICATION_LEVEL: usize = 40;
    const OFF_PRICE_FEED_MESSAGE: usize = 41;
    const PYTH_PRICE_UPDATE_V2_DISCRIMINATOR: [u8; 8] =
        [0x22, 0xf1, 0x23, 0x63, 0x9d, 0x7e, 0xf4, 0xcd];
    const PYTH_VERIFICATION_FULL_TAG: u8 = 1;
    const MAX_EXPO_ABS: i32 = 18;
    const SWITCHBOARD_PULL_FEED_DISCRIMINATOR: [u8; 8] = [196, 27, 108, 196, 10, 215, 219, 40];
    const SWITCHBOARD_PULL_FEED_MIN_LEN: usize = 3_208;
    const SB_OFF_FEED_HASH: usize = 8 + 2_112;
    const SB_OFF_MIN_SAMPLE_SIZE: usize = 8 + 2_207;
    const SB_OFF_LAST_UPDATE_TIMESTAMP: usize = 8 + 2_208;
    const SB_OFF_RESULT_VALUE: usize = 8 + 2_256;
    const SB_OFF_RESULT_STD_DEV: usize = 8 + 2_272;
    const SB_OFF_RESULT_NUM_SAMPLES: usize = 8 + 2_352;
    const SB_OFF_RESULT_SLOT: usize = 8 + 2_360;
    const CHAINLINK_TRANSMISSIONS_DISCRIMINATOR: [u8; 8] = [96, 179, 69, 66, 128, 129, 73, 117];
    const CHAINLINK_HEADER_SIZE: usize = 192;
    const CHAINLINK_FEED_MIN_LEN: usize = 8 + CHAINLINK_HEADER_SIZE + 48;
    const CL_OFF_VERSION: usize = 8;
    const CL_OFF_DECIMALS: usize = 138;
    const CL_OFF_LATEST_ROUND_ID: usize = 143;
    const CL_OFF_LIVE_LENGTH: usize = 148;
    const CL_OFF_TRANSMISSION: usize = 8 + CHAINLINK_HEADER_SIZE;
    const CL_TRANS_OFF_SLOT: usize = 0;
    const CL_TRANS_OFF_TIMESTAMP: usize = 8;
    const CL_TRANS_OFF_ANSWER: usize = 16;

    pub fn is_hybrid(config: &WrapperConfigV16) -> bool {
        config.oracle_mode == ORACLE_MODE_HYBRID_AFTER_HOURS
    }

    pub fn is_ewma_mark(config: &WrapperConfigV16) -> bool {
        config.oracle_mode == ORACLE_MODE_EWMA_MARK
    }

    pub fn is_auth_mark(config: &WrapperConfigV16) -> bool {
        config.oracle_mode == ORACLE_MODE_AUTH_MARK
    }

    pub fn profile_is_hybrid(profile: &AssetOracleProfileV16) -> bool {
        profile.oracle_mode == ORACLE_MODE_HYBRID_AFTER_HOURS
    }

    pub fn profile_is_ewma_mark(profile: &AssetOracleProfileV16) -> bool {
        profile.oracle_mode == ORACLE_MODE_EWMA_MARK
    }

    pub fn profile_is_auth_mark(profile: &AssetOracleProfileV16) -> bool {
        profile.oracle_mode == ORACLE_MODE_AUTH_MARK
    }

    pub fn profile_is_price_managed(profile: &AssetOracleProfileV16) -> bool {
        profile_is_hybrid(profile) || profile_is_ewma_mark(profile) || profile_is_auth_mark(profile)
    }

    pub fn hybrid_soft_stale_matured(config: &WrapperConfigV16, now_slot: u64) -> bool {
        is_hybrid(config)
            && config.hybrid_soft_stale_slots != 0
            && now_slot.saturating_sub(config.last_good_oracle_slot)
                > config.hybrid_soft_stale_slots
    }

    pub fn profile_hybrid_soft_stale_matured(
        profile: &AssetOracleProfileV16,
        now_slot: u64,
    ) -> bool {
        profile_is_hybrid(profile)
            && profile.hybrid_soft_stale_slots != 0
            && now_slot.saturating_sub(profile.last_good_oracle_slot)
                > profile.hybrid_soft_stale_slots
    }

    pub fn hard_stale_matured(config: &WrapperConfigV16, now_slot: u64) -> bool {
        is_hybrid(config) && permissionless_stale_matured(config, now_slot)
    }

    pub fn permissionless_stale_matured(config: &WrapperConfigV16, now_slot: u64) -> bool {
        config.permissionless_resolve_stale_slots != 0
            && now_slot.saturating_sub(config.last_good_oracle_slot)
                >= config.permissionless_resolve_stale_slots
    }

    pub fn oracle_leg_config_ok(count: u8, flags: u8, feeds: &[[u8; 32]; ORACLE_LEG_CAP]) -> bool {
        if flags & !ORACLE_LEG_FLAGS_MASK != 0 {
            return false;
        }
        if count == 0 {
            return flags == 0 && feeds.iter().all(|f| *f == [0u8; 32]);
        }
        if count > ORACLE_LEG_CAP as u8 || feeds[0] == [0u8; 32] {
            return false;
        }
        if count == 1 {
            return flags == 0 && feeds[1] == [0u8; 32] && feeds[2] == [0u8; 32];
        }
        if feeds[1] == [0u8; 32] || feeds[1] == feeds[0] {
            return false;
        }
        if count == 2 {
            return (flags & ORACLE_LEG_FLAG_DIVIDE_LEG3) == 0 && feeds[2] == [0u8; 32];
        }
        feeds[2] != [0u8; 32] && feeds[2] != feeds[0] && feeds[2] != feeds[1]
    }

    fn leg_divides(config: &WrapperConfigV16, idx: usize) -> bool {
        match idx {
            1 => (config.oracle_leg_flags & ORACLE_LEG_FLAG_DIVIDE_LEG2) != 0,
            2 => (config.oracle_leg_flags & ORACLE_LEG_FLAG_DIVIDE_LEG3) != 0,
            _ => false,
        }
    }

    fn profile_leg_divides(profile: &AssetOracleProfileV16, idx: usize) -> bool {
        match idx {
            1 => (profile.oracle_leg_flags & ORACLE_LEG_FLAG_DIVIDE_LEG2) != 0,
            2 => (profile.oracle_leg_flags & ORACLE_LEG_FLAG_DIVIDE_LEG3) != 0,
            _ => false,
        }
    }

    pub fn read_pyth_price_e6(
        price_ai: &AccountInfo,
        expected_feed_id: &[u8; 32],
        now_unix_ts: i64,
        max_staleness_secs: u64,
        conf_bps: u16,
    ) -> Result<(u64, i64), ProgramError> {
        if *price_ai.owner != PYTH_RECEIVER_PROGRAM_ID {
            return Err(ProgramError::IllegalOwner);
        }
        let data = price_ai.try_borrow_data()?;
        if data.len() < PRICE_UPDATE_V2_MIN_LEN {
            return Err(ProgramError::InvalidAccountData);
        }
        if data[..8] != PYTH_PRICE_UPDATE_V2_DISCRIMINATOR {
            return Err(PercolatorError::OracleInvalid.into());
        }
        if data[OFF_VERIFICATION_LEVEL] != PYTH_VERIFICATION_FULL_TAG {
            return Err(PercolatorError::OracleInvalid.into());
        }
        let msg = <PriceFeedMessage as BorshDeserialize>::deserialize(
            &mut &data[OFF_PRICE_FEED_MESSAGE..],
        )
        .map_err(|_| PercolatorError::OracleInvalid)?;
        if &msg.feed_id != expected_feed_id {
            return Err(PercolatorError::InvalidOracleKey.into());
        }
        if msg.price <= 0 || msg.exponent < -MAX_EXPO_ABS || msg.exponent > MAX_EXPO_ABS {
            return Err(PercolatorError::OracleInvalid.into());
        }
        let age = now_unix_ts.saturating_sub(msg.publish_time);
        if age < 0 || age as u64 > max_staleness_secs {
            return Err(PercolatorError::OracleStale.into());
        }
        let price_u = msg.price as u128;
        if conf_bps != 0 && (msg.conf as u128).saturating_mul(10_000) > price_u * conf_bps as u128 {
            return Err(PercolatorError::OracleConfTooWide.into());
        }
        let scale = msg.exponent + 6;
        let out = if scale >= 0 {
            price_u
                .checked_mul(10u128.pow(scale as u32))
                .ok_or(PercolatorError::EngineArithmeticOverflow)?
        } else {
            price_u / 10u128.pow((-scale) as u32)
        };
        if out == 0 || out > percolator::MAX_ORACLE_PRICE as u128 {
            return Err(PercolatorError::OracleInvalid.into());
        }
        Ok((out as u64, msg.publish_time))
    }

    #[inline]
    fn read_u32_le(data: &[u8], off: usize) -> Result<u32, ProgramError> {
        let bytes: [u8; 4] = data
            .get(off..off + 4)
            .ok_or(ProgramError::InvalidAccountData)?
            .try_into()
            .unwrap();
        Ok(u32::from_le_bytes(bytes))
    }

    #[inline]
    fn read_u64_le(data: &[u8], off: usize) -> Result<u64, ProgramError> {
        let bytes: [u8; 8] = data
            .get(off..off + 8)
            .ok_or(ProgramError::InvalidAccountData)?
            .try_into()
            .unwrap();
        Ok(u64::from_le_bytes(bytes))
    }

    #[inline]
    fn read_i64_le(data: &[u8], off: usize) -> Result<i64, ProgramError> {
        let bytes: [u8; 8] = data
            .get(off..off + 8)
            .ok_or(ProgramError::InvalidAccountData)?
            .try_into()
            .unwrap();
        Ok(i64::from_le_bytes(bytes))
    }

    #[inline]
    fn read_i128_le(data: &[u8], off: usize) -> Result<i128, ProgramError> {
        let bytes: [u8; 16] = data
            .get(off..off + 16)
            .ok_or(ProgramError::InvalidAccountData)?
            .try_into()
            .unwrap();
        Ok(i128::from_le_bytes(bytes))
    }

    fn scale_decimal_to_e6(mantissa: i128, scale: u32) -> Result<u64, ProgramError> {
        if mantissa <= 0 || scale > MAX_EXPO_ABS as u32 {
            return Err(PercolatorError::OracleInvalid.into());
        }
        let mantissa = mantissa as u128;
        let out = if scale >= 6 {
            mantissa / 10u128.pow(scale - 6)
        } else {
            mantissa
                .checked_mul(10u128.pow(6 - scale))
                .ok_or(PercolatorError::EngineArithmeticOverflow)?
        };
        if out == 0 || out > percolator::MAX_ORACLE_PRICE as u128 {
            return Err(PercolatorError::OracleInvalid.into());
        }
        Ok(out as u64)
    }

    pub fn read_switchboard_price_e6(
        price_ai: &AccountInfo,
        expected_feed_key: &[u8; 32],
        now_unix_ts: i64,
        max_staleness_secs: u64,
        conf_bps: u16,
    ) -> Result<(u64, i64), ProgramError> {
        if *price_ai.owner != SWITCHBOARD_ON_DEMAND_MAINNET_PROGRAM_ID
            && *price_ai.owner != SWITCHBOARD_ON_DEMAND_DEVNET_PROGRAM_ID
        {
            return Err(ProgramError::IllegalOwner);
        }
        if price_ai.key.to_bytes() != *expected_feed_key {
            return Err(PercolatorError::InvalidOracleKey.into());
        }
        let data = price_ai.try_borrow_data()?;
        if data.len() < SWITCHBOARD_PULL_FEED_MIN_LEN {
            return Err(ProgramError::InvalidAccountData);
        }
        if data[..8] != SWITCHBOARD_PULL_FEED_DISCRIMINATOR {
            return Err(PercolatorError::OracleInvalid.into());
        }
        let feed_hash: [u8; 32] = data[SB_OFF_FEED_HASH..SB_OFF_FEED_HASH + 32]
            .try_into()
            .unwrap();
        let min_sample_size = data[SB_OFF_MIN_SAMPLE_SIZE];
        let publish_time = read_i64_le(&data, SB_OFF_LAST_UPDATE_TIMESTAMP)?;
        let value = read_i128_le(&data, SB_OFF_RESULT_VALUE)?;
        let std_dev = read_i128_le(&data, SB_OFF_RESULT_STD_DEV)?;
        let num_samples = data[SB_OFF_RESULT_NUM_SAMPLES];
        let result_slot = read_u64_le(&data, SB_OFF_RESULT_SLOT)?;
        if feed_hash == [0u8; 32]
            || min_sample_size == 0
            || num_samples < min_sample_size
            || result_slot == 0
            || publish_time <= 0
            || value <= 0
            || std_dev < 0
        {
            return Err(PercolatorError::OracleInvalid.into());
        }
        let age = now_unix_ts.saturating_sub(publish_time);
        if age < 0 || age as u64 > max_staleness_secs {
            return Err(PercolatorError::OracleStale.into());
        }
        let value_u = value as u128;
        if conf_bps != 0 && (std_dev as u128).saturating_mul(10_000) > value_u * conf_bps as u128 {
            return Err(PercolatorError::OracleConfTooWide.into());
        }
        let out = value_u / SWITCHBOARD_RESULT_SCALE;
        if out == 0 || out > percolator::MAX_ORACLE_PRICE as u128 {
            return Err(PercolatorError::OracleInvalid.into());
        }
        Ok((out as u64, publish_time))
    }

    pub fn read_chainlink_price_e6(
        price_ai: &AccountInfo,
        expected_feed_key: &[u8; 32],
        now_unix_ts: i64,
        max_staleness_secs: u64,
    ) -> Result<(u64, i64), ProgramError> {
        if *price_ai.owner != CHAINLINK_STORE_PROGRAM_ID {
            return Err(ProgramError::IllegalOwner);
        }
        if price_ai.key.to_bytes() != *expected_feed_key {
            return Err(PercolatorError::InvalidOracleKey.into());
        }
        let data = price_ai.try_borrow_data()?;
        if data.len() < CHAINLINK_FEED_MIN_LEN {
            return Err(ProgramError::InvalidAccountData);
        }
        if data[..8] != CHAINLINK_TRANSMISSIONS_DISCRIMINATOR {
            return Err(PercolatorError::OracleInvalid.into());
        }
        let version = data[CL_OFF_VERSION];
        let decimals = data[CL_OFF_DECIMALS];
        let latest_round_id = read_u32_le(&data, CL_OFF_LATEST_ROUND_ID)?;
        let live_length = read_u32_le(&data, CL_OFF_LIVE_LENGTH)?;
        let tx = CL_OFF_TRANSMISSION;
        let result_slot = read_u64_le(&data, tx + CL_TRANS_OFF_SLOT)?;
        let publish_time = read_u32_le(&data, tx + CL_TRANS_OFF_TIMESTAMP)? as i64;
        let answer = read_i128_le(&data, tx + CL_TRANS_OFF_ANSWER)?;
        if version == 0
            || latest_round_id == 0
            || live_length != 1
            || result_slot == 0
            || publish_time <= 0
        {
            return Err(PercolatorError::OracleInvalid.into());
        }
        let age = now_unix_ts.saturating_sub(publish_time);
        if age < 0 || age as u64 > max_staleness_secs {
            return Err(PercolatorError::OracleStale.into());
        }
        scale_decimal_to_e6(answer, decimals as u32).map(|p| (p, publish_time))
    }

    pub fn read_oracle_price_e6(
        price_ai: &AccountInfo,
        expected_feed_id: &[u8; 32],
        now_unix_ts: i64,
        max_staleness_secs: u64,
        conf_bps: u16,
    ) -> Result<(u64, i64), ProgramError> {
        if *price_ai.owner == PYTH_RECEIVER_PROGRAM_ID {
            read_pyth_price_e6(
                price_ai,
                expected_feed_id,
                now_unix_ts,
                max_staleness_secs,
                conf_bps,
            )
        } else if *price_ai.owner == SWITCHBOARD_ON_DEMAND_MAINNET_PROGRAM_ID
            || *price_ai.owner == SWITCHBOARD_ON_DEMAND_DEVNET_PROGRAM_ID
        {
            read_switchboard_price_e6(
                price_ai,
                expected_feed_id,
                now_unix_ts,
                max_staleness_secs,
                conf_bps,
            )
        } else if *price_ai.owner == CHAINLINK_STORE_PROGRAM_ID {
            read_chainlink_price_e6(price_ai, expected_feed_id, now_unix_ts, max_staleness_secs)
        } else {
            Err(ProgramError::IllegalOwner)
        }
    }

    fn apply_transform(raw_price: u64, invert: u8, unit_scale: u32) -> Result<u64, ProgramError> {
        let mut price = raw_price;
        if invert != 0 {
            price = (1_000_000_000_000u128 / price as u128)
                .try_into()
                .map_err(|_| PercolatorError::OracleInvalid)?;
        }
        if unit_scale > 1 {
            price /= unit_scale as u64;
        }
        if price == 0 || price > percolator::MAX_ORACLE_PRICE {
            return Err(PercolatorError::OracleInvalid.into());
        }
        Ok(price)
    }

    fn compose(acc_e6: u64, leg_e6: u64, divide: bool) -> Result<u64, ProgramError> {
        if leg_e6 == 0 {
            return Err(PercolatorError::OracleInvalid.into());
        }
        let next = if divide {
            (acc_e6 as u128)
                .checked_mul(1_000_000)
                .ok_or(PercolatorError::EngineArithmeticOverflow)?
                / leg_e6 as u128
        } else {
            (acc_e6 as u128)
                .checked_mul(leg_e6 as u128)
                .ok_or(PercolatorError::EngineArithmeticOverflow)?
                / 1_000_000
        };
        if next == 0 || next > percolator::MAX_ORACLE_PRICE as u128 {
            return Err(PercolatorError::OracleInvalid.into());
        }
        Ok(next as u64)
    }

    pub fn read_external_price_e6(
        config: &mut WrapperConfigV16,
        oracle_accounts: &[AccountInfo],
        now_unix_ts: i64,
    ) -> Result<(u64, i64, bool), ProgramError> {
        if config.oracle_mode == ORACLE_MODE_MANUAL {
            return Err(PercolatorError::OracleInvalid.into());
        }
        let count = config.oracle_leg_count as usize;
        if count == 0 || count > ORACLE_LEG_CAP || oracle_accounts.len() < count {
            return Err(ProgramError::NotEnoughAccountKeys);
        }
        if !oracle_leg_config_ok(
            config.oracle_leg_count,
            config.oracle_leg_flags,
            &config.oracle_leg_feeds,
        ) {
            return Err(PercolatorError::EngineInvalidConfig.into());
        }
        let mut acc = 0u64;
        let mut advanced = false;
        let mut max_publish_time = i64::MIN;
        let mut i = 0usize;
        while i < count {
            let (price, publish_time) = read_oracle_price_e6(
                &oracle_accounts[i],
                &config.oracle_leg_feeds[i],
                now_unix_ts,
                config.max_staleness_secs,
                config.conf_filter_bps,
            )?;
            let prev_time = config.oracle_leg_publish_times[i];
            let prev_price = config.oracle_leg_prices_e6[i];
            if prev_time != 0 {
                if publish_time < prev_time {
                    return Err(PercolatorError::OracleStale.into());
                }
                if publish_time == prev_time && prev_price != 0 && price != prev_price {
                    return Err(PercolatorError::OracleInvalid.into());
                }
            }
            if publish_time > prev_time {
                config.oracle_leg_publish_times[i] = publish_time;
                config.oracle_leg_prices_e6[i] = price;
                advanced = true;
            }
            max_publish_time = core::cmp::max(max_publish_time, publish_time);
            acc = if i == 0 {
                price
            } else {
                compose(acc, price, leg_divides(config, i))?
            };
            i += 1;
        }
        Ok((
            apply_transform(acc, config.invert, config.unit_scale)?,
            max_publish_time,
            advanced,
        ))
    }

    pub fn read_external_price_e6_profile(
        profile: &mut AssetOracleProfileV16,
        oracle_accounts: &[AccountInfo],
        now_unix_ts: i64,
    ) -> Result<(u64, i64, bool), ProgramError> {
        if profile.oracle_mode == ORACLE_MODE_MANUAL {
            return Err(PercolatorError::OracleInvalid.into());
        }
        let count = profile.oracle_leg_count as usize;
        if count == 0 || count > ORACLE_LEG_CAP || oracle_accounts.len() < count {
            return Err(ProgramError::NotEnoughAccountKeys);
        }
        if !oracle_leg_config_ok(
            profile.oracle_leg_count,
            profile.oracle_leg_flags,
            &profile.oracle_leg_feeds,
        ) {
            return Err(PercolatorError::EngineInvalidConfig.into());
        }
        let mut acc = 0u64;
        let mut advanced = false;
        let mut max_publish_time = i64::MIN;
        let mut i = 0usize;
        while i < count {
            let (price, publish_time) = read_oracle_price_e6(
                &oracle_accounts[i],
                &profile.oracle_leg_feeds[i],
                now_unix_ts,
                profile.max_staleness_secs,
                profile.conf_filter_bps,
            )?;
            let prev_time = profile.oracle_leg_publish_times[i];
            let prev_price = profile.oracle_leg_prices_e6[i];
            if prev_time != 0 {
                if publish_time < prev_time {
                    return Err(PercolatorError::OracleStale.into());
                }
                if publish_time == prev_time && prev_price != 0 && price != prev_price {
                    return Err(PercolatorError::OracleInvalid.into());
                }
            }
            if publish_time > prev_time {
                profile.oracle_leg_publish_times[i] = publish_time;
                profile.oracle_leg_prices_e6[i] = price;
                advanced = true;
            }
            max_publish_time = core::cmp::max(max_publish_time, publish_time);
            acc = if i == 0 {
                price
            } else {
                compose(acc, price, profile_leg_divides(profile, i))?
            };
            i += 1;
        }
        Ok((
            apply_transform(acc, profile.invert, profile.unit_scale)?,
            max_publish_time,
            advanced,
        ))
    }

    pub fn clamp_toward_engine_dt(p_last: u64, target: u64, cap_bps: u64, dt_slots: u64) -> u64 {
        if p_last == 0 || target == 0 {
            return target;
        }
        if cap_bps == 0 || dt_slots == 0 {
            return p_last;
        }
        let max_delta = (p_last as u128)
            .saturating_mul(cap_bps as u128)
            .saturating_mul(dt_slots as u128)
            / 10_000;
        let max_delta = core::cmp::min(max_delta, u64::MAX as u128) as u64;
        if target > p_last {
            core::cmp::min(target, p_last.saturating_add(max_delta))
        } else {
            core::cmp::max(target, p_last.saturating_sub(max_delta))
        }
    }

    pub fn effective_price_from_target(
        anchor: u64,
        target: u64,
        max_change_bps: u64,
        dt_slots: u64,
        exposed: bool,
    ) -> u64 {
        if exposed {
            clamp_toward_engine_dt(anchor, target, max_change_bps, dt_slots)
        } else {
            target
        }
    }
}

pub mod policy_v16 {
    use crate::constants::MAX_DYNAMIC_TRADE_FEE_BPS;

    pub fn price_move_bps_ceil(old: u64, new: u64) -> Option<u64> {
        if old == 0 || old == new {
            return Some(0);
        }
        let diff = old.abs_diff(new) as u128;
        let den = old as u128;
        let bps = diff.checked_mul(10_000)?.checked_add(den.checked_sub(1)?)? / den;
        u64::try_from(bps).ok()
    }

    fn two_sided_trade_fee_paid_cap(notional: u128, fee_bps: u64) -> Option<u64> {
        if notional == 0 || fee_bps == 0 {
            return Some(0);
        }
        let one_side = notional.checked_mul(fee_bps as u128)?.checked_add(9_999)? / 10_000;
        u64::try_from(one_side.checked_mul(2)?).ok()
    }

    fn ceil_div_u128(num: u128, den: u128) -> Option<u128> {
        if den == 0 {
            return None;
        }
        Some(num.checked_add(den.checked_sub(1)?)? / den)
    }

    fn ewma_effective_alpha_bps(alpha_bps: u128, fee_paid: u64, mark_min_fee: u64) -> u128 {
        if mark_min_fee == 0 || fee_paid >= mark_min_fee {
            alpha_bps
        } else {
            alpha_bps.saturating_mul(fee_paid as u128) / mark_min_fee as u128
        }
    }

    pub fn ewma_update(
        old: u64,
        price: u64,
        halflife_slots: u64,
        last_slot: u64,
        now_slot: u64,
        fee_paid: u64,
        mark_min_fee: u64,
    ) -> u64 {
        if old == 0 {
            if mark_min_fee > 0 && fee_paid < mark_min_fee {
                return 0;
            }
            return price;
        }
        let dt = now_slot.saturating_sub(last_slot);
        if dt == 0 {
            return old;
        }
        if halflife_slots == 0 {
            return price;
        }
        if fee_paid == 0 && mark_min_fee > 0 {
            return old;
        }
        let alpha_bps = (10_000u128 * dt as u128) / (dt as u128 + halflife_slots as u128);
        let alpha_bps = ewma_effective_alpha_bps(alpha_bps, fee_paid, mark_min_fee);
        let old128 = old as u128;
        let price128 = price as u128;
        let out = if price >= old {
            old128 + ((price128 - old128) * alpha_bps / 10_000)
        } else {
            old128 - ((old128 - price128) * alpha_bps / 10_000)
        };
        core::cmp::min(out, u64::MAX as u128) as u64
    }

    pub fn dynamic_fee_bps_with_externality_floor(
        base_fee_bps: u64,
        old_mark_e6: u64,
        clamped_exec_e6: u64,
        halflife_slots: u64,
        last_mark_slot: u64,
        now_slot: u64,
        trade_notional: u128,
        mark_externality_notional: u128,
        mark_min_fee: u64,
        min_externality_bps: u64,
    ) -> Option<u64> {
        if base_fee_bps > MAX_DYNAMIC_TRADE_FEE_BPS {
            return None;
        }
        let mut fee_bps = base_fee_bps;
        let mut i = 0;
        while i < 64 {
            let fee_paid = two_sided_trade_fee_paid_cap(trade_notional, fee_bps)?;
            let next_mark = ewma_update(
                old_mark_e6,
                clamped_exec_e6,
                halflife_slots,
                last_mark_slot,
                now_slot,
                fee_paid,
                mark_min_fee,
            );
            let mark_move_bps = price_move_bps_ceil(old_mark_e6, next_mark)?;
            let charged_move_bps = core::cmp::max(mark_move_bps, min_externality_bps);
            let base_paid = two_sided_trade_fee_paid_cap(trade_notional, base_fee_bps)? as u128;
            let mark_fee = ceil_div_u128(
                mark_externality_notional.checked_mul(charged_move_bps as u128)?,
                10_000,
            )?;
            let required = base_paid.checked_add(mark_fee)?;
            let denom = trade_notional.checked_mul(2)?;
            let needed = ceil_div_u128(required.checked_mul(10_000)?, denom)?;
            let needed = u64::try_from(needed).ok()?;
            if needed > MAX_DYNAMIC_TRADE_FEE_BPS {
                return None;
            }
            if needed <= fee_bps {
                return Some(fee_bps);
            }
            fee_bps = needed;
            i += 1;
        }
        None
    }
}

pub mod processor {
    use super::*;
    use crate::{
        error::{map_v16_error, PercolatorError},
        ix::Instruction,
        state::{self, WrapperConfigV16},
    };

    pub const AUTHORITY_ADMIN: u8 = 0;
    pub const AUTHORITY_MARK: u8 = 1;
    pub const AUTHORITY_INSURANCE: u8 = 2;
    pub const AUTHORITY_BACKING_BUCKET: u8 = 3;
    pub const AUTHORITY_INSURANCE_OPERATOR: u8 = 4;
    pub const AUTHORITY_ASSET: u8 = 5;
    pub const AUTHORITY_BASE_UNIT: u8 = 6;

    pub const ASSET_ACTION_ACTIVATE: u8 = 0;
    pub const ASSET_ACTION_DRAIN_ONLY: u8 = 1;
    pub const ASSET_ACTION_RETIRE: u8 = 2;
    pub const ASSET_ACTION_SHUTDOWN: u8 = 3;
    const ASSET_LIFECYCLE_ACTIVE: u8 = 2;
    const ASSET_LIFECYCLE_DRAIN_ONLY: u8 = 3;
    const ASSET_LIFECYCLE_RETIRED: u8 = 4;
    const ASSET_LIFECYCLE_RECOVERY: u8 = 5;
    const SIDE_MODE_NORMAL: u8 = 0;

    fn authenticated_slot_or_fallback(fallback_slot: u64) -> u64 {
        Clock::get().map(|c| c.slot).unwrap_or(fallback_slot)
    }

    fn authenticated_market_slot_or_fallback_view(group: &state::MarketViewMutV16<'_>) -> u64 {
        Clock::get()
            .map(|c| c.slot)
            .unwrap_or(group.header.current_slot.get())
    }

    fn decode_side(value: u8) -> Result<SideV16, ProgramError> {
        match value {
            0 => Ok(SideV16::Long),
            1 => Ok(SideV16::Short),
            _ => Err(PercolatorError::InvalidInstruction.into()),
        }
    }

    fn fraction_ge_wide(
        lhs_num: u128,
        lhs_den: u128,
        rhs_num: u128,
        rhs_den: u128,
    ) -> Result<bool, ProgramError> {
        if lhs_den == 0 || rhs_den == 0 {
            return Err(PercolatorError::EngineInvalidConfig.into());
        }
        let lhs = percolator::wide_math::U256::from_u128(lhs_num)
            .checked_mul(percolator::wide_math::U256::from_u128(rhs_den))
            .ok_or(PercolatorError::EngineArithmeticOverflow)?;
        let rhs = percolator::wide_math::U256::from_u128(rhs_num)
            .checked_mul(percolator::wide_math::U256::from_u128(lhs_den))
            .ok_or(PercolatorError::EngineArithmeticOverflow)?;
        Ok(lhs >= rhs)
    }

    #[inline(always)]
    fn permissionless_market_init_fee_for_asset(
        base_fee: u128,
        asset_index: usize,
    ) -> Result<u128, ProgramError> {
        let mut fee = base_fee;
        if fee == 0 {
            return Ok(0);
        }
        let mut doublings = asset_index / 32;
        while doublings != 0 {
            fee = fee
                .checked_mul(2)
                .ok_or(PercolatorError::EngineArithmeticOverflow)?;
            doublings -= 1;
        }
        Ok(fee)
    }

    fn validate_resolved_payout_receipt_view(
        receipt: percolator::ResolvedPayoutReceiptV16,
    ) -> Result<(), ProgramError> {
        if !receipt.present {
            if !receipt.is_empty() {
                return Err(PercolatorError::EngineInvalidLeg.into());
            }
            return Ok(());
        }
        let exact_num = receipt
            .terminal_positive_claim_face
            .checked_mul(BOUND_SCALE)
            .ok_or(PercolatorError::EngineArithmeticOverflow)?;
        if exact_num > receipt.prior_bound_contribution_num
            || receipt.paid_effective > receipt.terminal_positive_claim_face
            || receipt.finalized != (receipt.paid_effective == receipt.terminal_positive_claim_face)
        {
            return Err(PercolatorError::EngineInvalidLeg.into());
        }
        Ok(())
    }

    fn resolved_receipt_claimable_now_view(
        group: &state::MarketViewMutV16<'_>,
        receipt: percolator::ResolvedPayoutReceiptV16,
    ) -> Result<u128, ProgramError> {
        validate_resolved_payout_receipt_view(receipt)?;
        if !receipt.present {
            return Ok(0);
        }
        let ledger = &group.header.resolved_payout_ledger;
        if ledger.payout_halted > 1 || ledger.finalized > 1 {
            return Err(ProgramError::InvalidAccountData);
        }
        if ledger.payout_halted != 0 {
            return Err(PercolatorError::EngineRecoveryRequired.into());
        }
        let den = ledger.current_payout_rate_den.get();
        if den == 0 {
            return Err(PercolatorError::EngineInvalidConfig.into());
        }
        let gross = percolator::wide_math::wide_mul_div_floor_u128(
            receipt.terminal_positive_claim_face,
            ledger.current_payout_rate_num.get(),
            den,
        );
        gross
            .checked_sub(receipt.paid_effective)
            .ok_or(PercolatorError::EngineInvalidLeg.into())
    }

    fn permissionless_resolve_matured_now_view(
        cfg: &WrapperConfigV16,
        group: &state::MarketViewMutV16<'_>,
    ) -> bool {
        oracle_v16::permissionless_stale_matured(
            cfg,
            authenticated_market_slot_or_fallback_view(group),
        )
    }

    fn permissionless_resolve_matured_for_profile_view(
        cfg: &WrapperConfigV16,
        profile: &state::AssetOracleProfileV16,
        group: &state::MarketViewMutV16<'_>,
    ) -> bool {
        cfg.permissionless_resolve_stale_slots != 0
            && profile.last_good_oracle_slot != 0
            && authenticated_market_slot_or_fallback_view(group)
                .saturating_sub(profile.last_good_oracle_slot)
                >= cfg.permissionless_resolve_stale_slots
    }

    fn reject_permissionless_resolve_matured_live_view(
        cfg: &WrapperConfigV16,
        group: &state::MarketViewMutV16<'_>,
    ) -> ProgramResult {
        if group.header.mode == 0 && permissionless_resolve_matured_now_view(cfg, group) {
            return Err(PercolatorError::OracleStale.into());
        }
        Ok(())
    }

    fn reject_permissionless_resolve_matured_live_for_profile_view(
        cfg: &WrapperConfigV16,
        profile: &state::AssetOracleProfileV16,
        group: &state::MarketViewMutV16<'_>,
    ) -> ProgramResult {
        if !oracle_v16::profile_is_price_managed(profile) {
            return reject_permissionless_resolve_matured_live_view(cfg, group);
        }
        if group.header.mode == 0
            && permissionless_resolve_matured_for_profile_view(cfg, profile, group)
        {
            return Err(PercolatorError::OracleStale.into());
        }
        Ok(())
    }

    fn shutdown_asset_matured_at_slot_view(
        cfg: &WrapperConfigV16,
        group: &state::MarketViewMutV16<'_>,
        asset_index: usize,
        now_slot: u64,
    ) -> ProgramResult {
        if group.header.mode != 0
            || asset_index == 0
            || cfg.force_close_delay_slots == 0
            || asset_index >= group.header.config.max_market_slots.get() as usize
            || asset_index >= group.markets.len()
            || group.markets[asset_index].engine.asset.lifecycle != ASSET_LIFECYCLE_RECOVERY
        {
            return Err(PercolatorError::EngineLockActive.into());
        }
        let profile = read_oracle_profile_from_view(group, cfg, asset_index)?;
        let shutdown_slot = profile.last_good_oracle_slot;
        if shutdown_slot == 0
            || now_slot < shutdown_slot
            || now_slot.saturating_sub(shutdown_slot) < cfg.force_close_delay_slots
        {
            return Err(PercolatorError::EngineLockActive.into());
        }
        Ok(())
    }

    fn shutdown_asset_empty_and_matured_at_slot_view(
        cfg: &WrapperConfigV16,
        group: &state::MarketViewMutV16<'_>,
        asset_index: usize,
        now_slot: u64,
    ) -> ProgramResult {
        shutdown_asset_matured_at_slot_view(cfg, group, asset_index, now_slot)?;
        if asset_local_has_position_or_loss_state_view(group, asset_index) {
            return Err(PercolatorError::EngineLockActive.into());
        }
        Ok(())
    }

    fn shutdown_asset_empty_and_matured_now_view(
        cfg: &WrapperConfigV16,
        group: &state::MarketViewMutV16<'_>,
        asset_index: usize,
    ) -> ProgramResult {
        let now_slot = authenticated_market_slot_or_fallback_view(group);
        shutdown_asset_empty_and_matured_at_slot_view(cfg, group, asset_index, now_slot)
    }

    fn live_domain_withdraw_health_or_shutdown_view(
        cfg: &WrapperConfigV16,
        group: &state::MarketViewMutV16<'_>,
        domain: usize,
    ) -> Result<bool, ProgramError> {
        let asset_index = domain / 2;
        if shutdown_asset_empty_and_matured_now_view(cfg, group, asset_index).is_ok() {
            return Ok(true);
        }
        reject_permissionless_resolve_matured_live_view(cfg, group)?;
        if group.header.bankruptcy_hlock_active != 0
            || group.header.threshold_stress_active != 0
            || group.header.loss_stale_active != 0
            || group
                .header
                .recovery_reason
                .try_to_runtime()
                .map_err(map_v16_error)?
                .is_some()
        {
            return Err(PercolatorError::EngineLockActive.into());
        }
        Ok(false)
    }

    fn read_oracle_profile_for_asset(
        market_data: &[u8],
        cfg: &WrapperConfigV16,
        asset_index: usize,
    ) -> Result<state::AssetOracleProfileV16, ProgramError> {
        if asset_index == 0 {
            Ok(state::asset_oracle_profile_from_config(cfg))
        } else {
            state::read_asset_oracle_profile(market_data, asset_index)
        }
    }

    fn read_oracle_profile_from_view(
        group: &state::MarketViewMutV16<'_>,
        cfg: &WrapperConfigV16,
        asset_index: usize,
    ) -> Result<state::AssetOracleProfileV16, ProgramError> {
        if asset_index == 0 {
            return Ok(state::asset_oracle_profile_from_config(cfg));
        }
        let market = group
            .markets
            .get(asset_index)
            .ok_or(PercolatorError::InvalidInstruction)?;
        let bytes = market
            .wrapper
            .get(..constants::ASSET_ORACLE_PROFILE_LEN)
            .ok_or(PercolatorError::InvalidAccountLen)?;
        let profile: state::AssetOracleProfileV16 = bytemuck::pod_read_unaligned(bytes);
        state::validate_asset_oracle_profile(&profile)?;
        Ok(profile)
    }

    fn write_oracle_profile_to_view_if_separate(
        group: &mut state::MarketViewMutV16<'_>,
        asset_index: usize,
        profile: &state::AssetOracleProfileV16,
    ) -> ProgramResult {
        if asset_index != 0 {
            state::validate_asset_oracle_profile(profile)?;
            let market = group
                .markets
                .get_mut(asset_index)
                .ok_or(PercolatorError::InvalidInstruction)?;
            market.wrapper[..constants::ASSET_ORACLE_PROFILE_LEN]
                .copy_from_slice(bytemuck::bytes_of(profile));
        }
        Ok(())
    }

    fn mirror_manual_profile_to_base_config(
        cfg: &mut WrapperConfigV16,
        profile: &state::AssetOracleProfileV16,
        refresh_liveness: bool,
    ) {
        cfg.oracle_mode = constants::ORACLE_MODE_MANUAL;
        cfg.oracle_leg_count = 0;
        cfg.oracle_leg_flags = 0;
        cfg.invert = 0;
        cfg.unit_scale = 0;
        cfg.conf_filter_bps = 0;
        cfg.max_staleness_secs = 0;
        cfg.hybrid_soft_stale_slots = 0;
        cfg.mark_ewma_e6 = profile.mark_ewma_e6;
        cfg.mark_ewma_last_slot = profile.mark_ewma_last_slot;
        cfg.mark_ewma_halflife_slots = profile.mark_ewma_halflife_slots;
        cfg.mark_min_fee = 0;
        cfg.oracle_target_price_e6 = profile.oracle_target_price_e6;
        cfg.oracle_target_publish_time = 0;
        if refresh_liveness {
            cfg.last_good_oracle_slot = profile.last_good_oracle_slot;
        }
        cfg.oracle_leg_feeds = [[0u8; 32]; constants::ORACLE_LEG_CAP];
        cfg.oracle_leg_prices_e6 = [0u64; constants::ORACLE_LEG_CAP];
        cfg.oracle_leg_publish_times = [0i64; constants::ORACLE_LEG_CAP];
    }

    fn preserve_backing_fee_policy(
        mut profile: state::AssetOracleProfileV16,
        existing: &state::AssetOracleProfileV16,
    ) -> state::AssetOracleProfileV16 {
        profile.backing_trade_fee_bps_long = existing.backing_trade_fee_bps_long;
        profile.backing_trade_fee_bps_short = existing.backing_trade_fee_bps_short;
        profile.backing_trade_fee_insurance_share_bps_long =
            existing.backing_trade_fee_insurance_share_bps_long;
        profile.backing_trade_fee_insurance_share_bps_short =
            existing.backing_trade_fee_insurance_share_bps_short;
        profile.insurance_authority = existing.insurance_authority;
        profile.insurance_operator = existing.insurance_operator;
        profile.backing_bucket_authority = existing.backing_bucket_authority;
        profile.oracle_authority = existing.oracle_authority;
        profile
    }

    #[derive(Clone, Copy)]
    struct DomainAuthoritiesV16 {
        insurance_authority: [u8; 32],
        insurance_operator: [u8; 32],
        backing_bucket_authority: [u8; 32],
        oracle_authority: [u8; 32],
    }

    fn domain_authorities_from_profile(
        cfg: &WrapperConfigV16,
        profile: &state::AssetOracleProfileV16,
        asset_index: usize,
    ) -> DomainAuthoritiesV16 {
        if asset_index == 0 {
            DomainAuthoritiesV16 {
                insurance_authority: cfg.insurance_authority,
                insurance_operator: cfg.insurance_operator,
                backing_bucket_authority: cfg.backing_bucket_authority,
                oracle_authority: cfg.mark_authority,
            }
        } else {
            DomainAuthoritiesV16 {
                insurance_authority: profile.insurance_authority,
                insurance_operator: profile.insurance_operator,
                backing_bucket_authority: profile.backing_bucket_authority,
                oracle_authority: profile.oracle_authority,
            }
        }
    }

    fn domain_authorities_from_view(
        group: &state::MarketViewMutV16<'_>,
        cfg: &WrapperConfigV16,
        domain: usize,
    ) -> Result<DomainAuthoritiesV16, ProgramError> {
        let asset_index = domain / 2;
        if domain >= (group.header.config.max_market_slots.get() as usize).saturating_mul(2)
            || asset_index >= group.markets.len()
        {
            return Err(PercolatorError::InvalidInstruction.into());
        }
        let profile = read_oracle_profile_from_view(group, cfg, asset_index)?;
        Ok(domain_authorities_from_profile(cfg, &profile, asset_index))
    }

    fn domain_budget_parts_view(
        group: &state::MarketViewMutV16<'_>,
        domain: usize,
    ) -> Result<(u128, u128), ProgramError> {
        let asset_index = domain / 2;
        if domain >= (group.header.config.max_market_slots.get() as usize).saturating_mul(2)
            || asset_index >= group.markets.len()
        {
            return Err(PercolatorError::InvalidInstruction.into());
        }
        let slot = &group.markets[asset_index].engine;
        Ok(if domain % 2 == 0 {
            (
                slot.insurance_domain_budget_long.get(),
                slot.insurance_domain_spent_long.get(),
            )
        } else {
            (
                slot.insurance_domain_budget_short.get(),
                slot.insurance_domain_spent_short.get(),
            )
        })
    }

    fn set_domain_budget_view(
        group: &mut state::MarketViewMutV16<'_>,
        domain: usize,
        budget: u128,
    ) -> ProgramResult {
        let asset_index = domain / 2;
        if domain >= (group.header.config.max_market_slots.get() as usize).saturating_mul(2)
            || asset_index >= group.markets.len()
        {
            return Err(PercolatorError::InvalidInstruction.into());
        }
        let slot = &mut group.markets[asset_index].engine;
        if domain % 2 == 0 {
            slot.insurance_domain_budget_long = percolator::V16PodU128::new(budget);
        } else {
            slot.insurance_domain_budget_short = percolator::V16PodU128::new(budget);
        }
        Ok(())
    }

    fn add_to_domain_budget_view(
        group: &mut state::MarketViewMutV16<'_>,
        domain: usize,
        amount: u128,
    ) -> ProgramResult {
        if amount == 0 {
            return Ok(());
        }
        let (budget, _) = domain_budget_parts_view(group, domain)?;
        set_domain_budget_view(
            group,
            domain,
            budget
                .checked_add(amount)
                .ok_or(PercolatorError::EngineArithmeticOverflow)?,
        )
    }

    fn domain_budget_remaining_view(
        group: &state::MarketViewMutV16<'_>,
        domain: usize,
    ) -> Result<u128, ProgramError> {
        let (budget, spent) = domain_budget_parts_view(group, domain)?;
        budget
            .checked_sub(spent)
            .ok_or(PercolatorError::EngineCounterUnderflow.into())
    }

    fn credit_market_insurance_budget_view(
        group: &mut state::MarketViewMutV16<'_>,
        asset_index: usize,
        amount: u128,
    ) -> ProgramResult {
        if amount == 0 {
            return Ok(());
        }
        let long_amount = amount / 2;
        let short_amount = amount
            .checked_sub(long_amount)
            .ok_or(PercolatorError::EngineCounterUnderflow)?;
        add_to_domain_budget_view(group, asset_index * 2, long_amount)?;
        add_to_domain_budget_view(group, asset_index * 2 + 1, short_amount)
    }

    fn market_insurance_remaining_view(
        group: &state::MarketViewMutV16<'_>,
        asset_index: usize,
    ) -> Result<u128, ProgramError> {
        let long = domain_budget_remaining_view(group, asset_index * 2)?;
        let short = domain_budget_remaining_view(group, asset_index * 2 + 1)?;
        let budget = long
            .checked_add(short)
            .ok_or(PercolatorError::EngineArithmeticOverflow)?;
        Ok(budget.min(group.header.insurance.get()))
    }

    fn terminal_insurance_remaining_for_authority_view(
        group: &state::MarketViewMutV16<'_>,
        cfg: &WrapperConfigV16,
        authority: &Pubkey,
    ) -> Result<u128, ProgramError> {
        let authority_bytes = authority.to_bytes();
        if authority_bytes == [0u8; 32] {
            return Err(PercolatorError::Unauthorized.into());
        }
        let domain_count = (group.header.config.max_market_slots.get() as usize)
            .checked_mul(2)
            .ok_or(PercolatorError::EngineArithmeticOverflow)?;
        let mut total = 0u128;
        let mut domain = 0usize;
        while domain < domain_count {
            let authorities = domain_authorities_from_view(group, cfg, domain)?;
            if authorities.insurance_authority == authority_bytes {
                total = total
                    .checked_add(domain_budget_remaining_view(group, domain)?)
                    .ok_or(PercolatorError::EngineArithmeticOverflow)?;
            }
            domain += 1;
        }
        Ok(total.min(group.header.insurance.get()))
    }

    fn debit_terminal_insurance_budgets_for_authority_view(
        group: &mut state::MarketViewMutV16<'_>,
        cfg: &WrapperConfigV16,
        authority: &Pubkey,
        mut amount: u128,
    ) -> ProgramResult {
        if amount == 0 {
            return Ok(());
        }
        let authority_bytes = authority.to_bytes();
        if authority_bytes == [0u8; 32] {
            return Err(PercolatorError::Unauthorized.into());
        }
        let domain_count = (group.header.config.max_market_slots.get() as usize)
            .checked_mul(2)
            .ok_or(PercolatorError::EngineArithmeticOverflow)?;
        let mut domain = 0usize;
        while domain < domain_count && amount != 0 {
            let authorities = domain_authorities_from_view(group, cfg, domain)?;
            if authorities.insurance_authority == authority_bytes {
                let remaining = domain_budget_remaining_view(group, domain)?;
                let debit = remaining.min(amount);
                if debit != 0 {
                    let (budget, _) = domain_budget_parts_view(group, domain)?;
                    set_domain_budget_view(
                        group,
                        domain,
                        budget
                            .checked_sub(debit)
                            .ok_or(PercolatorError::EngineCounterUnderflow)?,
                    )?;
                    amount = amount
                        .checked_sub(debit)
                        .ok_or(PercolatorError::EngineCounterUnderflow)?;
                }
            }
            domain += 1;
        }
        if amount != 0 {
            return Err(PercolatorError::EngineCounterUnderflow.into());
        }
        Ok(())
    }

    fn debit_market_insurance_budget_view(
        group: &mut state::MarketViewMutV16<'_>,
        asset_index: usize,
        mut amount: u128,
    ) -> ProgramResult {
        if amount == 0 {
            return Ok(());
        }
        let long_domain = asset_index * 2;
        let short_domain = long_domain + 1;
        let long_remaining = domain_budget_remaining_view(group, long_domain)?;
        let long_debit = long_remaining.min(amount);
        if long_debit != 0 {
            let (budget, _) = domain_budget_parts_view(group, long_domain)?;
            set_domain_budget_view(
                group,
                long_domain,
                budget
                    .checked_sub(long_debit)
                    .ok_or(PercolatorError::EngineCounterUnderflow)?,
            )?;
            amount = amount
                .checked_sub(long_debit)
                .ok_or(PercolatorError::EngineCounterUnderflow)?;
        }
        if amount != 0 {
            let short_remaining = domain_budget_remaining_view(group, short_domain)?;
            if amount > short_remaining {
                return Err(PercolatorError::EngineCounterUnderflow.into());
            }
            let (budget, _) = domain_budget_parts_view(group, short_domain)?;
            set_domain_budget_view(
                group,
                short_domain,
                budget
                    .checked_sub(amount)
                    .ok_or(PercolatorError::EngineCounterUnderflow)?,
            )?;
        }
        Ok(())
    }

    fn credit_fee_to_domain_budget_view(
        cfg: &WrapperConfigV16,
        group: &mut state::MarketViewMutV16<'_>,
        domain: usize,
        amount: u128,
    ) -> ProgramResult {
        if amount == 0 {
            return Ok(());
        }
        let asset_index = domain / 2;
        let redirect = if asset_index == 0 {
            0
        } else {
            fee_share_floor(amount, cfg.fee_redirect_to_market_0_bps)?
        };
        let domain_amount = amount
            .checked_sub(redirect)
            .ok_or(PercolatorError::EngineCounterUnderflow)?;
        add_to_domain_budget_view(group, domain, domain_amount)?;
        credit_market_insurance_budget_view(group, 0, redirect)
    }

    fn credit_trade_fees_to_market_budgets_view(
        cfg: &WrapperConfigV16,
        group: &mut state::MarketViewMutV16<'_>,
        asset_index: usize,
        fee_long: u128,
        fee_short: u128,
    ) -> ProgramResult {
        credit_fee_to_domain_budget_view(cfg, group, asset_index * 2, fee_long)?;
        credit_fee_to_domain_budget_view(cfg, group, asset_index * 2 + 1, fee_short)
    }

    fn credit_market_fee_split_across_domains_view(
        cfg: &WrapperConfigV16,
        group: &mut state::MarketViewMutV16<'_>,
        asset_index: usize,
        amount: u128,
    ) -> ProgramResult {
        if amount == 0 {
            return Ok(());
        }
        let redirect = if asset_index == 0 {
            0
        } else {
            fee_share_floor(amount, cfg.fee_redirect_to_market_0_bps)?
        };
        let domain_amount = amount
            .checked_sub(redirect)
            .ok_or(PercolatorError::EngineCounterUnderflow)?;
        credit_market_insurance_budget_view(group, asset_index, domain_amount)?;
        credit_market_insurance_budget_view(group, 0, redirect)
    }

    fn clear_asset_domain_budget_counters_view(
        group: &mut state::MarketViewMutV16<'_>,
        asset_index: usize,
    ) -> ProgramResult {
        if asset_index >= group.markets.len() {
            return Err(PercolatorError::InvalidInstruction.into());
        }
        let slot = &mut group.markets[asset_index].engine;
        slot.insurance_domain_budget_long = percolator::V16PodU128::new(0);
        slot.insurance_domain_budget_short = percolator::V16PodU128::new(0);
        slot.insurance_domain_spent_long = percolator::V16PodU128::new(0);
        slot.insurance_domain_spent_short = percolator::V16PodU128::new(0);
        Ok(())
    }

    fn credit_maintenance_fee_to_active_market_budgets_view(
        cfg: &WrapperConfigV16,
        group: &mut state::MarketViewMutV16<'_>,
        amount: u128,
    ) -> ProgramResult {
        if amount == 0 {
            return Ok(());
        }
        let configured = group.header.config.max_market_slots.get() as usize;
        let mut active_count = 0usize;
        let mut i = 0usize;
        while i < configured {
            if i < group.markets.len()
                && group.markets[i].engine.asset.lifecycle == ASSET_LIFECYCLE_ACTIVE
            {
                active_count += 1;
            }
            i += 1;
        }
        if active_count == 0 {
            return credit_market_insurance_budget_view(group, 0, amount);
        }
        let base_share = amount / active_count as u128;
        let mut remainder = amount
            .checked_sub(
                base_share
                    .checked_mul(active_count as u128)
                    .ok_or(PercolatorError::EngineArithmeticOverflow)?,
            )
            .ok_or(PercolatorError::EngineCounterUnderflow)?;
        let mut i = 0usize;
        while i < configured {
            if i < group.markets.len()
                && group.markets[i].engine.asset.lifecycle == ASSET_LIFECYCLE_ACTIVE
            {
                let extra = if remainder != 0 {
                    remainder -= 1;
                    1
                } else {
                    0
                };
                credit_market_fee_split_across_domains_view(
                    cfg,
                    group,
                    i,
                    base_share
                        .checked_add(extra)
                        .ok_or(PercolatorError::EngineArithmeticOverflow)?,
                )?;
            }
            i += 1;
        }
        Ok(())
    }

    fn require_asset_active_for_oracle_reconfiguration_view(
        group: &state::MarketViewMutV16<'_>,
        asset_index: usize,
    ) -> ProgramResult {
        if asset_index >= group.markets.len()
            || group.markets[asset_index].engine.asset.lifecycle != 2
            || asset_local_has_position_or_loss_state_view(group, asset_index)
            || group.header.pnl_pos_tot.get() != 0
            || group.header.stale_certificate_count.get() != 0
            || group.header.b_stale_account_count.get() != 0
            || group.header.negative_pnl_account_count.get() != 0
            || group.header.bankruptcy_hlock_active != 0
            || group.header.threshold_stress_active != 0
            || group.header.loss_stale_active != 0
            || group
                .header
                .recovery_reason
                .try_to_runtime()
                .map_err(map_v16_error)?
                .is_some()
        {
            return Err(PercolatorError::EngineLockActive.into());
        }
        Ok(())
    }

    fn require_asset_mark_pushable_view(
        group: &state::MarketViewMutV16<'_>,
        asset_index: usize,
    ) -> ProgramResult {
        if asset_index >= group.markets.len() {
            return Err(PercolatorError::EngineLockActive.into());
        }
        match group.markets[asset_index].engine.asset.lifecycle {
            2 | 3 => Ok(()),
            _ => Err(PercolatorError::EngineLockActive.into()),
        }
    }

    pub fn process_instruction<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        instruction_data: &[u8],
    ) -> ProgramResult {
        match Instruction::decode(instruction_data)? {
            Instruction::InitMarket {
                max_portfolio_assets,
                h_min,
                h_max,
                initial_price,
                min_nonzero_mm_req,
                min_nonzero_im_req,
                maintenance_margin_bps,
                initial_margin_bps,
                max_trading_fee_bps,
                trade_fee_base_bps,
                liquidation_fee_bps,
                liquidation_fee_cap,
                min_liquidation_abs,
                max_price_move_bps_per_slot,
                max_accrual_dt_slots,
                max_abs_funding_e9_per_slot,
                min_funding_lifetime_slots,
                max_account_b_settlement_chunks,
                max_bankrupt_close_chunks,
                max_bankrupt_close_lifetime_slots,
                public_b_chunk_atoms,
                maintenance_fee_per_slot,
            } => handle_init_market(
                program_id,
                accounts,
                max_portfolio_assets,
                h_min,
                h_max,
                initial_price,
                min_nonzero_mm_req,
                min_nonzero_im_req,
                maintenance_margin_bps,
                initial_margin_bps,
                max_trading_fee_bps,
                trade_fee_base_bps,
                liquidation_fee_bps,
                liquidation_fee_cap,
                min_liquidation_abs,
                max_price_move_bps_per_slot,
                max_accrual_dt_slots,
                max_abs_funding_e9_per_slot,
                min_funding_lifetime_slots,
                max_account_b_settlement_chunks,
                max_bankrupt_close_chunks,
                max_bankrupt_close_lifetime_slots,
                public_b_chunk_atoms,
                maintenance_fee_per_slot,
            ),
            Instruction::InitPortfolio => handle_init_portfolio(program_id, accounts),
            Instruction::Deposit { amount } => handle_deposit(program_id, accounts, amount),
            Instruction::Withdraw { amount } => handle_withdraw(program_id, accounts, amount),
            Instruction::PermissionlessCrank {
                action,
                asset_index,
                now_slot,
                funding_rate_e9,
                close_q,
                fee_bps,
                recovery_reason,
            } => handle_permissionless_crank(
                program_id,
                accounts,
                action,
                asset_index,
                now_slot,
                funding_rate_e9,
                close_q,
                fee_bps,
                recovery_reason,
            ),
            Instruction::TradeNoCpi {
                asset_index,
                size_q,
                exec_price,
                fee_bps,
            } => handle_trade_nocpi(
                program_id,
                accounts,
                asset_index,
                size_q,
                exec_price,
                fee_bps,
            ),
            Instruction::TradeCpi {
                asset_index,
                size_q,
                fee_bps,
                limit_price,
            } => handle_trade_cpi(
                program_id,
                accounts,
                asset_index,
                size_q,
                fee_bps,
                limit_price,
            ),
            Instruction::ClosePortfolio => handle_close_portfolio(program_id, accounts),
            Instruction::TopUpInsurance { amount } => {
                handle_top_up_insurance(program_id, accounts, amount)
            }
            Instruction::TopUpInsuranceDomain { domain, amount } => {
                handle_top_up_insurance_domain(program_id, accounts, domain, amount)
            }
            Instruction::CloseSlab => handle_close_slab(program_id, accounts),
            Instruction::ResolveMarket => handle_resolve_market(program_id, accounts),
            Instruction::WithdrawInsuranceLimited { amount } => {
                handle_withdraw_insurance_limited(program_id, accounts, amount)
            }
            Instruction::TopUpBackingBucket {
                domain,
                amount,
                expiry_slot,
            } => handle_top_up_backing_bucket(program_id, accounts, domain, amount, expiry_slot),
            Instruction::WithdrawBackingBucket { domain, amount } => {
                handle_withdraw_backing_bucket(program_id, accounts, domain, amount)
            }
            Instruction::ConvertReleasedPnl { amount } => {
                handle_convert_released_pnl(program_id, accounts, amount)
            }
            Instruction::CloseResolved { fee_rate_per_slot } => {
                handle_close_resolved(program_id, accounts, fee_rate_per_slot)
            }
            Instruction::UpdateAuthority { kind, new_pubkey } => {
                handle_update_authority(program_id, accounts, kind, new_pubkey)
            }
            Instruction::UpdateInsurancePolicy {
                max_bps,
                deposits_only,
                cooldown_slots,
            } => handle_update_insurance_policy(
                program_id,
                accounts,
                max_bps,
                deposits_only,
                cooldown_slots,
            ),
            Instruction::UpdateLiquidationFeePolicy { cranker_share_bps } => {
                handle_update_liquidation_fee_policy(program_id, accounts, cranker_share_bps)
            }
            Instruction::UpdateMaintenanceFeePolicy { cranker_share_bps } => {
                handle_update_maintenance_fee_policy(program_id, accounts, cranker_share_bps)
            }
            Instruction::UpdateBackingFeePolicy {
                domain,
                fee_bps,
                insurance_share_bps,
            } => handle_update_backing_fee_policy(
                program_id,
                accounts,
                domain,
                fee_bps,
                insurance_share_bps,
            ),
            Instruction::UpdateTradeFeePolicy { trade_fee_base_bps } => {
                handle_update_trade_fee_policy(program_id, accounts, trade_fee_base_bps)
            }
            Instruction::UpdateFeeRedirectPolicy { redirect_bps } => {
                handle_update_fee_redirect_policy(program_id, accounts, redirect_bps)
            }
            Instruction::UpdateMarketInitFeePolicy { min_init_fee } => {
                handle_update_market_init_fee_policy(program_id, accounts, min_init_fee)
            }
            Instruction::WithdrawBackingBucketEarnings { domain, amount } => {
                handle_withdraw_backing_bucket_earnings(program_id, accounts, domain, amount)
            }
            Instruction::SyncBackingDomainLedger { domain } => {
                handle_sync_backing_domain_ledger(program_id, accounts, domain)
            }
            Instruction::SyncInsuranceLedger => handle_sync_insurance_ledger(program_id, accounts),
            Instruction::ConfigurePermissionlessResolve {
                stale_slots,
                force_close_delay_slots,
            } => handle_configure_permissionless_resolve(
                program_id,
                accounts,
                stale_slots,
                force_close_delay_slots,
            ),
            Instruction::ResolveStalePermissionless { now_slot } => {
                handle_resolve_stale_permissionless(program_id, accounts, now_slot)
            }
            Instruction::ConfigureHybridOracle {
                asset_index,
                now_slot,
                now_unix_ts,
                oracle_leg_count,
                oracle_leg_flags,
                max_staleness_secs,
                hybrid_soft_stale_slots,
                mark_ewma_halflife_slots,
                mark_min_fee,
                invert,
                unit_scale,
                conf_filter_bps,
                oracle_leg_feeds,
            } => handle_configure_hybrid_oracle(
                program_id,
                accounts,
                asset_index,
                now_slot,
                now_unix_ts,
                oracle_leg_count,
                oracle_leg_flags,
                max_staleness_secs,
                hybrid_soft_stale_slots,
                mark_ewma_halflife_slots,
                mark_min_fee,
                invert,
                unit_scale,
                conf_filter_bps,
                oracle_leg_feeds,
            ),
            Instruction::ConfigureEwmaMark {
                asset_index,
                now_slot,
                initial_mark_e6,
                mark_ewma_halflife_slots,
                mark_min_fee,
            } => handle_configure_ewma_mark(
                program_id,
                accounts,
                asset_index,
                now_slot,
                initial_mark_e6,
                mark_ewma_halflife_slots,
                mark_min_fee,
            ),
            Instruction::PushEwmaMark {
                asset_index,
                now_slot,
                mark_e6,
            } => handle_push_ewma_mark(program_id, accounts, asset_index, now_slot, mark_e6),
            Instruction::ConfigureAuthMark {
                asset_index,
                now_slot,
                initial_mark_e6,
            } => handle_configure_auth_mark(
                program_id,
                accounts,
                asset_index,
                now_slot,
                initial_mark_e6,
            ),
            Instruction::PushAuthMark {
                asset_index,
                now_slot,
                mark_e6,
            } => handle_push_auth_mark(program_id, accounts, asset_index, now_slot, mark_e6),
            Instruction::ForceCloseAbandonedAsset {
                asset_index,
                now_slot,
                close_q,
            } => handle_force_close_abandoned_asset(
                program_id,
                accounts,
                asset_index,
                now_slot,
                close_q,
            ),
            Instruction::UpdateAssetLifecycle {
                action,
                asset_index,
                now_slot,
                initial_price,
                insurance_authority,
                insurance_operator,
                backing_bucket_authority,
                oracle_authority,
            } => handle_update_asset_lifecycle(
                program_id,
                accounts,
                action,
                asset_index,
                now_slot,
                initial_price,
                insurance_authority,
                insurance_operator,
                backing_bucket_authority,
                oracle_authority,
            ),
            Instruction::WithdrawInsurance { amount } => {
                handle_withdraw_insurance(program_id, accounts, amount)
            }
            Instruction::WithdrawInsuranceDomain { domain, amount } => {
                handle_withdraw_insurance_domain(program_id, accounts, domain, amount)
            }
            Instruction::CureAndCancelClose { optional_deposit } => {
                handle_cure_and_cancel_close(program_id, accounts, optional_deposit)
            }
            Instruction::ForfeitRecoveryLeg {
                asset_index,
                b_delta_budget,
            } => handle_forfeit_recovery_leg(program_id, accounts, asset_index, b_delta_budget),
            Instruction::RebalanceReduce {
                asset_index,
                reduce_q,
            } => handle_rebalance_reduce(program_id, accounts, asset_index, reduce_q),
            Instruction::FinalizeResetSide { asset_index, side } => {
                handle_finalize_reset_side(program_id, accounts, asset_index, side)
            }
            Instruction::ClaimResolvedPayoutTopup => {
                handle_claim_resolved_payout_topup(program_id, accounts)
            }
            Instruction::RefineResolvedUnreceiptedBound { decrease_num } => {
                handle_refine_resolved_unreceipted_bound(program_id, accounts, decrease_num)
            }
            Instruction::SyncMaintenanceFee { now_slot } => {
                handle_sync_maintenance_fee(program_id, accounts, now_slot)
            }
            Instruction::UpdateBaseUnitMints {
                primary_mint,
                secondary_mint,
            } => handle_update_base_unit_mints(program_id, accounts, primary_mint, secondary_mint),
            Instruction::SwapSecondaryForPrimary { amount } => {
                handle_swap_secondary_for_primary(program_id, accounts, amount)
            }
            Instruction::CreateLpVault {
                fee_share_bps,
                redemption_cooldown_slots,
                oi_reservation_threshold_bps,
                domain,
            } => handle_create_lp_vault(
                program_id,
                accounts,
                fee_share_bps,
                redemption_cooldown_slots,
                oi_reservation_threshold_bps,
                domain,
            ),
            Instruction::DepositToLpVault { amount } => {
                handle_deposit_to_lp_vault(program_id, accounts, amount)
            }
            Instruction::RequestRedeemLpShares { lp_amount } => {
                handle_request_redeem_lp_shares(program_id, accounts, lp_amount)
            }
            Instruction::ExecuteRedemption => handle_execute_redemption(program_id, accounts),
            Instruction::LpVaultCrankFees => handle_lp_vault_crank_fees(program_id, accounts),
            Instruction::SetLpVaultPaused { paused } => {
                handle_set_lp_vault_paused(program_id, accounts, paused)
            }
            Instruction::CloseLpVault => handle_close_lp_vault(program_id, accounts),
            Instruction::TransferPortfolioOwnership { new_owner, asset_index } => {
                handle_transfer_portfolio_ownership(program_id, accounts, new_owner, asset_index)
            }
            Instruction::SetNftProgramId { nft_program_id } => {
                handle_set_nft_program_id(program_id, accounts, nft_program_id)
            }
        }
    }

    /// LP Vault — CreateLpVault (tag 65). Phase 2.B Tier 3 Workstream 4B / Phase C.
    ///
    /// Admin-gated. Creates the per-market LpVaultRegistry PDA (program-owned)
    /// and the LP-share SPL mint PDA (authority = registry PDA, supply 0,
    /// decimals 0 — shares are integer collateral atoms, 1:1 on the fresh
    /// epoch). The registry is NOT yet the backing-bucket authority: the
    /// operator runs UpdateAssetLifecycle as a separate step (lp_vault_design.md
    /// §6 two-step create; deposits fail closed with LpVaultAuthorityMismatch
    /// until then).
    ///
    /// Conservation: structurally pure-meta — creates accounts (rent lamports
    /// only, via System) and inits a mint at supply 0. No collateral moves, no
    /// LP shares minted, no backing/source/lien/reservation state touched. All
    /// 4 conservation-proof obligations are N/A (design §8 CreateLpVault row).
    /// No Kani harness (no share-math executed; account-init only).
    #[inline(never)]
    fn handle_create_lp_vault<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        fee_share_bps: u16,
        redemption_cooldown_slots: u64,
        oi_reservation_threshold_bps: u16,
        domain: u16,
    ) -> ProgramResult {
        let admin = account(accounts, 0)?;
        let market_ai = account(accounts, 1)?;
        let registry_ai = account(accounts, 2)?;
        let mint_ai = account(accounts, 3)?;
        let system_program_ai = account(accounts, 4)?;
        let token_program = account(accounts, 5)?;

        expect_signer(admin)?;
        expect_writable(admin)?;
        expect_writable(registry_ai)?;
        expect_writable(mint_ai)?;
        expect_owner(market_ai, program_id)?;
        verify_token_program(token_program)?;
        if system_program_ai.key != &system_program::ID {
            return Err(PercolatorError::InvalidInstruction.into());
        }
        if fee_share_bps > 10_000 || oi_reservation_threshold_bps > 10_000 {
            return Err(PercolatorError::InvalidInstruction.into());
        }

        // admin authorization + domain bound, read from the market.
        let (cfg, mode, configured_slots, _) =
            state::read_market_config_mode_and_capacity(&market_ai.try_borrow_data()?)?;
        if mode != MarketModeV16::Live {
            return Err(PercolatorError::EngineLockActive.into());
        }
        if admin.key.to_bytes() != cfg.admin {
            return Err(PercolatorError::Unauthorized.into());
        }
        if (domain as usize) >= configured_slots.saturating_mul(2) {
            return Err(PercolatorError::InvalidInstruction.into());
        }

        // Derive + bind the two PDAs.
        let (registry_pda, registry_bump) =
            state::derive_lp_vault_registry(program_id, market_ai.key);
        expect_key(registry_ai, &registry_pda)?;
        let (mint_pda, mint_bump) = state::derive_lp_vault_mint(program_id, market_ai.key);
        expect_key(mint_ai, &mint_pda)?;

        // Both PDAs must be fresh (system-owned, no data) — fail closed otherwise.
        if registry_ai.owner != &system_program::ID
            || mint_ai.owner != &system_program::ID
            || !registry_ai.data_is_empty()
            || !mint_ai.data_is_empty()
        {
            return Err(PercolatorError::AlreadyInitialized.into());
        }

        let rent = Rent::get()?;
        let market_bytes = market_ai.key.to_bytes();

        // Create the program-owned registry PDA account.
        let registry_len = state::lp_vault_registry_account_len();
        invoke_signed(
            &system_instruction::create_account(
                admin.key,
                registry_ai.key,
                rent.minimum_balance(registry_len),
                registry_len as u64,
                program_id,
            ),
            &[admin.clone(), registry_ai.clone(), system_program_ai.clone()],
            &[&[
                crate::constants::LP_VAULT_REGISTRY_SEED,
                market_bytes.as_ref(),
                &[registry_bump],
            ]],
        )?;

        // Create the spl_token-owned LP share mint PDA account.
        let mint_len = spl_token::state::Mint::LEN;
        invoke_signed(
            &system_instruction::create_account(
                admin.key,
                mint_ai.key,
                rent.minimum_balance(mint_len),
                mint_len as u64,
                token_program.key,
            ),
            &[admin.clone(), mint_ai.clone(), system_program_ai.clone()],
            &[&[
                crate::constants::LP_VAULT_MINT_SEED,
                market_bytes.as_ref(),
                &[mint_bump],
            ]],
        )?;

        // Initialize the LP share mint: authority = registry PDA, no freeze.
        let init_mint_ix = spl_token::instruction::initialize_mint2(
            token_program.key,
            mint_ai.key,
            &registry_pda,
            None,
            0,
        )?;
        invoke(&init_mint_ix, &[mint_ai.clone(), token_program.clone()])?;

        // Persist the registry config.
        let registry = state::LpVaultRegistryV16 {
            market_group: market_bytes,
            lp_mint: mint_ai.key.to_bytes(),
            total_lp_shares_outstanding: 0,
            insurance_fee_snapshot_atoms: 0,
            fee_distribution_total_atoms: 0,
            epoch: 0,
            redemption_cooldown_slots,
            fee_share_bps,
            oi_reservation_threshold_bps,
            domain,
            paused: 0,
            version: crate::constants::LP_VAULT_VERSION,
            bump: registry_bump,
            mint_bump,
            _padding: [0u8; 6],
            _reserved: [0u8; 16],
        };
        state::init_lp_vault_registry(&mut registry_ai.try_borrow_mut_data()?, &registry)?;
        Ok(())
    }

    /// LP Vault — DepositToLpVault (tag 66). Phase 2.B Tier 3 Workstream 4B / Phase D.
    ///
    /// Permissionless deposit: moves the depositor's collateral into the LP
    /// Vault's backing bucket and mints pro-rata LP shares.
    ///
    /// Sequence (lp_vault_design.md §5.5): read NAV (Note 2 — strictly from the
    /// BackingDomainLedger counters via `lp_vault_nav_atoms`, never a token
    /// balance) → compute shares (`lp_shares_for_deposit`, round DOWN), reject 0
    /// (Note 1 — `LpVaultZeroSharesMinted`, never silently absorb) → transfer
    /// depositor→vault (depositor signs as token authority — owns the funds) →
    /// update backing + ledger via the SAME helpers `handle_top_up_backing_bucket`
    /// uses (Option A: no duplication; backing-authority asserted as the registry
    /// PDA in-program rather than carried by the signer) → mint shares (registry
    /// PDA signs as mint authority) → bump `total_lp_shares_outstanding`.
    ///
    /// AUDIT MIRROR: the backing-bucket + BackingDomainLedger update sequence
    /// below is mirrored from `handle_top_up_backing_bucket` — change BOTH
    /// together. The differential test `lp_deposit_backing_state_matches_top_up`
    /// fails mechanically if the two sequences ever drift.
    ///
    /// Conservation (design §8 DepositToLpVault row): TokenValueFlowProofV16
    /// (depositor→vault, balanced), StockReconciliationProofV16 (LP supply++ vs
    /// vault principal++), ReservationEncumbranceProofV16 +
    /// SourceCreditLienAggregateProofV16 (backing reservation grows) — all
    /// produced by the engine's `add_fresh_counterparty_backing_*` /
    /// `validate_shape` path (which internally validates the reservation
    /// encumbrance + source-credit lien proofs) plus the explicit token-flow
    /// (transfer == amount) and stock reconciliation (shares minted == math).
    #[inline(never)]
    fn handle_deposit_to_lp_vault<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        amount: u128,
    ) -> ProgramResult {
        let depositor = account(accounts, 0)?;
        let market_ai = account(accounts, 1)?;
        let registry_ai = account(accounts, 2)?;
        let mint_ai = account(accounts, 3)?;
        let depositor_lp_ata = account(accounts, 4)?;
        let source_token = account(accounts, 5)?;
        let vault_token = account(accounts, 6)?;
        let ledger_ai = account(accounts, 7)?;
        let token_program = account(accounts, 8)?;
        let system_program_ai = account(accounts, 9)?;

        expect_signer(depositor)?;
        expect_writable(depositor)?;
        expect_writable(market_ai)?;
        expect_writable(registry_ai)?;
        expect_writable(mint_ai)?;
        expect_writable(depositor_lp_ata)?;
        expect_writable(source_token)?;
        expect_writable(vault_token)?;
        expect_writable(ledger_ai)?;
        expect_owner(market_ai, program_id)?;
        expect_owner(registry_ai, program_id)?;
        verify_token_program(token_program)?;
        if system_program_ai.key != &system_program::ID {
            return Err(PercolatorError::InvalidInstruction.into());
        }
        if amount == 0 {
            return Err(PercolatorError::LpVaultZeroAmount.into());
        }

        // Registry shape + PDA binding.
        let registry = state::read_lp_vault_registry(&registry_ai.try_borrow_data()?)?;
        if registry.paused != 0 {
            return Err(PercolatorError::LpVaultPaused.into());
        }
        let (registry_pda, registry_bump) =
            state::derive_lp_vault_registry(program_id, market_ai.key);
        expect_key(registry_ai, &registry_pda)?;
        if registry.market_group != market_ai.key.to_bytes()
            || mint_ai.key.to_bytes() != registry.lp_mint
        {
            return Err(PercolatorError::LpVaultNotFound.into());
        }
        let domain = registry.domain as usize;

        // Market preflight: Live + the registry PDA is the backing-bucket
        // authority for this domain (Note 5 fail-closed — until the operator's
        // UpdateAssetLifecycle step, deposits reject with LpVaultAuthorityMismatch).
        let (cfg, mode, configured_slots, _) =
            state::read_market_config_mode_and_capacity(&market_ai.try_borrow_data()?)?;
        if mode != MarketModeV16::Live {
            return Err(PercolatorError::EngineLockActive.into());
        }
        let asset_index = domain / 2;
        if domain >= configured_slots.saturating_mul(2) || asset_index >= configured_slots {
            return Err(PercolatorError::InvalidInstruction.into());
        }
        {
            let market_data = market_ai.try_borrow_data()?;
            let profile = read_oracle_profile_for_asset(&market_data, &cfg, asset_index)?;
            let authorities = domain_authorities_from_profile(&cfg, &profile, asset_index);
            if authorities.backing_bucket_authority != registry_pda.to_bytes() {
                return Err(PercolatorError::LpVaultAuthorityMismatch.into());
            }
        }

        // Token-account checks: depositor owns source; vault is the program vault;
        // LP ATA belongs to depositor for the LP mint.
        let mint = primary_collateral_mint(&cfg);
        let (vault_authority, _) = derive_vault_authority(program_id, market_ai.key);
        verify_user_token_account(source_token, depositor.key, &mint)?;
        verify_vault_token_account(vault_token, &vault_authority, &mint)?;
        verify_user_token_account(depositor_lp_ata, depositor.key, mint_ai.key)?;
        let amount_u64 = amount_to_u64(amount)?;
        require_token_balance(source_token, amount_u64)?;

        // Backing-domain ledger PDA: lazily create (program-owned) on first deposit.
        let (ledger_pda, ledger_bump) =
            state::derive_lp_backing_ledger(program_id, market_ai.key, registry.domain);
        expect_key(ledger_ai, &ledger_pda)?;
        if ledger_ai.data_is_empty() {
            let rent = Rent::get()?;
            let len = state::backing_domain_ledger_account_len();
            invoke_signed(
                &system_instruction::create_account(
                    depositor.key,
                    ledger_ai.key,
                    rent.minimum_balance(len),
                    len as u64,
                    program_id,
                ),
                &[depositor.clone(), ledger_ai.clone(), system_program_ai.clone()],
                &[&[
                    crate::constants::LP_BACKING_LEDGER_SEED,
                    market_ai.key.as_ref(),
                    &registry.domain.to_le_bytes(),
                    &[ledger_bump],
                ]],
            )?;
        }

        let backing_num = amount
            .checked_mul(BOUND_SCALE)
            .ok_or(PercolatorError::EngineArithmeticOverflow)?;

        // ── Phase 1: NAV (pre-deposit) + shares, no mutation, no token move. ──
        let shares = {
            let mut market_data = market_ai.try_borrow_mut_data()?;
            let (_, group) = state::market_view_mut(&mut market_data)?;
            let (_, bucket) = backing_domain_parts_view(&group, domain)?;
            let ledger_data = ledger_ai.try_borrow_data()?;
            let (mut ledger, _) = read_or_new_backing_domain_ledger(
                &ledger_data,
                market_ai.key.to_bytes(),
                registry_pda.to_bytes(),
                registry.domain,
                &bucket,
            )?;
            sync_backing_domain_ledger(&mut ledger, &bucket)?;
            let nav = percolator::lp_vault::lp_vault_nav_atoms(
                ledger.total_principal_atoms,
                ledger.total_earnings_atoms,
                ledger.total_earnings_withdrawn_atoms,
                ledger.cumulative_loss_atoms,
                ledger.cumulative_recovery_atoms,
                registry.fee_share_bps,
            )
            .map_err(map_v16_error)?;
            percolator::lp_vault::lp_shares_for_deposit(
                amount,
                registry.total_lp_shares_outstanding,
                nav,
            )
            .map_err(map_v16_error)?
        };
        // Note 1: never silently mint 0 and absorb the deposit.
        if shares == 0 {
            return Err(PercolatorError::LpVaultZeroSharesMinted.into());
        }
        let shares_u64 = u64::try_from(shares).map_err(|_| PercolatorError::EngineArithmeticOverflow)?;

        // ── Phase 2: move depositor collateral into the backing vault. ──
        transfer_tokens(token_program, source_token, vault_token, depositor, amount_u64)?;

        // ── Phase 3: backing-bucket + ledger update — MIRRORS handle_top_up_backing_bucket. ──
        {
            let mut market_data = market_ai.try_borrow_mut_data()?;
            let (cfg_v, mut group) = state::market_view_mut(&mut market_data)?;
            if group.header.mode != 0 {
                return Err(PercolatorError::EngineLockActive.into());
            }
            reject_permissionless_resolve_matured_live_view(&cfg_v, &group)?;
            let mut ledger_data = ledger_ai.try_borrow_mut_data()?;
            let (_, bucket) = backing_domain_parts_view(&group, domain)?;
            let (mut ledger, initialized) = read_or_new_backing_domain_ledger(
                &ledger_data,
                market_ai.key.to_bytes(),
                registry_pda.to_bytes(),
                registry.domain,
                &bucket,
            )?;
            sync_backing_domain_ledger(&mut ledger, &bucket)?;
            let next_vault = group
                .header
                .vault
                .get()
                .checked_add(amount)
                .ok_or(PercolatorError::EngineArithmeticOverflow)?;
            add_fresh_counterparty_backing_view(
                &mut group,
                domain,
                backing_num,
                crate::constants::LP_VAULT_BACKING_EXPIRY_SLOT,
            )?;
            ledger.total_principal_atoms = ledger
                .total_principal_atoms
                .checked_add(amount)
                .ok_or(PercolatorError::EngineArithmeticOverflow)?;
            ledger.total_deposited_atoms = ledger
                .total_deposited_atoms
                .checked_add(amount)
                .ok_or(PercolatorError::EngineArithmeticOverflow)?;
            group.header.vault = percolator::V16PodU128::new(next_vault);
            group.validate_shape().map_err(map_v16_error)?;
            write_or_init_backing_domain_ledger(&mut ledger_data, &ledger, initialized)?;
        }

        // ── Phase 4: mint LP shares to the depositor (registry PDA signs). ──
        let mint_ix = spl_token::instruction::mint_to(
            token_program.key,
            mint_ai.key,
            depositor_lp_ata.key,
            &registry_pda,
            &[],
            shares_u64,
        )?;
        invoke_signed(
            &mint_ix,
            &[
                mint_ai.clone(),
                depositor_lp_ata.clone(),
                registry_ai.clone(),
                token_program.clone(),
            ],
            &[&[
                crate::constants::LP_VAULT_REGISTRY_SEED,
                market_ai.key.as_ref(),
                &[registry_bump],
            ]],
        )?;

        // ── Phase 5: bump outstanding shares (mirrors the SPL mint supply). ──
        {
            let mut reg = state::read_lp_vault_registry(&registry_ai.try_borrow_data()?)?;
            reg.total_lp_shares_outstanding = reg
                .total_lp_shares_outstanding
                .checked_add(shares)
                .ok_or(PercolatorError::EngineArithmeticOverflow)?;
            state::write_lp_vault_registry(&mut registry_ai.try_borrow_mut_data()?, &reg)?;
        }
        Ok(())
    }

    /// LP Vault — RequestRedeemLpShares (tag 67). Phase 2.B Tier 3 Workstream 4B / Phase E.
    ///
    /// Step 1 of the two-step redemption (B-7 cooldown). Escrows the redeemer's
    /// LP shares and records a redemption request; does NOT burn or pay out.
    /// Per sign-off: shares are TRANSFERRED to a registry-owned escrow ATA (not
    /// burned) so `total_lp_shares_outstanding == lp_mint.supply` (invariant I2)
    /// holds through the cooldown, and the redeemer cannot sell/move the shares
    /// while the request is pending. `total_lp_shares_outstanding` is UNCHANGED
    /// here — the shares still exist, just escrowed.
    ///
    /// Conservation: TokenValueFlowProofV16 is the SPL transfer
    /// (redeemer→escrow, balanced by SPL). No backing/source/lien/reservation
    /// state changes (those happen at ExecuteRedemption). No Kani (account-init
    /// + SPL transfer only).
    #[inline(never)]
    fn handle_request_redeem_lp_shares<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        lp_amount: u128,
    ) -> ProgramResult {
        let redeemer = account(accounts, 0)?;
        let registry_ai = account(accounts, 1)?;
        let lp_mint = account(accounts, 2)?;
        let redeemer_lp_ata = account(accounts, 3)?;
        let escrow_ai = account(accounts, 4)?;
        let redemption_ai = account(accounts, 5)?;
        let token_program = account(accounts, 6)?;
        let system_program_ai = account(accounts, 7)?;

        expect_signer(redeemer)?;
        expect_writable(redeemer)?;
        expect_writable(redeemer_lp_ata)?;
        expect_writable(escrow_ai)?;
        expect_writable(redemption_ai)?;
        expect_owner(registry_ai, program_id)?;
        verify_token_program(token_program)?;
        if system_program_ai.key != &system_program::ID {
            return Err(PercolatorError::InvalidInstruction.into());
        }
        if lp_amount == 0 {
            return Err(PercolatorError::LpVaultZeroAmount.into());
        }

        let registry = state::read_lp_vault_registry(&registry_ai.try_borrow_data()?)?;
        if registry.paused != 0 {
            return Err(PercolatorError::LpVaultPaused.into());
        }
        let market_key = Pubkey::new_from_array(registry.market_group);
        let (registry_pda, _) = state::derive_lp_vault_registry(program_id, &market_key);
        expect_key(registry_ai, &registry_pda)?;
        if lp_mint.key.to_bytes() != registry.lp_mint {
            return Err(PercolatorError::InvalidMint.into());
        }
        verify_user_token_account(redeemer_lp_ata, redeemer.key, lp_mint.key)?;
        let lp_amount_u64 =
            u64::try_from(lp_amount).map_err(|_| PercolatorError::EngineArithmeticOverflow)?;
        require_token_balance(redeemer_lp_ata, lp_amount_u64)?;

        // Escrow ATA (registry-owned, shared per vault): lazily created.
        let (escrow_pda, escrow_bump) = state::derive_lp_escrow(program_id, &market_key);
        expect_key(escrow_ai, &escrow_pda)?;
        if escrow_ai.data_is_empty() {
            let rent = Rent::get()?;
            let len = spl_token::state::Account::LEN;
            invoke_signed(
                &system_instruction::create_account(
                    redeemer.key,
                    escrow_ai.key,
                    rent.minimum_balance(len),
                    len as u64,
                    token_program.key,
                ),
                &[redeemer.clone(), escrow_ai.clone(), system_program_ai.clone()],
                &[&[
                    crate::constants::LP_ESCROW_SEED,
                    market_key.as_ref(),
                    &[escrow_bump],
                ]],
            )?;
            let init_ix = spl_token::instruction::initialize_account3(
                token_program.key,
                escrow_ai.key,
                lp_mint.key,
                &registry_pda,
            )?;
            invoke(&init_ix, &[escrow_ai.clone(), lp_mint.clone(), token_program.clone()])?;
        }

        // Redemption PDA: one pending request per (registry, redeemer). Fails
        // closed if a request already exists (must execute/cancel first).
        let (redemption_pda, redemption_bump) =
            state::derive_lp_redemption(program_id, &registry_pda, redeemer.key);
        expect_key(redemption_ai, &redemption_pda)?;
        if !redemption_ai.data_is_empty() {
            return Err(PercolatorError::AlreadyInitialized.into());
        }
        {
            let rent = Rent::get()?;
            let rlen = state::lp_redemption_account_len();
            invoke_signed(
                &system_instruction::create_account(
                    redeemer.key,
                    redemption_ai.key,
                    rent.minimum_balance(rlen),
                    rlen as u64,
                    program_id,
                ),
                &[redeemer.clone(), redemption_ai.clone(), system_program_ai.clone()],
                &[&[
                    crate::constants::LP_REDEMPTION_SEED,
                    registry_pda.as_ref(),
                    redeemer.key.as_ref(),
                    &[redemption_bump],
                ]],
            )?;
        }

        // Escrow the shares (redeemer signs — owns the source).
        transfer_tokens(token_program, redeemer_lp_ata, escrow_ai, redeemer, lp_amount_u64)?;

        // Record the request. total_lp_shares_outstanding UNCHANGED (I2 holds).
        let now_slot = Clock::get().map(|c| c.slot).unwrap_or(0);
        let redemption = state::LpRedemptionV16 {
            registry: registry_pda.to_bytes(),
            redeemer: redeemer.key.to_bytes(),
            shares: lp_amount,
            request_slot: now_slot,
            version: crate::constants::LP_VAULT_VERSION,
            bump: redemption_bump,
            _padding: [0u8; 6],
        };
        state::init_lp_redemption(&mut redemption_ai.try_borrow_mut_data()?, &redemption)?;
        Ok(())
    }

    /// LP Vault — ExecuteRedemption (tag 68). Phase 2.B Tier 3 Workstream 4B / Phase E.
    ///
    /// Step 2: after cooldown, pays the redeemer their execute-time pro-rata
    /// share of the vault and burns the escrowed shares. Permissionless.
    ///
    /// DOUBLE-EXECUTE REPLAY GUARD (headline): the FIRST thing this does is
    /// `read_lp_redemption`, which fails `NotInitialized` if the PDA's magic was
    /// zeroed by a prior consume. The LAST thing it does is
    /// `consume_lp_redemption` (zero the magic) — keyed on DATA, not lamports,
    /// so even a SECOND ExecuteRedemption in the SAME transaction (where lamports
    /// could be re-funded before end-of-tx GC) rejects on the zeroed magic.
    ///
    /// Sequence (lp_vault_design.md §5.6): read PDA (replay guard) → cooldown
    /// (I5) → NAV from ledger counters (Note 2) → round-DOWN
    /// `lp_atoms_for_redemption` → OI guard (I6) → inline withdraw mirroring
    /// `handle_withdraw_backing_bucket` (bucket/source/ledger decrements +
    /// `vault_authority`-signed transfer to the redeemer directly) → burn shares
    /// from escrow (registry PDA signs) → `total_lp_shares_outstanding -= shares`
    /// → consume PDA.
    ///
    /// AUDIT MIRROR: the backing decrement sequence is mirrored from
    /// `handle_withdraw_backing_bucket` — change BOTH together. The differential
    /// test `lp_execute_redemption_backing_state_matches_withdraw` fails if they
    /// drift.
    ///
    /// Conservation (mirror of deposit, shrinking the reservation):
    /// TokenValueFlowProofV16 (vault→redeemer), StockReconciliationProofV16 (LP
    /// supply-- via escrow burn vs vault principal--), ReservationEncumbranceProofV16
    /// + SourceCreditLienAggregateProofV16 (backing reservation shrinks) —
    /// produced by the inlined decrement + validate_shape.
    #[inline(never)]
    fn handle_execute_redemption<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
    ) -> ProgramResult {
        let cranker = account(accounts, 0)?;
        let market_ai = account(accounts, 1)?;
        let registry_ai = account(accounts, 2)?;
        let redemption_ai = account(accounts, 3)?;
        let lp_mint = account(accounts, 4)?;
        let escrow_ai = account(accounts, 5)?;
        let vault_token = account(accounts, 6)?;
        let vault_authority_ai = account(accounts, 7)?;
        let ledger_ai = account(accounts, 8)?;
        let redeemer_dest = account(accounts, 9)?;
        let token_program = account(accounts, 10)?;

        expect_signer(cranker)?;
        expect_writable(market_ai)?;
        expect_writable(registry_ai)?;
        expect_writable(redemption_ai)?;
        expect_writable(lp_mint)?;
        expect_writable(escrow_ai)?;
        expect_writable(vault_token)?;
        expect_writable(ledger_ai)?;
        expect_writable(redeemer_dest)?;
        expect_owner(market_ai, program_id)?;
        expect_owner(registry_ai, program_id)?;
        expect_owner(redemption_ai, program_id)?;
        expect_owner(ledger_ai, program_id)?;
        verify_token_program(token_program)?;

        // ── REPLAY GUARD: rejects NotInitialized if the magic was zeroed. ──
        let redemption = state::read_lp_redemption(&redemption_ai.try_borrow_data()?)?;
        let registry = state::read_lp_vault_registry(&registry_ai.try_borrow_data()?)?;

        // Bindings.
        let market_key = Pubkey::new_from_array(registry.market_group);
        expect_key(market_ai, &market_key)?;
        let (registry_pda, registry_bump) = state::derive_lp_vault_registry(program_id, &market_key);
        expect_key(registry_ai, &registry_pda)?;
        if redemption.registry != registry_pda.to_bytes() {
            return Err(PercolatorError::LpVaultNotFound.into());
        }
        let redeemer = Pubkey::new_from_array(redemption.redeemer);
        let (redemption_pda, _) = state::derive_lp_redemption(program_id, &registry_pda, &redeemer);
        expect_key(redemption_ai, &redemption_pda)?;
        if lp_mint.key.to_bytes() != registry.lp_mint {
            return Err(PercolatorError::InvalidMint.into());
        }
        let (escrow_pda, _) = state::derive_lp_escrow(program_id, &market_key);
        expect_key(escrow_ai, &escrow_pda)?;

        // ── Cooldown gate (I5). ──
        let now_slot = Clock::get().map(|c| c.slot).unwrap_or(0);
        if !percolator::lp_vault::lp_redemption_cooldown_elapsed(
            redemption.request_slot,
            now_slot,
            registry.redemption_cooldown_slots,
        ) {
            return Err(PercolatorError::LpVaultCooldownActive.into());
        }

        let domain = registry.domain as usize;
        let asset_index = domain / 2;

        // Collateral mint + vault authority + redeemer dest checks.
        let (cfg, mode, configured_slots, _) =
            state::read_market_config_mode_and_capacity(&market_ai.try_borrow_data()?)?;
        if mode != MarketModeV16::Live {
            return Err(PercolatorError::EngineLockActive.into());
        }
        if domain >= configured_slots.saturating_mul(2) || asset_index >= configured_slots {
            return Err(PercolatorError::InvalidInstruction.into());
        }
        let mint = primary_collateral_mint(&cfg);
        let (vault_authority, vault_bump) = derive_vault_authority(program_id, market_ai.key);
        expect_key(vault_authority_ai, &vault_authority)?;
        verify_user_token_account(redeemer_dest, &redeemer, &mint)?;
        verify_vault_token_account(vault_token, &vault_authority, &mint)?;

        // ── NAV (pre-withdraw) → atoms (round DOWN, Note 2). ──
        let atoms = {
            let mut market_data = market_ai.try_borrow_mut_data()?;
            let (_, group) = state::market_view_mut(&mut market_data)?;
            let (_, bucket) = backing_domain_parts_view(&group, domain)?;
            let ledger_data = ledger_ai.try_borrow_data()?;
            let (mut ledger, _) = read_or_new_backing_domain_ledger(
                &ledger_data,
                market_ai.key.to_bytes(),
                registry_pda.to_bytes(),
                registry.domain,
                &bucket,
            )?;
            sync_backing_domain_ledger(&mut ledger, &bucket)?;
            let nav = percolator::lp_vault::lp_vault_nav_atoms(
                ledger.total_principal_atoms,
                ledger.total_earnings_atoms,
                ledger.total_earnings_withdrawn_atoms,
                ledger.cumulative_loss_atoms,
                ledger.cumulative_recovery_atoms,
                registry.fee_share_bps,
            )
            .map_err(map_v16_error)?;
            percolator::lp_vault::lp_atoms_for_redemption(
                redemption.shares,
                registry.total_lp_shares_outstanding,
                nav,
            )
            .map_err(map_v16_error)?
        };
        // Dust-share redemption rounding to 0 atoms would burn shares for no
        // payout — reject (recoverable via cancel in Phase H).
        if atoms == 0 {
            return Err(PercolatorError::LpVaultZeroAmount.into());
        }
        let atoms_u64 = amount_to_u64(atoms)?;
        let backing_num = atoms
            .checked_mul(BOUND_SCALE)
            .ok_or(PercolatorError::EngineArithmeticOverflow)?;

        // ── Inline withdraw — MIRRORS handle_withdraw_backing_bucket. ──
        {
            let mut market_data = market_ai.try_borrow_mut_data()?;
            let (cfg_v, group) = state::market_view_mut(&mut market_data)?;
            if group.header.mode != 0 {
                return Err(PercolatorError::EngineLockActive.into());
            }
            // Registry must be the backing authority for this domain (Note 5).
            let authorities = domain_authorities_from_view(&group, &cfg_v, domain)?;
            if authorities.backing_bucket_authority != registry_pda.to_bytes() {
                return Err(PercolatorError::LpVaultAuthorityMismatch.into());
            }
            if asset_index >= group.markets.len()
                || asset_index >= group.header.config.max_market_slots.get() as usize
            {
                return Err(PercolatorError::InvalidInstruction.into());
            }
            let (source_acc, bucket_acc) = if domain % 2 == 0 {
                (
                    &mut group.markets[asset_index].engine.source_credit_long,
                    &mut group.markets[asset_index].engine.backing_long,
                )
            } else {
                (
                    &mut group.markets[asset_index].engine.source_credit_short,
                    &mut group.markets[asset_index].engine.backing_short,
                )
            };
            let mut source = source_acc.try_to_runtime().map_err(map_v16_error)?;
            let mut bucket = bucket_acc.try_to_runtime().map_err(map_v16_error)?;
            let mut ledger_data = ledger_ai.try_borrow_mut_data()?;
            let (mut ledger, initialized) = read_or_new_backing_domain_ledger(
                &ledger_data,
                market_ai.key.to_bytes(),
                registry_pda.to_bytes(),
                registry.domain,
                &bucket,
            )?;
            sync_backing_domain_ledger(&mut ledger, &bucket)?;
            if atoms > ledger.total_principal_atoms {
                return Err(PercolatorError::EngineCounterUnderflow.into());
            }
            // Same withdrawability gate as handle_withdraw_backing_bucket.
            if source.positive_claim_bound_num != 0
                || source.exact_positive_claim_num != 0
                || bucket.status != BackingBucketStatusV16::Fresh
                || bucket.fresh_unliened_backing_num < backing_num
                || source.fresh_reserved_backing_num < backing_num
                || atoms > group.header.vault.get()
            {
                return Err(PercolatorError::EngineLockActive.into());
            }
            // OI reservation guard (I6): leave nav_post * threshold/10_000 of
            // outstanding backing covered. nav_post == ledger principal after
            // this withdraw (no earnings change here).
            if registry.oi_reservation_threshold_bps != 0 {
                let outstanding_post = bucket
                    .fresh_unliened_backing_num
                    .checked_sub(backing_num)
                    .ok_or(PercolatorError::EngineCounterUnderflow)?
                    .checked_add(bucket.valid_liened_backing_num)
                    .ok_or(PercolatorError::EngineArithmeticOverflow)?;
                let nav_post = ledger
                    .total_principal_atoms
                    .checked_sub(atoms)
                    .ok_or(PercolatorError::EngineCounterUnderflow)?
                    .checked_mul(BOUND_SCALE)
                    .ok_or(PercolatorError::EngineArithmeticOverflow)?;
                let covered = nav_post
                    .checked_mul(registry.oi_reservation_threshold_bps as u128)
                    .ok_or(PercolatorError::EngineArithmeticOverflow)?
                    / 10_000u128;
                if covered < outstanding_post {
                    return Err(PercolatorError::LpVaultOiReservationViolated.into());
                }
            }
            bucket.fresh_unliened_backing_num = bucket
                .fresh_unliened_backing_num
                .checked_sub(backing_num)
                .ok_or(PercolatorError::EngineCounterUnderflow)?;
            if bucket.fresh_unliened_backing_num == 0 && bucket.valid_liened_backing_num == 0 {
                if bucket.impaired_liened_backing_num != 0 {
                    bucket.status = BackingBucketStatusV16::Impaired;
                } else if bucket.consumed_liened_backing_num != 0 {
                    bucket.status = BackingBucketStatusV16::Expired;
                } else {
                    bucket.status = BackingBucketStatusV16::Empty;
                    bucket.expiry_slot = 0;
                }
            }
            source.fresh_reserved_backing_num = source
                .fresh_reserved_backing_num
                .checked_sub(backing_num)
                .ok_or(PercolatorError::EngineCounterUnderflow)?;
            source.credit_rate_num = percolator::CREDIT_RATE_SCALE;
            source.credit_epoch = source
                .credit_epoch
                .checked_add(1)
                .ok_or(PercolatorError::EngineArithmeticOverflow)?;
            *source_acc = percolator::SourceCreditStateV16Account::from_runtime(&source);
            *bucket_acc = percolator::BackingBucketV16Account::from_runtime(&bucket);
            group.header.risk_epoch = percolator::V16PodU64::new(
                group
                    .header
                    .risk_epoch
                    .get()
                    .checked_add(1)
                    .ok_or(PercolatorError::EngineArithmeticOverflow)?,
            );
            group.header.vault = percolator::V16PodU128::new(
                group
                    .header
                    .vault
                    .get()
                    .checked_sub(atoms)
                    .ok_or(PercolatorError::EngineCounterUnderflow)?,
            );
            ledger.total_principal_atoms = ledger
                .total_principal_atoms
                .checked_sub(atoms)
                .ok_or(PercolatorError::EngineCounterUnderflow)?;
            ledger.total_principal_withdrawn_atoms = ledger
                .total_principal_withdrawn_atoms
                .checked_add(atoms)
                .ok_or(PercolatorError::EngineArithmeticOverflow)?;
            group.validate_shape().map_err(map_v16_error)?;
            write_or_init_backing_domain_ledger(&mut ledger_data, &ledger, initialized)?;
        }

        // ── Transfer vault → redeemer (vault_authority PDA signs). ──
        let vault_bump_arr = [vault_bump];
        let vault_seeds: &[&[&[u8]]] = &[&[b"vault", market_ai.key.as_ref(), &vault_bump_arr]];
        transfer_tokens_signed(
            token_program,
            vault_token,
            redeemer_dest,
            vault_authority_ai,
            atoms_u64,
            vault_seeds,
        )?;

        // ── Burn the escrowed shares (registry PDA signs as escrow owner). ──
        let shares_u64 =
            u64::try_from(redemption.shares).map_err(|_| PercolatorError::EngineArithmeticOverflow)?;
        let burn_ix = spl_token::instruction::burn(
            token_program.key,
            escrow_ai.key,
            lp_mint.key,
            &registry_pda,
            &[],
            shares_u64,
        )?;
        invoke_signed(
            &burn_ix,
            &[
                escrow_ai.clone(),
                lp_mint.clone(),
                registry_ai.clone(),
                token_program.clone(),
            ],
            &[&[
                crate::constants::LP_VAULT_REGISTRY_SEED,
                market_ai.key.as_ref(),
                &[registry_bump],
            ]],
        )?;

        // ── Decrement outstanding shares (mirrors the burn). ──
        {
            let mut reg = state::read_lp_vault_registry(&registry_ai.try_borrow_data()?)?;
            reg.total_lp_shares_outstanding = reg
                .total_lp_shares_outstanding
                .checked_sub(redemption.shares)
                .ok_or(PercolatorError::EngineCounterUnderflow)?;
            state::write_lp_vault_registry(&mut registry_ai.try_borrow_mut_data()?, &reg)?;
        }

        // ── Consume the redemption PDA (zero magic — replay guard) + reclaim rent. ──
        state::consume_lp_redemption(&mut redemption_ai.try_borrow_mut_data()?)?;
        let reclaim = redemption_ai.lamports();
        **redemption_ai.try_borrow_mut_lamports()? = 0;
        **cranker.try_borrow_mut_lamports()? = cranker
            .lamports()
            .checked_add(reclaim)
            .ok_or(PercolatorError::EngineArithmeticOverflow)?;
        Ok(())
    }

    /// LP Vault — LpVaultCrankFees (tag 69). Phase 2.B Tier 3 Workstream 4B / Phase F.
    ///
    /// Permissionless snapshot bookkeeping. Syncs the backing-domain ledger from
    /// the live bucket (so NAV reflects accrued utilization-fee earnings), then
    /// advances the insurance-fee snapshot and records the cumulative LP-side
    /// distribution.
    ///
    /// CORRECTED MODEL (sign-off Note 4): the earnings signal is
    /// `total_earnings_atoms` (synced from `bucket.utilization_fee_earnings`),
    /// NOT `cumulative_recovery_atoms`. LP-side earnings accrue AUTOMATICALLY via
    /// NAV (`total_earnings_atoms` is a `lp_vault_nav_atoms` input) — this crank
    /// moves NO tokens; it only advances the snapshot + audit counter. The
    /// insurance-side fraction is a v1 stub (Note 3): it accrues in the bucket as
    /// a protocol reserve; no insurance token routing in v1.
    ///
    /// Conservation: structurally pure-meta (snapshot + audit-counter update +
    /// ledger sync persist). No token/stock/lien/reservation value change beyond
    /// the idempotent ledger sync. All 4 proofs N/A.
    #[inline(never)]
    fn handle_lp_vault_crank_fees<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
    ) -> ProgramResult {
        let cranker = account(accounts, 0)?;
        let market_ai = account(accounts, 1)?;
        let registry_ai = account(accounts, 2)?;
        let ledger_ai = account(accounts, 3)?;
        expect_signer(cranker)?;
        expect_writable(market_ai)?;
        expect_writable(registry_ai)?;
        expect_writable(ledger_ai)?;
        expect_owner(market_ai, program_id)?;
        expect_owner(registry_ai, program_id)?;
        expect_owner(ledger_ai, program_id)?;

        let registry = state::read_lp_vault_registry(&registry_ai.try_borrow_data()?)?;
        let market_key = Pubkey::new_from_array(registry.market_group);
        expect_key(market_ai, &market_key)?;
        let (registry_pda, _) = state::derive_lp_vault_registry(program_id, &market_key);
        expect_key(registry_ai, &registry_pda)?;
        let (ledger_pda, _) =
            state::derive_lp_backing_ledger(program_id, &market_key, registry.domain);
        expect_key(ledger_ai, &ledger_pda)?;
        let domain = registry.domain as usize;

        // Sync the ledger from the live bucket, persist, read current earnings.
        let total_earnings = {
            let mut market_data = market_ai.try_borrow_mut_data()?;
            let (_, group) = state::market_view_mut(&mut market_data)?;
            let (_, bucket) = backing_domain_parts_view(&group, domain)?;
            let mut ledger_data = ledger_ai.try_borrow_mut_data()?;
            let (mut ledger, initialized) = read_or_new_backing_domain_ledger(
                &ledger_data,
                market_ai.key.to_bytes(),
                registry_pda.to_bytes(),
                registry.domain,
                &bucket,
            )?;
            sync_backing_domain_ledger(&mut ledger, &bucket)?;
            let te = ledger.total_earnings_atoms;
            write_or_init_backing_domain_ledger(&mut ledger_data, &ledger, initialized)?;
            te
        };

        let delta = total_earnings
            .checked_sub(registry.insurance_fee_snapshot_atoms)
            .ok_or(PercolatorError::EngineCounterUnderflow)?;
        if delta == 0 {
            return Err(PercolatorError::LpVaultNoFeesToCrank.into());
        }
        let (lp_side, _insurance_side) =
            percolator::lp_vault::lp_fee_split(delta, registry.fee_share_bps).map_err(map_v16_error)?;
        {
            let mut reg = state::read_lp_vault_registry(&registry_ai.try_borrow_data()?)?;
            reg.insurance_fee_snapshot_atoms = total_earnings;
            reg.fee_distribution_total_atoms = reg
                .fee_distribution_total_atoms
                .checked_add(lp_side)
                .ok_or(PercolatorError::EngineArithmeticOverflow)?;
            state::write_lp_vault_registry(&mut registry_ai.try_borrow_mut_data()?, &reg)?;
        }
        Ok(())
    }

    /// LP Vault — SetLpVaultPaused (tag 70). Phase 2.B Tier 3 Workstream 4B / Phase G.
    /// Admin toggles the vault pause flag. Paused vaults reject DepositToLpVault
    /// and RequestRedeemLpShares (ExecuteRedemption + CloseLpVault still allowed).
    /// Pure-meta; no conservation proofs.
    #[inline(never)]
    fn handle_set_lp_vault_paused<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        paused: u8,
    ) -> ProgramResult {
        let admin = account(accounts, 0)?;
        let market_ai = account(accounts, 1)?;
        let registry_ai = account(accounts, 2)?;
        expect_signer(admin)?;
        expect_writable(registry_ai)?;
        expect_owner(market_ai, program_id)?;
        expect_owner(registry_ai, program_id)?;
        if paused > 1 {
            return Err(PercolatorError::InvalidInstruction.into());
        }
        let mut registry = state::read_lp_vault_registry(&registry_ai.try_borrow_data()?)?;
        let market_key = Pubkey::new_from_array(registry.market_group);
        expect_key(market_ai, &market_key)?;
        let (registry_pda, _) = state::derive_lp_vault_registry(program_id, &market_key);
        expect_key(registry_ai, &registry_pda)?;
        let (cfg, _, _, _) =
            state::read_market_config_mode_and_capacity(&market_ai.try_borrow_data()?)?;
        if admin.key.to_bytes() != cfg.admin {
            return Err(PercolatorError::Unauthorized.into());
        }
        registry.paused = paused;
        state::write_lp_vault_registry(&mut registry_ai.try_borrow_mut_data()?, &registry)?;
        Ok(())
    }

    /// LP Vault — CloseLpVault (tag 71). Phase 2.B Tier 3 Workstream 4B / Phase G.
    /// Admin-gated. Requires zero outstanding shares (registry counter AND the
    /// live LP mint supply — defense against an I2 desync). Closes the registry
    /// PDA (zero data + reclaim rent to admin). The LP mint is left on-chain at
    /// supply 0 as a historical record (design §19).
    /// Pure-meta; no conservation proofs.
    #[inline(never)]
    fn handle_close_lp_vault<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
    ) -> ProgramResult {
        let admin = account(accounts, 0)?;
        let market_ai = account(accounts, 1)?;
        let registry_ai = account(accounts, 2)?;
        let lp_mint = account(accounts, 3)?;
        expect_signer(admin)?;
        expect_writable(admin)?;
        expect_writable(registry_ai)?;
        expect_owner(market_ai, program_id)?;
        expect_owner(registry_ai, program_id)?;

        let registry = state::read_lp_vault_registry(&registry_ai.try_borrow_data()?)?;
        if registry.total_lp_shares_outstanding != 0 {
            return Err(PercolatorError::LpVaultSharesOutstanding.into());
        }
        let market_key = Pubkey::new_from_array(registry.market_group);
        expect_key(market_ai, &market_key)?;
        let (registry_pda, _) = state::derive_lp_vault_registry(program_id, &market_key);
        expect_key(registry_ai, &registry_pda)?;
        if lp_mint.key.to_bytes() != registry.lp_mint {
            return Err(PercolatorError::InvalidMint.into());
        }
        let (cfg, _, _, _) =
            state::read_market_config_mode_and_capacity(&market_ai.try_borrow_data()?)?;
        if admin.key.to_bytes() != cfg.admin {
            return Err(PercolatorError::Unauthorized.into());
        }
        // Defense-in-depth (I2): the live SPL mint supply must also be zero.
        {
            let mint_data = lp_mint.try_borrow_data()?;
            let mint = spl_token::state::Mint::unpack(&mint_data)?;
            if mint.supply != 0 {
                return Err(PercolatorError::LpVaultSharesOutstanding.into());
            }
        }
        // Close the registry PDA: zero data + reclaim rent to admin.
        {
            let mut data = registry_ai.try_borrow_mut_data()?;
            for b in data.iter_mut() {
                *b = 0;
            }
        }
        let reclaim = registry_ai.lamports();
        **registry_ai.try_borrow_mut_lamports()? = 0;
        **admin.try_borrow_mut_lamports()? = admin
            .lamports()
            .checked_add(reclaim)
            .ok_or(PercolatorError::EngineArithmeticOverflow)?;
        Ok(())
    }

    // ── Fork NFT / B-3 (Phase 2.B Tier 3 / A.4) ─────────────────────────────
    //
    // SetNftProgramId (tag 73) + TransferPortfolioOwnership (tag 72).
    //
    // Security design: nft_design.md §7 and §7.0.  All 6 guardrails are
    // implemented.  The fund-critical comment from the design doc is reproduced
    // here for review-time visibility:
    //
    //   "TransferPortfolioOwnership reassigns control of a portfolio and
    //    everything in it — under-gated = portfolio theft."
    //
    // Guardrail 5 (trust boundary) — DOCUMENTED HERE: the wrapper trusts the
    // PDA-signing NFT program (proven via Guardrail 1) to have verified NFT
    // holdership; the wrapper does NOT re-check holdership.
    //
    // Guardrail 6 (conservation, N/A) — DOCUMENTED HERE: owner-field rewrite
    // only; zero token/stock/lien movement; portfolio value invariant.  All 4
    // v16 conservation proofs are N/A:
    //   - TokenValueFlowProofV16     N/A — no token transfer
    //   - ReservationEncumbranceProof N/A — no encumbrance change
    //   - StockReconciliation        N/A — no stock mutation
    //   - SourceCreditLienAggregate  N/A — no lien change
    //
    // Cross-ref: NFT program `ExecuteTransferHook` is the CPI caller; see
    // `percolator-nft/src/transfer_hook.rs handle_execute_transfer_hook`.

    /// B-3 core: pure function operating on the portfolio POD.
    ///
    /// Implements Guardrails 2 (atomic dual-write), 3 (state gating),
    /// 4 (no-op/self-transfer).  The handler performs Guardrail 1 (auth).
    ///
    /// Returns `Ok(())` and writes BOTH `p.owner` AND
    /// `p.provenance_header.owner` to `new_owner` — or returns an `Err`
    /// leaving the struct byte-identical to its input (Solana reverts all
    /// mutations on instruction failure so the caller invariant also holds at
    /// the account level).
    ///
    /// Unit tests in `tests/v16_fork_b3_nft_cpi.rs` assert that every
    /// value-relevant field (legs, capital, pnl, reserved_pnl, close_progress,
    /// health_cert, resolved_payout_receipt, …) is byte-identical before/after
    /// a successful call — proving the "portfolio value unchanged across
    /// transfer" conservation invariant.
    pub fn b3_check_and_rewrite_owner(
        p: &mut percolator::PortfolioAccountV16Account,
        new_owner: [u8; 32],
        asset_index: u16,
    ) -> Result<(), ProgramError> {
        use crate::error::PercolatorError;

        // ── Guardrail 4 (no-op / self-transfer) ─────────────────────────────
        if new_owner == [0u8; 32] {
            return Err(PercolatorError::NftTransferSelfOrZero.into());
        }
        if new_owner == p.owner {
            return Err(PercolatorError::NftTransferSelfOrZero.into());
        }

        // ── Guardrail 3 (state gating) ───────────────────────────────────────
        // Find the active leg for asset_index.
        let asset_index_u32 = asset_index as u32;
        let leg_slot: Option<&percolator::PortfolioLegV16Account> = {
            let mut found = None;
            let mut i = 0usize;
            while i < percolator::V16_MAX_PORTFOLIO_ASSETS_N {
                let leg = &p.legs[i];
                if leg.active != 0 && leg.asset_index.get() == asset_index_u32 {
                    found = Some(leg);
                    break;
                }
                i += 1;
            }
            found
        };
        let leg = leg_slot.ok_or(PercolatorError::NftPortfolioNotTransferable)?;

        // Portfolio-level lock/stale gates.
        if p.liquidation_lock != 0
            || p.stale_state != 0
            || p.b_stale_state != 0
        {
            return Err(PercolatorError::NftPortfolioNotTransferable.into());
        }

        // Resolved-payout receipt gate.
        if p.resolved_payout_receipt.present != 0 {
            return Err(PercolatorError::NftPortfolioNotTransferable.into());
        }

        // Mid-close gate: reject if a close is in progress for this asset.
        if p.close_progress.active != 0
            && p.close_progress.asset_index.get() == asset_index_u32
        {
            return Err(PercolatorError::NftPortfolioNotTransferable.into());
        }

        // Per-leg stale gates.
        if leg.b_stale != 0 || leg.stale != 0 {
            return Err(PercolatorError::NftPortfolioNotTransferable.into());
        }

        // ── Guardrail 2 (atomic dual-write) ──────────────────────────────────
        // Write BOTH owner fields in the SAME mutation.  Any Err above leaves
        // both fields untouched; we only reach here when all gates pass.
        p.owner = new_owner;
        p.provenance_header.owner = new_owner;

        // Post-write assertion: both fields must match (guards against future
        // re-ordering of the write lines above).
        assert_eq!(
            p.owner,
            p.provenance_header.owner,
            "b3: dual-write invariant violated"
        );
        assert_eq!(p.owner, new_owner, "b3: owner not written");

        Ok(())
    }

    /// SetNftProgramId (tag 73) — creates or updates the per-market
    /// `NftRegistryV16` PDA.
    ///
    /// Accounts:
    ///   0  admin        [signer, writable] — pays rent on create
    ///   1  market       [ro]               — source of cfg.admin + key for PDA
    ///   2  nft_registry [writable, PDA]    — `["nft_registry", market.key]`
    ///   3  system_program
    ///
    /// Guardrail: admin == cfg.admin; nft_program_id != [0;32].
    ///
    /// Re-pointing (updating an existing registry) is deliberate — a comment is
    /// left noting that it freezes in-flight transfers for NFTs minted under the
    /// old program ID.  No code needed for that: the fail-closed signer check
    /// already rejects those CPIs after the update.
    #[inline(never)]
    fn handle_set_nft_program_id<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        nft_program_id: [u8; 32],
    ) -> ProgramResult {
        use crate::{constants, error::PercolatorError};

        let admin = account(accounts, 0)?;
        let market_ai = account(accounts, 1)?;
        let registry_ai = account(accounts, 2)?;
        let system_program_ai = account(accounts, 3)?;

        expect_signer(admin)?;
        expect_writable(admin)?;
        expect_writable(registry_ai)?;
        expect_owner(market_ai, program_id)?;

        if system_program_ai.key != &system_program::ID {
            return Err(PercolatorError::InvalidInstruction.into());
        }
        if nft_program_id == [0u8; 32] {
            return Err(PercolatorError::InvalidInstruction.into());
        }

        // Verify admin authorization from market config.
        let (cfg, _, _, _) =
            state::read_market_config_mode_and_capacity(&market_ai.try_borrow_data()?)?;
        if admin.key.to_bytes() != cfg.admin {
            return Err(PercolatorError::Unauthorized.into());
        }

        // Derive PDA and bind.
        let (expected_pda, bump) =
            state::derive_nft_registry(program_id, market_ai.key);
        expect_key(registry_ai, &expected_pda)?;

        if registry_ai.owner == &system_program::ID && registry_ai.data_is_empty() {
            // CREATE path: account does not yet exist.
            let registry_len = state::nft_registry_account_len();
            let rent = Rent::get()?;
            let market_bytes = market_ai.key.to_bytes();
            invoke_signed(
                &system_instruction::create_account(
                    admin.key,
                    registry_ai.key,
                    rent.minimum_balance(registry_len),
                    registry_len as u64,
                    program_id,
                ),
                &[admin.clone(), registry_ai.clone(), system_program_ai.clone()],
                &[&[
                    constants::NFT_REGISTRY_SEED,
                    market_bytes.as_ref(),
                    &[bump],
                ]],
            )?;
            let new_reg = state::NftRegistryV16 {
                market_group: market_ai.key.to_bytes(),
                nft_program_id,
                version: constants::NFT_REGISTRY_VERSION,
                bump,
                _padding: [0u8; 6],
            };
            state::init_nft_registry(&mut registry_ai.try_borrow_mut_data()?, &new_reg)?;
        } else {
            // UPDATE path: registry already exists — re-point the nft_program_id.
            // NOTE: updating the registry freezes in-flight NFT transfers for
            // NFTs minted under the old program ID.  Those CPIs fail with
            // NftInvalidMintAuthority (signer != derive_nft_mint_authority(new
            // program_id)).  This is intentional: re-pointing is a deliberate
            // admin action.
            expect_owner(registry_ai, program_id)?;
            let mut reg = state::read_nft_registry(&registry_ai.try_borrow_data()?)?;
            // Bind the stored market_group to the account we just validated.
            if reg.market_group != market_ai.key.to_bytes() {
                return Err(PercolatorError::EngineProvenanceMismatch.into());
            }
            reg.nft_program_id = nft_program_id;
            state::write_nft_registry(&mut registry_ai.try_borrow_mut_data()?, &reg)?;
        }

        Ok(())
    }

    /// B-3 TransferPortfolioOwnership (tag 72).
    ///
    /// CPI-ONLY — the NFT program's `ExecuteTransferHook` calls this with its
    /// `mint_authority` PDA as the signer (cryptographic proof of NFT-program
    /// identity).
    ///
    /// Accounts:
    ///   0  mint_auth    [signer]           — NFT program's mint-authority PDA
    ///   1  portfolio    [writable]         — portfolio being transferred
    ///   2  nft_registry [ro, PDA]          — `["nft_registry", market_group]`
    ///
    /// All 6 §7 guardrails are implemented:
    ///   1 Auth + registry FAIL-CLOSED (uninitialized registry = reject)
    ///   2 Atomic dual-write (owner AND provenance_header.owner)
    ///   3 State gating (active leg, no lock/stale/close/resolved)
    ///   4 No-op/self-transfer rejection
    ///   5 Trust boundary: wrapper trusts NFT program via Guardrail 1
    ///   6 Conservation N/A — see module-level comment above
    #[inline(never)]
    fn handle_transfer_portfolio_ownership<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        new_owner: [u8; 32],
        asset_index: u16,
    ) -> ProgramResult {
        use crate::error::PercolatorError;

        let mint_auth_ai = account(accounts, 0)?;
        let portfolio_ai = account(accounts, 1)?;
        let registry_ai = account(accounts, 2)?;

        // ── Guardrail 1 (auth + registry, FAIL-CLOSED) ──────────────────────
        expect_signer(mint_auth_ai)?;
        expect_writable(portfolio_ai)?;
        expect_owner(portfolio_ai, program_id)?;

        // Validate portfolio header (KIND_PORTFOLIO + MAGIC + VERSION).
        {
            let data = portfolio_ai.try_borrow_data()?;
            state::check_portfolio_kind(&data)?;
        }

        // Read portfolio POD to get market_group_id for registry binding.
        let market_group_bytes: [u8; 32] = {
            let data = portfolio_ai.try_borrow_data()?;
            let wire = state::portfolio_wire(&data)?;
            // Validate provenance header (checks layout_discriminator == 16 AND version == 1).
            wire.provenance_header
                .try_to_runtime()
                .map_err(|_| PercolatorError::NftPortfolioProvenance)?;
            // Verify anti-substitution anchor: portfolio account key == provenance id.
            if wire.provenance_header.portfolio_account_id != portfolio_ai.key.to_bytes() {
                return Err(PercolatorError::NftPortfolioProvenance.into());
            }
            wire.provenance_header.market_group_id
        };

        // Read NftRegistry — FAIL-CLOSED: uninitialized = NotInitialized error.
        let market_group_key = Pubkey::new_from_array(market_group_bytes);
        let (expected_registry_pda, _) =
            state::derive_nft_registry(program_id, &market_group_key);
        expect_key(registry_ai, &expected_registry_pda)?;
        expect_owner(registry_ai, program_id)?;

        let registry = state::read_nft_registry(&registry_ai.try_borrow_data()?)
            .map_err(|_| PercolatorError::NftRegistryNotFound)?;

        // Bind registry to this market.
        if registry.market_group != market_group_bytes {
            return Err(PercolatorError::NftRegistryNotFound.into());
        }

        // Derive expected mint-authority PDA from the registered NFT program.
        let nft_program_id = Pubkey::new_from_array(registry.nft_program_id);
        let (expected_mint_auth, _) =
            state::derive_nft_mint_authority(&nft_program_id);
        if mint_auth_ai.key != &expected_mint_auth {
            return Err(PercolatorError::NftInvalidMintAuthority.into());
        }
        // Signer is already asserted above; this is the critical check:
        // a valid signer proves the CPI came from `nft_program_id`.

        // ── Guardrails 2/3/4 via pure function ──────────────────────────────
        {
            let mut data = portfolio_ai.try_borrow_mut_data()?;
            let p = state::portfolio_wire_mut(&mut data)?;
            b3_check_and_rewrite_owner(p, new_owner, asset_index)?;

            // Post-write: assert invariant holds in the on-chain data bytes.
            debug_assert_eq!(
                p.owner,
                p.provenance_header.owner,
                "b3 handler: dual-write invariant violated"
            );
        }

        Ok(())
    }

    #[inline(never)]
    fn handle_init_market<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        max_portfolio_assets: u16,
        h_min: u64,
        h_max: u64,
        initial_price: u64,
        min_nonzero_mm_req: u128,
        min_nonzero_im_req: u128,
        maintenance_margin_bps: u64,
        initial_margin_bps: u64,
        max_trading_fee_bps: u64,
        trade_fee_base_bps: u64,
        liquidation_fee_bps: u64,
        liquidation_fee_cap: u128,
        min_liquidation_abs: u128,
        max_price_move_bps_per_slot: u64,
        max_accrual_dt_slots: u64,
        max_abs_funding_e9_per_slot: u64,
        min_funding_lifetime_slots: u64,
        max_account_b_settlement_chunks: u64,
        max_bankrupt_close_chunks: u64,
        max_bankrupt_close_lifetime_slots: u64,
        public_b_chunk_atoms: u128,
        maintenance_fee_per_slot: u128,
    ) -> ProgramResult {
        let admin = account(accounts, 0)?;
        let market_ai = account(accounts, 1)?;
        let mint_ai = account(accounts, 2)?;
        expect_signer(admin)?;
        expect_writable(market_ai)?;
        expect_owner(market_ai, program_id)?;
        verify_mint(mint_ai)?;
        if trade_fee_base_bps > max_trading_fee_bps
            || max_portfolio_assets == 0
            || max_portfolio_assets > constants::WRAPPER_MAX_PORTFOLIO_ASSETS
            || h_max as u128 > BOUND_SCALE
            || maintenance_fee_per_slot > percolator::MAX_PROTOCOL_FEE_ABS
        {
            return Err(PercolatorError::EngineInvalidConfig.into());
        }
        let mut cfg = V16Config::public_user_fund(max_portfolio_assets, h_min, h_max);
        cfg.min_nonzero_mm_req = min_nonzero_mm_req;
        cfg.min_nonzero_im_req = min_nonzero_im_req;
        cfg.maintenance_margin_bps = maintenance_margin_bps;
        cfg.initial_margin_bps = initial_margin_bps;
        cfg.max_trading_fee_bps = max_trading_fee_bps;
        cfg.liquidation_fee_bps = liquidation_fee_bps;
        cfg.liquidation_fee_cap = liquidation_fee_cap;
        cfg.min_liquidation_abs = min_liquidation_abs;
        cfg.max_price_move_bps_per_slot = max_price_move_bps_per_slot;
        cfg.max_accrual_dt_slots = max_accrual_dt_slots;
        cfg.max_abs_funding_e9_per_slot = max_abs_funding_e9_per_slot;
        cfg.min_funding_lifetime_slots = min_funding_lifetime_slots;
        cfg.max_account_b_settlement_chunks = max_account_b_settlement_chunks;
        cfg.max_bankrupt_close_chunks = max_bankrupt_close_chunks;
        cfg.max_bankrupt_close_lifetime_slots = max_bankrupt_close_lifetime_slots;
        cfg.public_b_chunk_atoms = public_b_chunk_atoms;
        if initial_price == 0 || initial_price > percolator::MAX_ORACLE_PRICE {
            return Err(PercolatorError::EngineInvalidConfig.into());
        }
        let init_slot = Clock::get().map(|c| c.slot).unwrap_or(0);
        let wrapper = WrapperConfigV16 {
            admin: admin.key.to_bytes(),
            collateral_mint: mint_ai.key.to_bytes(),
            secondary_collateral_mint: [0u8; 32],
            base_unit_authority: admin.key.to_bytes(),
            maintenance_fee_per_slot,
            permissionless_market_init_fee: 0,
            trade_fee_base_bps,
            permissionless_resolve_stale_slots: 0,
            force_close_delay_slots: 0,
            last_good_oracle_slot: init_slot,
            insurance_authority: admin.key.to_bytes(),
            insurance_operator: admin.key.to_bytes(),
            backing_bucket_authority: admin.key.to_bytes(),
            asset_authority: admin.key.to_bytes(),
            mark_authority: admin.key.to_bytes(),
            insurance_withdraw_deposit_remaining: 0,
            insurance_withdraw_max_bps: 0,
            liquidation_cranker_fee_share_bps: 0,
            maintenance_cranker_fee_share_bps: 0,
            backing_trade_fee_bps_long: 0,
            backing_trade_fee_bps_short: 0,
            unit_scale: 0,
            conf_filter_bps: 0,
            insurance_withdraw_deposits_only: 0,
            oracle_mode: constants::ORACLE_MODE_MANUAL,
            oracle_leg_count: 0,
            oracle_leg_flags: 0,
            invert: 0,
            _padding0: 0,
            free_market_slot_count: 0,
            insurance_withdraw_cooldown_slots: 0,
            last_insurance_withdraw_slot: 0,
            max_staleness_secs: 0,
            hybrid_soft_stale_slots: 0,
            mark_ewma_e6: initial_price,
            mark_ewma_last_slot: init_slot,
            mark_ewma_halflife_slots: constants::DEFAULT_MARK_EWMA_HALFLIFE_SLOTS,
            mark_min_fee: 0,
            oracle_target_price_e6: initial_price,
            oracle_target_publish_time: 0,
            oracle_leg_feeds: [[0u8; 32]; constants::ORACLE_LEG_CAP],
            oracle_leg_prices_e6: [0u64; constants::ORACLE_LEG_CAP],
            oracle_leg_publish_times: [0i64; constants::ORACLE_LEG_CAP],
            backing_trade_fee_policy_count: 0,
            backing_trade_fee_insurance_share_bps_long: 0,
            backing_trade_fee_insurance_share_bps_short: 0,
            fee_redirect_to_market_0_bps: 0,
        };
        state::init_market_account_zero_copy(
            &mut market_ai.try_borrow_mut_data()?,
            &wrapper,
            cfg,
            market_ai.key.to_bytes(),
            initial_price,
            init_slot,
        )
    }

    #[inline(never)]
    fn handle_init_portfolio<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
    ) -> ProgramResult {
        let owner = account(accounts, 0)?;
        let market_ai = account(accounts, 1)?;
        let portfolio_ai = account(accounts, 2)?;
        expect_signer(owner)?;
        expect_writable(market_ai)?;
        expect_writable(portfolio_ai)?;
        expect_owner(market_ai, program_id)?;
        expect_owner(portfolio_ai, program_id)?;
        if state::is_initialized(&portfolio_ai.try_borrow_data()?) {
            return Err(PercolatorError::AlreadyInitialized.into());
        }
        let (cfg, mode, max_market_slots, _) =
            state::read_market_config_mode_and_capacity(&market_ai.try_borrow_data()?)?;
        if mode != MarketModeV16::Live {
            return Err(PercolatorError::EngineLockActive.into());
        }
        let source_domain_count =
            v16_domain_count_for_market_slots(max_market_slots as u32).map_err(map_v16_error)?;
        let required_portfolio_len =
            state::portfolio_account_len_for_market_slots(max_market_slots)?;
        if portfolio_ai.data_len() < required_portfolio_len {
            portfolio_ai.realloc(required_portfolio_len, true)?;
        }
        {
            let mut market_data = market_ai.try_borrow_mut_data()?;
            let (cfg, group) = state::market_view_mut(&mut market_data)?;
            if group.header.mode != 0 {
                return Err(PercolatorError::EngineLockActive.into());
            }
            reject_permissionless_resolve_matured_live_view(&cfg, &group)?;
            let last_fee_slot = authenticated_market_slot_or_fallback_view(&group);
            state::init_portfolio_account_zero_copy(
                &mut portfolio_ai.try_borrow_mut_data()?,
                market_ai.key.to_bytes(),
                portfolio_ai.key.to_bytes(),
                owner.key.to_bytes(),
                last_fee_slot,
                max_market_slots,
            )?;
            let mut portfolio_data = portfolio_ai.try_borrow_mut_data()?;
            let portfolio =
                state::portfolio_view_mut_for_market_slots(&mut portfolio_data, max_market_slots)?;
            portfolio
                .as_view()
                .validate_with_market(&group.as_view())
                .map_err(map_v16_error)?;
            let next = group
                .header
                .materialized_portfolio_count
                .get()
                .checked_add(1)
                .ok_or(PercolatorError::EngineCounterOverflow)?;
            group.header.materialized_portfolio_count = percolator::V16PodU64::new(next);
            group.validate_shape().map_err(map_v16_error)?;
        }
        let _ = (cfg, source_domain_count);
        Ok(())
    }

    #[inline(never)]
    fn handle_deposit<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        amount: u128,
    ) -> ProgramResult {
        let owner = account(accounts, 0)?;
        let market_ai = account(accounts, 1)?;
        let portfolio_ai = account(accounts, 2)?;
        let source_token = account(accounts, 3)?;
        let vault_token = account(accounts, 4)?;
        let token_program = account(accounts, 5)?;
        expect_signer(owner)?;
        expect_writable(market_ai)?;
        expect_writable(portfolio_ai)?;
        expect_writable(source_token)?;
        expect_writable(vault_token)?;
        expect_owner(market_ai, program_id)?;
        expect_owner(portfolio_ai, program_id)?;
        verify_token_program(token_program)?;

        let (cfg, mode, max_market_slots, _) =
            state::read_market_config_mode_and_capacity(&market_ai.try_borrow_data()?)?;
        if mode != MarketModeV16::Live {
            return Err(PercolatorError::EngineLockActive.into());
        }
        let mint = primary_collateral_mint(&cfg);
        let (vault_authority, _) = derive_vault_authority(program_id, market_ai.key);
        verify_user_token_account(source_token, owner.key, &mint)?;
        verify_vault_token_account(vault_token, &vault_authority, &mint)?;
        let amount_u64 = amount_to_u64(amount)?;
        require_token_balance(source_token, amount_u64)?;

        ensure_portfolio_storage_for_market_slots(portfolio_ai, max_market_slots)?;
        {
            let mut market_data = market_ai.try_borrow_mut_data()?;
            let (cfg, mut group) = state::market_view_mut(&mut market_data)?;
            if group.header.mode != 0 {
                return Err(PercolatorError::EngineLockActive.into());
            }
            reject_permissionless_resolve_matured_live_view(&cfg, &group)?;
            let mut portfolio_data = portfolio_ai.try_borrow_mut_data()?;
            let mut portfolio =
                state::portfolio_view_mut_for_market_slots(&mut portfolio_data, max_market_slots)?;
            expect_portfolio_view_account_key(&portfolio, portfolio_ai.key)?;
            expect_portfolio_view_owner(&portfolio, owner.key)?;
            group
                .deposit_not_atomic(&mut portfolio, amount)
                .map_err(map_v16_error)?;
        }
        transfer_tokens(token_program, source_token, vault_token, owner, amount_u64)?;
        Ok(())
    }

    #[inline(never)]
    fn handle_withdraw<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        amount: u128,
    ) -> ProgramResult {
        let owner = account(accounts, 0)?;
        let market_ai = account(accounts, 1)?;
        let portfolio_ai = account(accounts, 2)?;
        let dest_token = account(accounts, 3)?;
        let vault_token = account(accounts, 4)?;
        let vault_authority_ai = account(accounts, 5)?;
        let token_program = account(accounts, 6)?;
        expect_signer(owner)?;
        expect_writable(market_ai)?;
        expect_writable(portfolio_ai)?;
        expect_writable(dest_token)?;
        expect_writable(vault_token)?;
        expect_owner(market_ai, program_id)?;
        expect_owner(portfolio_ai, program_id)?;
        verify_token_program(token_program)?;

        let (cfg, mode, max_market_slots, _) =
            state::read_market_config_mode_and_capacity(&market_ai.try_borrow_data()?)?;
        if mode != MarketModeV16::Live {
            return Err(PercolatorError::EngineLockActive.into());
        }
        let (vault_authority, bump) = derive_vault_authority(program_id, market_ai.key);
        expect_key(vault_authority_ai, &vault_authority)?;
        verify_withdrawable_token_accounts(
            dest_token,
            owner.key,
            vault_token,
            &vault_authority,
            &cfg,
        )?;
        let amount_u64 = amount_to_u64(amount)?;
        require_token_balance(vault_token, amount_u64)?;

        ensure_portfolio_storage_for_market_slots(portfolio_ai, max_market_slots)?;
        {
            let mut market_data = market_ai.try_borrow_mut_data()?;
            let (cfg, mut group) = state::market_view_mut(&mut market_data)?;
            if group.header.mode != 0 {
                return Err(PercolatorError::EngineLockActive.into());
            }
            reject_permissionless_resolve_matured_live_view(&cfg, &group)?;
            let mut portfolio_data = portfolio_ai.try_borrow_mut_data()?;
            let mut portfolio =
                state::portfolio_view_mut_for_market_slots(&mut portfolio_data, max_market_slots)?;
            expect_portfolio_view_account_key(&portfolio, portfolio_ai.key)?;
            expect_portfolio_view_owner(&portfolio, owner.key)?;
            group
                .withdraw_not_atomic(&mut portfolio, amount)
                .map_err(map_v16_error)?;
        }
        let bump_arr = [bump];
        let signer_seeds: &[&[&[u8]]] = &[&[b"vault", market_ai.key.as_ref(), &bump_arr]];
        transfer_tokens_signed(
            token_program,
            vault_token,
            dest_token,
            vault_authority_ai,
            amount_u64,
            signer_seeds,
        )
    }

    #[inline(never)]
    fn handle_trade_nocpi_zero_copy<'a>(
        _program_id: &Pubkey,
        signer_a: &AccountInfo<'a>,
        signer_b: &AccountInfo<'a>,
        market_ai: &AccountInfo<'a>,
        account_a_ai: &AccountInfo<'a>,
        account_b_ai: &AccountInfo<'a>,
        asset_index: u16,
        size_q: i128,
        exec_price: u64,
        fee_bps: u64,
        max_market_slots: usize,
    ) -> ProgramResult {
        ensure_portfolio_storage_for_market_slots(account_a_ai, max_market_slots)?;
        ensure_portfolio_storage_for_market_slots(account_b_ai, max_market_slots)?;
        let mut cfg_after = None;
        {
            let mut market_data = market_ai.try_borrow_mut_data()?;
            let (mut cfg, mut group) = state::market_view_mut(&mut market_data)?;
            let mut oracle_profile =
                read_oracle_profile_from_view(&group, &cfg, asset_index as usize)?;
            reject_permissionless_resolve_matured_live_for_profile_view(
                &cfg,
                &oracle_profile,
                &group,
            )?;
            let mut account_a_data = account_a_ai.try_borrow_mut_data()?;
            let mut account_b_data = account_b_ai.try_borrow_mut_data()?;
            let mut account_a =
                state::portfolio_view_mut_for_market_slots(&mut account_a_data, max_market_slots)?;
            let mut account_b =
                state::portfolio_view_mut_for_market_slots(&mut account_b_data, max_market_slots)?;
            expect_portfolio_view_account_key(&account_a, account_a_ai.key)?;
            expect_portfolio_view_account_key(&account_b, account_b_ai.key)?;
            expect_portfolio_view_owner(&account_a, signer_a.key)?;
            expect_portfolio_view_owner(&account_b, signer_b.key)?;
            let size_abs = if size_q == i128::MIN || size_q == 0 {
                return Err(PercolatorError::InvalidInstruction.into());
            } else {
                size_q.unsigned_abs()
            };
            let fee_bps = hybrid_trade_fee_bps_view(
                &cfg,
                &oracle_profile,
                &group,
                asset_index as usize,
                size_abs,
                exec_price,
                fee_bps,
            )?;
            let req = TradeRequestV16 {
                asset_index: asset_index as usize,
                size_q: size_abs,
                exec_price,
                fee_bps,
                // A-1 baseline: pass None so wrapper trade path is byte-identical
                // to upstream v16 behavior. Fork bundles that need the
                // admit-threshold ratchet (cf. V16_DIVERGENCES.md A-1) wire it
                // from instruction args in Phase 2.B.
                admit_h_max_consumption_threshold_bps_opt: None,
            };
            let backing_before = if cfg.backing_trade_fee_policy_count == 0 {
                None
            } else {
                Some((
                    source_counterparty_backing_snapshot_view(&account_a)?,
                    source_counterparty_backing_snapshot_view(&account_b)?,
                ))
            };
            // The engine stores loss-stale as a market-wide bit; isolate it to
            // trades and accounts that actually depend on stale assets.
            let restore_loss_stale_active = group.header.loss_stale_active;
            let ignore_unrelated_loss_stale = can_ignore_unrelated_loss_stale_for_trade_view(
                &group,
                &account_a,
                &account_b,
                asset_index as usize,
            )?;
            if ignore_unrelated_loss_stale {
                group.header.loss_stale_active = 0;
            }
            let outcome = if size_q > 0 {
                group
                    .execute_trade_with_fee_in_place_not_atomic(&mut account_a, &mut account_b, req)
                    .map_err(map_v16_error)?
            } else {
                group
                    .execute_trade_with_fee_in_place_not_atomic(&mut account_b, &mut account_a, req)
                    .map_err(map_v16_error)?
            };
            if ignore_unrelated_loss_stale {
                group.header.loss_stale_active = restore_loss_stale_active;
            }
            let backing_domain_fee =
                if let Some((backing_before_a, backing_before_b)) = backing_before {
                    apply_backing_domain_fees_after_trade_view(
                        &cfg,
                        &mut group,
                        &mut account_a,
                        backing_before_a.as_ref(),
                        &mut account_b,
                        backing_before_b.as_ref(),
                    )?
                } else {
                    0
                };
            credit_trade_fees_to_market_budgets_view(
                &cfg,
                &mut group,
                asset_index as usize,
                outcome.fee_a,
                outcome.fee_b,
            )?;
            update_hybrid_mark_after_trade_view(
                &mut oracle_profile,
                &group,
                asset_index as usize,
                exec_price,
                outcome
                    .fee_a
                    .checked_add(outcome.fee_b)
                    .and_then(|v| v.checked_add(backing_domain_fee))
                    .ok_or(PercolatorError::EngineArithmeticOverflow)?,
            )?;
            if asset_index == 0 && oracle_v16::profile_is_price_managed(&oracle_profile) {
                cfg.mark_ewma_e6 = oracle_profile.mark_ewma_e6;
                cfg.mark_ewma_last_slot = oracle_profile.mark_ewma_last_slot;
                cfg_after = Some(cfg);
            } else {
                write_oracle_profile_to_view_if_separate(
                    &mut group,
                    asset_index as usize,
                    &oracle_profile,
                )?;
            }
            if ignore_unrelated_loss_stale {
                group.header.loss_stale_active = 0;
            }
            let validation_result = group.validate_shape();
            if ignore_unrelated_loss_stale {
                group.header.loss_stale_active = restore_loss_stale_active;
            }
            validation_result.map_err(map_v16_error)?;
        }
        if let Some(cfg) = cfg_after {
            state::write_wrapper_config(&mut market_ai.try_borrow_mut_data()?, &cfg)?;
        }
        Ok(())
    }

    #[inline(never)]
    fn handle_trade_nocpi<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        asset_index: u16,
        size_q: i128,
        exec_price: u64,
        fee_bps: u64,
    ) -> ProgramResult {
        let signer_a = account(accounts, 0)?;
        let signer_b = account(accounts, 1)?;
        let market_ai = account(accounts, 2)?;
        let account_a_ai = account(accounts, 3)?;
        let account_b_ai = account(accounts, 4)?;
        expect_signer(signer_a)?;
        expect_signer(signer_b)?;
        expect_writable(market_ai)?;
        expect_writable(account_a_ai)?;
        expect_writable(account_b_ai)?;
        expect_owner(market_ai, program_id)?;
        expect_owner(account_a_ai, program_id)?;
        expect_owner(account_b_ai, program_id)?;
        if account_a_ai.key == account_b_ai.key {
            return Err(PercolatorError::InvalidInstruction.into());
        }
        let (_cfg_pre, mode_pre, max_market_slots, _) =
            state::read_market_config_mode_and_capacity(&market_ai.try_borrow_data()?)?;
        if mode_pre != MarketModeV16::Live {
            return Err(PercolatorError::EngineLockActive.into());
        }
        handle_trade_nocpi_zero_copy(
            program_id,
            signer_a,
            signer_b,
            market_ai,
            account_a_ai,
            account_b_ai,
            asset_index,
            size_q,
            exec_price,
            fee_bps,
            max_market_slots,
        )
    }

    fn active_leg_for_asset_view(
        portfolio: &percolator::PortfolioV16ViewMut<'_>,
        asset_index: usize,
    ) -> Result<percolator::PortfolioLegV16, ProgramError> {
        let mut found = None;
        let mut slot = 0usize;
        while slot < portfolio.header.legs.len() {
            let leg = portfolio.header.legs[slot]
                .try_to_runtime()
                .map_err(map_v16_error)?;
            if leg.active && leg.asset_index as usize == asset_index {
                if found.replace(leg).is_some() {
                    return Err(PercolatorError::EngineHiddenLeg.into());
                }
            }
            slot += 1;
        }
        found.ok_or(PercolatorError::EngineInvalidLeg.into())
    }

    fn asset_loss_stale_view(
        group: &state::MarketViewMutV16<'_>,
        asset_index: usize,
    ) -> Result<bool, ProgramError> {
        if asset_index >= group.header.config.max_market_slots.get() as usize
            || asset_index >= group.markets.len()
        {
            return Err(PercolatorError::EngineLockActive.into());
        }
        Ok(group.markets[asset_index].engine.asset.slot_last.get()
            < group.header.current_slot.get())
    }

    fn account_has_loss_stale_asset_exposure_view(
        group: &state::MarketViewMutV16<'_>,
        account: &percolator::PortfolioV16ViewMut<'_>,
    ) -> Result<bool, ProgramError> {
        let mut slot = 0usize;
        while slot < account.header.legs.len() {
            let leg = account.header.legs[slot]
                .try_to_runtime()
                .map_err(map_v16_error)?;
            if leg.active && asset_loss_stale_view(group, leg.asset_index as usize)? {
                return Ok(true);
            }
            slot += 1;
        }
        Ok(false)
    }

    fn can_ignore_unrelated_loss_stale_for_trade_view(
        group: &state::MarketViewMutV16<'_>,
        account_a: &percolator::PortfolioV16ViewMut<'_>,
        account_b: &percolator::PortfolioV16ViewMut<'_>,
        asset_index: usize,
    ) -> Result<bool, ProgramError> {
        if group.header.loss_stale_active == 0 {
            return Ok(false);
        }
        if asset_loss_stale_view(group, asset_index)?
            || account_has_loss_stale_asset_exposure_view(group, account_a)?
            || account_has_loss_stale_asset_exposure_view(group, account_b)?
        {
            return Ok(false);
        }
        Ok(true)
    }

    #[inline(never)]
    fn handle_force_close_abandoned_asset<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        asset_index: u16,
        now_slot: u64,
        close_q: u128,
    ) -> ProgramResult {
        let cranker = account(accounts, 0)?;
        let market_ai = account(accounts, 1)?;
        let account_a_ai = account(accounts, 2)?;
        let account_b_ai = account(accounts, 3)?;
        expect_signer(cranker)?;
        expect_writable(market_ai)?;
        expect_writable(account_a_ai)?;
        expect_writable(account_b_ai)?;
        expect_owner(market_ai, program_id)?;
        expect_owner(account_a_ai, program_id)?;
        expect_owner(account_b_ai, program_id)?;
        if account_a_ai.key == account_b_ai.key || close_q == 0 {
            return Err(PercolatorError::InvalidInstruction.into());
        }
        let asset_index_usize = asset_index as usize;
        let authenticated_slot = authenticated_slot_or_fallback(now_slot);
        let (_, mode_pre, max_market_slots, _) =
            state::read_market_config_mode_and_capacity(&market_ai.try_borrow_data()?)?;
        if mode_pre != MarketModeV16::Live {
            return Err(PercolatorError::EngineLockActive.into());
        }
        ensure_portfolio_storage_for_market_slots(account_a_ai, max_market_slots)?;
        ensure_portfolio_storage_for_market_slots(account_b_ai, max_market_slots)?;

        let mut market_data = market_ai.try_borrow_mut_data()?;
        let (cfg, mut group) = state::market_view_mut(&mut market_data)?;
        if group.header.mode != 0
            || asset_index_usize == 0
            || asset_index_usize >= group.header.config.max_market_slots.get() as usize
            || asset_index_usize >= group.markets.len()
            || cfg.force_close_delay_slots == 0
        {
            return Err(PercolatorError::EngineLockActive.into());
        }
        let asset = group.markets[asset_index_usize].engine.asset;
        if asset.lifecycle != ASSET_LIFECYCLE_RECOVERY {
            return Err(PercolatorError::EngineLockActive.into());
        }
        let profile = read_oracle_profile_from_view(&group, &cfg, asset_index_usize)?;
        let shutdown_slot = profile.last_good_oracle_slot;
        if shutdown_slot == 0
            || authenticated_slot < shutdown_slot
            || authenticated_slot.saturating_sub(shutdown_slot) < cfg.force_close_delay_slots
        {
            return Err(PercolatorError::EngineLockActive.into());
        }
        let frozen_mark = asset.effective_price.get();
        if frozen_mark == 0 || frozen_mark > percolator::MAX_ORACLE_PRICE {
            return Err(PercolatorError::OracleInvalid.into());
        }

        let mut account_a_data = account_a_ai.try_borrow_mut_data()?;
        let mut account_b_data = account_b_ai.try_borrow_mut_data()?;
        let mut account_a =
            state::portfolio_view_mut_for_market_slots(&mut account_a_data, max_market_slots)?;
        let mut account_b =
            state::portfolio_view_mut_for_market_slots(&mut account_b_data, max_market_slots)?;
        expect_portfolio_view_account_key(&account_a, account_a_ai.key)?;
        expect_portfolio_view_account_key(&account_b, account_b_ai.key)?;
        account_a
            .validate_with_market(&group.as_view())
            .map_err(map_v16_error)?;
        account_b
            .validate_with_market(&group.as_view())
            .map_err(map_v16_error)?;
        let leg_a = active_leg_for_asset_view(&account_a, asset_index_usize)?;
        let leg_b = active_leg_for_asset_view(&account_b, asset_index_usize)?;
        if leg_a.side == leg_b.side {
            return Err(PercolatorError::EngineInvalidLeg.into());
        }
        let close_q = close_q
            .min(leg_a.basis_pos_q.unsigned_abs())
            .min(leg_b.basis_pos_q.unsigned_abs());
        if close_q == 0 {
            return Err(PercolatorError::EngineNonProgress.into());
        }
        let req = TradeRequestV16 {
            asset_index: asset_index_usize,
            size_q: close_q,
            exec_price: frozen_mark,
            fee_bps: 0,
            // A-1 baseline (offsetting close-pair path): None preserves upstream
            // v16 semantics. Fork bundles wire this through in Phase 2.B.
            admit_h_max_consumption_threshold_bps_opt: None,
        };
        if leg_a.side == SideV16::Short {
            group
                .execute_trade_with_fee_in_place_not_atomic(&mut account_a, &mut account_b, req)
                .map_err(map_v16_error)?;
        } else {
            group
                .execute_trade_with_fee_in_place_not_atomic(&mut account_b, &mut account_a, req)
                .map_err(map_v16_error)?;
        }
        group.validate_shape().map_err(map_v16_error)?;
        account_a
            .validate_with_market(&group.as_view())
            .map_err(map_v16_error)?;
        account_b
            .validate_with_market(&group.as_view())
            .map_err(map_v16_error)
    }

    #[inline(never)]
    fn handle_trade_cpi<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        asset_index: u16,
        size_q: i128,
        fee_bps: u64,
        limit_price: u64,
    ) -> ProgramResult {
        let signer_a = account(accounts, 0)?;
        let signer_b = account(accounts, 1)?;
        let market_ai = account(accounts, 2)?;
        let account_a_ai = account(accounts, 3)?;
        let account_b_ai = account(accounts, 4)?;
        let matcher_prog = account(accounts, 5)?;
        let matcher_ctx = account(accounts, 6)?;
        let matcher_delegate = account(accounts, 7)?;
        let tail = &accounts[8..];

        expect_signer(signer_a)?;
        expect_signer(signer_b)?;
        expect_writable(market_ai)?;
        expect_writable(account_a_ai)?;
        expect_writable(account_b_ai)?;
        expect_writable(matcher_ctx)?;
        expect_owner(market_ai, program_id)?;
        expect_owner(account_a_ai, program_id)?;
        expect_owner(account_b_ai, program_id)?;
        if account_a_ai.key == account_b_ai.key
            || !matcher_prog.executable
            || matcher_ctx.owner != matcher_prog.key
            || matcher_ctx.data_len() < constants::MATCHER_CONTEXT_MIN_LEN
            || tail.len() > constants::MAX_MATCHER_TAIL_ACCOUNTS
        {
            return Err(PercolatorError::InvalidInstruction.into());
        }
        for ai in tail {
            if ai.key == market_ai.key
                || ai.key == account_a_ai.key
                || ai.key == account_b_ai.key
                || ai.key == program_id
                || ai.owner == program_id
            {
                return Err(PercolatorError::InvalidInstruction.into());
            }
        }

        let (delegate, bump) = derive_matcher_delegate(
            program_id,
            market_ai.key,
            account_b_ai.key,
            matcher_prog.key,
            matcher_ctx.key,
        );
        expect_key(matcher_delegate, &delegate)?;

        let (cfg_pre, mode_pre, current_slot_pre, oracle_price, max_trading_fee_bps) =
            state::read_market_trade_preflight(
                &market_ai.try_borrow_data()?,
                asset_index as usize,
            )?;
        let (account_a_header, account_a_owner) =
            state::read_portfolio_owner_preflight(&account_a_ai.try_borrow_data()?)?;
        let (account_b_header, account_b_owner) =
            state::read_portfolio_owner_preflight(&account_b_ai.try_borrow_data()?)?;
        if mode_pre != MarketModeV16::Live {
            return Err(PercolatorError::EngineLockActive.into());
        }
        let oracle_profile_pre = read_oracle_profile_for_asset(
            &market_ai.try_borrow_data()?,
            &cfg_pre,
            asset_index as usize,
        )?;
        let stale_matured = if oracle_v16::profile_is_price_managed(&oracle_profile_pre) {
            cfg_pre.permissionless_resolve_stale_slots != 0
                && authenticated_slot_or_fallback(current_slot_pre)
                    .saturating_sub(oracle_profile_pre.last_good_oracle_slot)
                    >= cfg_pre.permissionless_resolve_stale_slots
        } else {
            oracle_v16::permissionless_stale_matured(
                &cfg_pre,
                authenticated_slot_or_fallback(current_slot_pre),
            )
        };
        if stale_matured {
            return Err(PercolatorError::OracleStale.into());
        }
        let fee_floor_pre = core::cmp::max(fee_bps, cfg_pre.trade_fee_base_bps);
        if fee_floor_pre > max_trading_fee_bps {
            return Err(PercolatorError::InvalidInstruction.into());
        }
        if account_a_header.portfolio_account_id != account_a_ai.key.to_bytes()
            || account_b_header.portfolio_account_id != account_b_ai.key.to_bytes()
        {
            return Err(PercolatorError::EngineProvenanceMismatch.into());
        }
        if account_a_owner != signer_a.key.to_bytes() || account_b_owner != signer_b.key.to_bytes()
        {
            return Err(PercolatorError::Unauthorized.into());
        }
        if size_q == 0 || size_q == i128::MIN {
            return Err(PercolatorError::InvalidInstruction.into());
        }
        if oracle_price == 0 {
            return Err(PercolatorError::InvalidInstruction.into());
        }
        let req_id = current_slot_pre.wrapping_add(1);
        let lp_account_id = matcher_lp_account_id(&delegate);

        invoke_matcher(
            matcher_prog,
            matcher_ctx,
            matcher_delegate,
            tail,
            req_id,
            asset_index,
            lp_account_id,
            oracle_price,
            size_q,
            &[
                b"matcher",
                market_ai.key.as_ref(),
                account_b_ai.key.as_ref(),
                matcher_prog.key.as_ref(),
                matcher_ctx.key.as_ref(),
                &[bump],
            ],
        )?;

        let ret = {
            let data = matcher_ctx.try_borrow_data()?;
            matcher_abi::read_matcher_return(&data)?
        };
        matcher_abi::validate_matcher_return(
            &ret,
            lp_account_id,
            asset_index,
            oracle_price,
            size_q,
            req_id,
        )?;
        if limit_price != 0 {
            let limit_ok = if size_q > 0 {
                ret.exec_price_e6 <= limit_price
            } else {
                ret.exec_price_e6 >= limit_price
            };
            if !limit_ok {
                return Err(PercolatorError::InvalidInstruction.into());
            }
        }
        if ret.exec_size == 0 {
            return Ok(());
        }
        let (_, _, max_market_slots, _) =
            state::read_market_config_mode_and_capacity(&market_ai.try_borrow_data()?)?;
        handle_trade_nocpi_zero_copy(
            program_id,
            signer_a,
            signer_b,
            market_ai,
            account_a_ai,
            account_b_ai,
            asset_index,
            ret.exec_size,
            ret.exec_price_e6,
            fee_bps,
            max_market_slots,
        )
    }

    #[inline(never)]
    fn handle_close_portfolio<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
    ) -> ProgramResult {
        let owner = account(accounts, 0)?;
        let market_ai = account(accounts, 1)?;
        let portfolio_ai = account(accounts, 2)?;
        expect_signer(owner)?;
        expect_writable(market_ai)?;
        expect_writable(portfolio_ai)?;
        expect_owner(market_ai, program_id)?;
        expect_owner(portfolio_ai, program_id)?;
        let (_, _, max_market_slots, _) =
            state::read_market_config_mode_and_capacity(&market_ai.try_borrow_data()?)?;
        ensure_portfolio_storage_for_market_slots(portfolio_ai, max_market_slots)?;
        {
            let mut market_data = market_ai.try_borrow_mut_data()?;
            let (_cfg, group) = state::market_view_mut(&mut market_data)?;
            let mut portfolio_data = portfolio_ai.try_borrow_mut_data()?;
            let portfolio =
                state::portfolio_view_mut_for_market_slots(&mut portfolio_data, max_market_slots)?;
            expect_portfolio_view_account_key(&portfolio, portfolio_ai.key)?;
            expect_portfolio_view_owner(&portfolio, owner.key)?;
            portfolio
                .validate_with_market(&group.as_view())
                .map_err(map_v16_error)?;
            if !percolator::active_bitmap_is_empty(
                portfolio
                    .header
                    .active_bitmap
                    .map(percolator::V16PodU64::get),
            ) || portfolio.header.capital.get() != 0
                || portfolio.header.pnl.get() != 0
                || portfolio.header.reserved_pnl.get() != 0
                || portfolio.header.fee_credits.get() != 0
                || portfolio.header.cancel_deposit_escrow.get() != 0
                || portfolio.header.stale_state != 0
                || portfolio.header.b_stale_state != 0
                || portfolio
                    .header
                    .close_progress
                    .try_to_runtime()
                    .map_err(map_v16_error)?
                    .has_pending_residual()
                || (portfolio.header.resolved_payout_receipt.present != 0
                    && portfolio.header.resolved_payout_receipt.finalized == 0)
            {
                return Err(PercolatorError::EngineLockActive.into());
            }
            let mut d = 0usize;
            while d < portfolio.source_domains.len() {
                if portfolio.source_domains[d].source_claim_bound_num.get() != 0 {
                    return Err(PercolatorError::EngineLockActive.into());
                }
                d += 1;
            }
            group.header.materialized_portfolio_count = percolator::V16PodU64::new(
                group
                    .header
                    .materialized_portfolio_count
                    .get()
                    .checked_sub(1)
                    .ok_or(PercolatorError::EngineCounterUnderflow)?,
            );
            group.validate_shape().map_err(map_v16_error)?;
        }
        for b in portfolio_ai.try_borrow_mut_data()?.iter_mut() {
            *b = 0;
        }
        Ok(())
    }

    #[inline(never)]
    fn handle_top_up_insurance<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        amount: u128,
    ) -> ProgramResult {
        let signer = account(accounts, 0)?;
        let market_ai = account(accounts, 1)?;
        let source_token = account(accounts, 2)?;
        let vault_token = account(accounts, 3)?;
        let token_program = account(accounts, 4)?;
        let ledger_ai = accounts.get(5);
        expect_signer(signer)?;
        expect_writable(market_ai)?;
        expect_writable(source_token)?;
        expect_writable(vault_token)?;
        expect_owner(market_ai, program_id)?;
        if let Some(ledger_ai) = ledger_ai {
            expect_writable(ledger_ai)?;
            expect_owner(ledger_ai, program_id)?;
        }
        verify_token_program(token_program)?;
        let (cfg_pre, mode, _, _) =
            state::read_market_config_mode_and_capacity(&market_ai.try_borrow_data()?)?;
        if mode != MarketModeV16::Live {
            return Err(PercolatorError::EngineLockActive.into());
        }
        expect_live_authority(&cfg_pre.insurance_authority, signer.key)?;
        let mint = primary_collateral_mint(&cfg_pre);
        let (vault_authority, _) = derive_vault_authority(program_id, market_ai.key);
        verify_user_token_account(source_token, signer.key, &mint)?;
        verify_vault_token_account(vault_token, &vault_authority, &mint)?;
        let amount_u64 = amount_to_u64(amount)?;
        require_token_balance(source_token, amount_u64)?;
        let mut cfg_after = None;
        {
            let mut market_data = market_ai.try_borrow_mut_data()?;
            let (mut cfg, mut group) = state::market_view_mut(&mut market_data)?;
            if group.header.mode != 0 {
                return Err(PercolatorError::EngineLockActive.into());
            }
            reject_permissionless_resolve_matured_live_view(&cfg, &group)?;
            expect_live_authority(&cfg.insurance_authority, signer.key)?;
            let mut ledger_data = if let Some(ledger_ai) = ledger_ai {
                Some(ledger_ai.try_borrow_mut_data()?)
            } else {
                None
            };
            let mut ledger_state = if let Some(data) = ledger_data.as_deref() {
                let (mut ledger, initialized) = read_or_new_insurance_ledger(
                    data,
                    market_ai.key.to_bytes(),
                    cfg.insurance_authority,
                    market_insurance_remaining_view(&group, 0)?,
                )?;
                sync_insurance_ledger(&mut ledger, market_insurance_remaining_view(&group, 0)?)?;
                Some((ledger, initialized))
            } else {
                None
            };
            group.header.insurance = percolator::V16PodU128::new(
                group
                    .header
                    .insurance
                    .get()
                    .checked_add(amount)
                    .ok_or(PercolatorError::EngineArithmeticOverflow)?,
            );
            group.header.vault = percolator::V16PodU128::new(
                group
                    .header
                    .vault
                    .get()
                    .checked_add(amount)
                    .ok_or(PercolatorError::EngineArithmeticOverflow)?,
            );
            credit_market_insurance_budget_view(&mut group, 0, amount)?;
            if let Some((ledger, _)) = ledger_state.as_mut() {
                ledger.total_principal_atoms = ledger
                    .total_principal_atoms
                    .checked_add(amount)
                    .ok_or(PercolatorError::EngineArithmeticOverflow)?;
                ledger.total_deposited_atoms = ledger
                    .total_deposited_atoms
                    .checked_add(amount)
                    .ok_or(PercolatorError::EngineArithmeticOverflow)?;
                ledger.last_observed_insurance_atoms = ledger
                    .last_observed_insurance_atoms
                    .checked_add(amount)
                    .ok_or(PercolatorError::EngineArithmeticOverflow)?;
            }
            if cfg.insurance_withdraw_deposits_only != 0 {
                cfg.insurance_withdraw_deposit_remaining = cfg
                    .insurance_withdraw_deposit_remaining
                    .checked_add(amount)
                    .ok_or(PercolatorError::EngineArithmeticOverflow)?;
                cfg_after = Some(cfg);
            }
            group.validate_shape().map_err(map_v16_error)?;
            if let (Some(data), Some((ledger, initialized))) =
                (ledger_data.as_deref_mut(), ledger_state.as_ref())
            {
                write_or_init_insurance_ledger(data, ledger, *initialized)?;
            }
        }
        transfer_tokens(token_program, source_token, vault_token, signer, amount_u64)?;
        if let Some(cfg) = cfg_after {
            state::write_wrapper_config(&mut market_ai.try_borrow_mut_data()?, &cfg)?;
        }
        Ok(())
    }

    #[inline(never)]
    fn handle_top_up_insurance_domain<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        domain: u8,
        amount: u128,
    ) -> ProgramResult {
        let signer = account(accounts, 0)?;
        let market_ai = account(accounts, 1)?;
        let source_token = account(accounts, 2)?;
        let vault_token = account(accounts, 3)?;
        let token_program = account(accounts, 4)?;
        let ledger_ai = accounts.get(5);
        expect_signer(signer)?;
        expect_writable(market_ai)?;
        expect_writable(source_token)?;
        expect_writable(vault_token)?;
        expect_owner(market_ai, program_id)?;
        if let Some(ledger_ai) = ledger_ai {
            expect_writable(ledger_ai)?;
            expect_owner(ledger_ai, program_id)?;
        }
        verify_token_program(token_program)?;
        let domain = domain as usize;
        let (cfg_pre, authorities) = {
            let market_data = market_ai.try_borrow_data()?;
            let (cfg, mode, configured_slots, _) =
                state::read_market_config_mode_and_capacity(&market_data)?;
            let asset_index = domain / 2;
            if mode != MarketModeV16::Live
                || domain >= configured_slots.saturating_mul(2)
                || asset_index >= configured_slots
            {
                return Err(PercolatorError::InvalidInstruction.into());
            }
            let profile = read_oracle_profile_for_asset(&market_data, &cfg, asset_index)?;
            let authorities = domain_authorities_from_profile(&cfg, &profile, asset_index);
            (cfg, authorities)
        };
        expect_live_authority(&authorities.insurance_authority, signer.key)?;
        let mint = primary_collateral_mint(&cfg_pre);
        let (vault_authority, _) = derive_vault_authority(program_id, market_ai.key);
        verify_user_token_account(source_token, signer.key, &mint)?;
        verify_vault_token_account(vault_token, &vault_authority, &mint)?;
        let amount_u64 = amount_to_u64(amount)?;
        require_token_balance(source_token, amount_u64)?;
        {
            let mut market_data = market_ai.try_borrow_mut_data()?;
            let (cfg, mut group) = state::market_view_mut(&mut market_data)?;
            if group.header.mode != 0 {
                return Err(PercolatorError::EngineLockActive.into());
            }
            reject_permissionless_resolve_matured_live_view(&cfg, &group)?;
            let authorities = domain_authorities_from_view(&group, &cfg, domain)?;
            expect_live_authority(&authorities.insurance_authority, signer.key)?;
            let mut ledger_data = if let Some(ledger_ai) = ledger_ai {
                Some(ledger_ai.try_borrow_mut_data()?)
            } else {
                None
            };
            let mut ledger_state = if let Some(data) = ledger_data.as_deref() {
                let observed = domain_budget_remaining_view(&group, domain)?;
                let (mut ledger, initialized) = read_or_new_insurance_ledger(
                    data,
                    market_ai.key.to_bytes(),
                    authorities.insurance_authority,
                    observed,
                )?;
                sync_insurance_ledger(&mut ledger, observed)?;
                Some((ledger, initialized))
            } else {
                None
            };
            group.header.insurance = percolator::V16PodU128::new(
                group
                    .header
                    .insurance
                    .get()
                    .checked_add(amount)
                    .ok_or(PercolatorError::EngineArithmeticOverflow)?,
            );
            group.header.vault = percolator::V16PodU128::new(
                group
                    .header
                    .vault
                    .get()
                    .checked_add(amount)
                    .ok_or(PercolatorError::EngineArithmeticOverflow)?,
            );
            add_to_domain_budget_view(&mut group, domain, amount)?;
            if let Some((ledger, _)) = ledger_state.as_mut() {
                ledger.total_principal_atoms = ledger
                    .total_principal_atoms
                    .checked_add(amount)
                    .ok_or(PercolatorError::EngineArithmeticOverflow)?;
                ledger.total_deposited_atoms = ledger
                    .total_deposited_atoms
                    .checked_add(amount)
                    .ok_or(PercolatorError::EngineArithmeticOverflow)?;
                ledger.last_observed_insurance_atoms = ledger
                    .last_observed_insurance_atoms
                    .checked_add(amount)
                    .ok_or(PercolatorError::EngineArithmeticOverflow)?;
            }
            group.validate_shape().map_err(map_v16_error)?;
            if let (Some(data), Some((ledger, initialized))) =
                (ledger_data.as_deref_mut(), ledger_state.as_ref())
            {
                write_or_init_insurance_ledger(data, ledger, *initialized)?;
            }
        }
        transfer_tokens(token_program, source_token, vault_token, signer, amount_u64)?;
        Ok(())
    }

    fn source_credit_available_backing_num(state: SourceCreditStateV16) -> Result<u128, V16Error> {
        if state.fresh_reserved_backing_num < state.valid_liened_backing_num
            || state.spent_backing_num < state.provider_receivable_num
        {
            return Err(V16Error::InvalidConfig);
        }
        let insurance_encumbered = state
            .valid_liened_insurance_num
            .checked_add(state.impaired_liened_insurance_num)
            .ok_or(V16Error::ArithmeticOverflow)?;
        if state.insurance_credit_reserved_num < insurance_encumbered {
            return Err(V16Error::InvalidConfig);
        }
        state
            .fresh_reserved_backing_num
            .checked_sub(state.valid_liened_backing_num)
            .and_then(|v| v.checked_add(state.insurance_credit_reserved_num - insurance_encumbered))
            .ok_or(V16Error::ArithmeticOverflow)
    }

    fn expected_source_credit_rate_num(state: SourceCreditStateV16) -> Result<u128, V16Error> {
        if state.exact_positive_claim_num > state.positive_claim_bound_num
            || state.credit_rate_num > percolator::CREDIT_RATE_SCALE
        {
            return Err(V16Error::InvalidConfig);
        }
        if state.positive_claim_bound_num == 0 {
            source_credit_available_backing_num(state)?;
            return Ok(percolator::CREDIT_RATE_SCALE);
        }
        let available = source_credit_available_backing_num(state)?;
        let rate = percolator::wide_math::U256::from_u128(available)
            .checked_mul(percolator::wide_math::U256::from_u128(
                percolator::CREDIT_RATE_SCALE,
            ))
            .and_then(|v| {
                v.checked_div(percolator::wide_math::U256::from_u128(
                    state.positive_claim_bound_num,
                ))
            })
            .and_then(|v| v.try_into_u128())
            .ok_or(V16Error::ArithmeticOverflow)?;
        Ok(core::cmp::min(rate, percolator::CREDIT_RATE_SCALE))
    }

    fn add_fresh_counterparty_backing_view(
        group: &mut state::MarketViewMutV16<'_>,
        domain: usize,
        amount_num: u128,
        expiry_slot: u64,
    ) -> ProgramResult {
        let max_markets = group.header.config.max_market_slots.get() as usize;
        let asset_index = domain / 2;
        if domain >= max_markets.saturating_mul(2)
            || asset_index >= group.markets.len()
            || amount_num == 0
            || expiry_slot <= group.header.current_slot.get()
        {
            return Err(PercolatorError::InvalidInstruction.into());
        }
        let slot = &mut group.markets[asset_index].engine;
        let (source_acc, bucket_acc) = if domain % 2 == 0 {
            (&mut slot.source_credit_long, &mut slot.backing_long)
        } else {
            (&mut slot.source_credit_short, &mut slot.backing_short)
        };
        let mut source = source_acc.try_to_runtime().map_err(map_v16_error)?;
        let mut bucket = bucket_acc.try_to_runtime().map_err(map_v16_error)?;
        if source.provider_receivable_num != bucket.consumed_liened_backing_num
            || source.spent_backing_num < source.provider_receivable_num
        {
            return Err(PercolatorError::EngineInvalidConfig.into());
        }
        match bucket.status {
            BackingBucketStatusV16::Empty | BackingBucketStatusV16::Expired => {
                bucket.status = BackingBucketStatusV16::Fresh;
                bucket.expiry_slot = expiry_slot;
            }
            BackingBucketStatusV16::Fresh if bucket.expiry_slot == expiry_slot => {}
            _ => return Err(PercolatorError::EngineLockActive.into()),
        }
        let refill = core::cmp::min(amount_num, source.provider_receivable_num);
        if refill > bucket.consumed_liened_backing_num {
            return Err(PercolatorError::EngineCounterUnderflow.into());
        }
        bucket.consumed_liened_backing_num -= refill;
        source.provider_receivable_num -= refill;
        bucket.fresh_unliened_backing_num = bucket
            .fresh_unliened_backing_num
            .checked_add(amount_num)
            .ok_or(PercolatorError::EngineCounterOverflow)?;
        source.fresh_reserved_backing_num = source
            .fresh_reserved_backing_num
            .checked_add(amount_num)
            .ok_or(PercolatorError::EngineCounterOverflow)?;
        source.credit_rate_num = expected_source_credit_rate_num(source).map_err(map_v16_error)?;
        source.credit_epoch = source
            .credit_epoch
            .checked_add(1)
            .ok_or(PercolatorError::EngineCounterOverflow)?;
        group.header.risk_epoch = percolator::V16PodU64::new(
            group
                .header
                .risk_epoch
                .get()
                .checked_add(1)
                .ok_or(PercolatorError::EngineCounterOverflow)?,
        );
        *source_acc = percolator::SourceCreditStateV16Account::from_runtime(&source);
        *bucket_acc = percolator::BackingBucketV16Account::from_runtime(&bucket);
        Ok(())
    }

    fn backing_domain_parts_view(
        group: &state::MarketViewMutV16<'_>,
        domain: usize,
    ) -> Result<(SourceCreditStateV16, percolator::BackingBucketV16), ProgramError> {
        let max_markets = group.header.config.max_market_slots.get() as usize;
        let asset_index = domain / 2;
        if domain >= max_markets.saturating_mul(2) || asset_index >= group.markets.len() {
            return Err(PercolatorError::InvalidInstruction.into());
        }
        let slot = &group.markets[asset_index].engine;
        let (source_acc, bucket_acc) = if domain % 2 == 0 {
            (&slot.source_credit_long, &slot.backing_long)
        } else {
            (&slot.source_credit_short, &slot.backing_short)
        };
        Ok((
            source_acc.try_to_runtime().map_err(map_v16_error)?,
            bucket_acc.try_to_runtime().map_err(map_v16_error)?,
        ))
    }

    fn backing_unavailable_principal_atoms(
        bucket: &percolator::BackingBucketV16,
    ) -> Result<u128, ProgramError> {
        bucket
            .consumed_liened_backing_num
            .checked_add(bucket.impaired_liened_backing_num)
            .map(|v| v / BOUND_SCALE)
            .ok_or(PercolatorError::EngineArithmeticOverflow.into())
    }

    fn sync_backing_domain_ledger(
        ledger: &mut state::BackingDomainLedgerAccountV16,
        bucket: &percolator::BackingBucketV16,
    ) -> ProgramResult {
        let bucket_earnings_atoms = bucket.utilization_fee_earnings;
        if bucket_earnings_atoms >= ledger.last_observed_bucket_earnings_atoms {
            ledger.total_earnings_atoms = ledger
                .total_earnings_atoms
                .checked_add(bucket_earnings_atoms - ledger.last_observed_bucket_earnings_atoms)
                .ok_or(PercolatorError::EngineArithmeticOverflow)?;
        }
        ledger.last_observed_bucket_earnings_atoms = bucket_earnings_atoms;

        let unavailable_atoms = backing_unavailable_principal_atoms(bucket)?;
        if unavailable_atoms >= ledger.last_observed_unavailable_principal_atoms {
            ledger.cumulative_loss_atoms = ledger
                .cumulative_loss_atoms
                .checked_add(unavailable_atoms - ledger.last_observed_unavailable_principal_atoms)
                .ok_or(PercolatorError::EngineArithmeticOverflow)?;
        } else {
            ledger.cumulative_recovery_atoms = ledger
                .cumulative_recovery_atoms
                .checked_add(ledger.last_observed_unavailable_principal_atoms - unavailable_atoms)
                .ok_or(PercolatorError::EngineArithmeticOverflow)?;
        }
        ledger.last_observed_unavailable_principal_atoms = unavailable_atoms;
        Ok(())
    }

    fn sync_insurance_ledger(
        ledger: &mut state::InsuranceLedgerAccountV16,
        insurance_atoms: u128,
    ) -> ProgramResult {
        if insurance_atoms >= ledger.last_observed_insurance_atoms {
            ledger.cumulative_profit_atoms = ledger
                .cumulative_profit_atoms
                .checked_add(insurance_atoms - ledger.last_observed_insurance_atoms)
                .ok_or(PercolatorError::EngineArithmeticOverflow)?;
        } else {
            ledger.cumulative_loss_atoms = ledger
                .cumulative_loss_atoms
                .checked_add(ledger.last_observed_insurance_atoms - insurance_atoms)
                .ok_or(PercolatorError::EngineArithmeticOverflow)?;
        }
        ledger.last_observed_insurance_atoms = insurance_atoms;
        Ok(())
    }

    fn read_or_new_backing_domain_ledger(
        data: &[u8],
        market_group: [u8; 32],
        authority: [u8; 32],
        domain: u16,
        bucket: &percolator::BackingBucketV16,
    ) -> Result<(state::BackingDomainLedgerAccountV16, bool), ProgramError> {
        if state::is_initialized(data) {
            let ledger = state::read_backing_domain_ledger(data)?;
            if ledger.market_group != market_group
                || ledger.authority != authority
                || ledger.domain != domain
            {
                return Err(PercolatorError::Unauthorized.into());
            }
            Ok((ledger, true))
        } else {
            Ok((
                state::BackingDomainLedgerAccountV16 {
                    market_group,
                    authority,
                    total_principal_atoms: 0,
                    total_deposited_atoms: 0,
                    total_principal_withdrawn_atoms: 0,
                    total_earnings_atoms: 0,
                    total_earnings_withdrawn_atoms: 0,
                    last_observed_bucket_earnings_atoms: bucket.utilization_fee_earnings,
                    cumulative_loss_atoms: 0,
                    cumulative_recovery_atoms: 0,
                    last_observed_unavailable_principal_atoms: backing_unavailable_principal_atoms(
                        bucket,
                    )?,
                    domain,
                    _padding: [0u8; 14],
                },
                false,
            ))
        }
    }

    fn write_or_init_backing_domain_ledger(
        data: &mut [u8],
        ledger: &state::BackingDomainLedgerAccountV16,
        initialized: bool,
    ) -> ProgramResult {
        if initialized {
            state::write_backing_domain_ledger(data, ledger)
        } else {
            state::init_backing_domain_ledger(data, ledger)
        }
    }

    fn read_or_new_insurance_ledger(
        data: &[u8],
        market_group: [u8; 32],
        authority: [u8; 32],
        insurance_atoms: u128,
    ) -> Result<(state::InsuranceLedgerAccountV16, bool), ProgramError> {
        if state::is_initialized(data) {
            let ledger = state::read_insurance_ledger(data)?;
            if ledger.market_group != market_group || ledger.authority != authority {
                return Err(PercolatorError::Unauthorized.into());
            }
            Ok((ledger, true))
        } else {
            Ok((
                state::InsuranceLedgerAccountV16 {
                    market_group,
                    authority,
                    total_principal_atoms: 0,
                    total_deposited_atoms: 0,
                    total_withdrawn_atoms: 0,
                    cumulative_profit_atoms: 0,
                    cumulative_loss_atoms: 0,
                    last_observed_insurance_atoms: insurance_atoms,
                },
                false,
            ))
        }
    }

    fn write_or_init_insurance_ledger(
        data: &mut [u8],
        ledger: &state::InsuranceLedgerAccountV16,
        initialized: bool,
    ) -> ProgramResult {
        if initialized {
            state::write_insurance_ledger(data, ledger)
        } else {
            state::init_insurance_ledger(data, ledger)
        }
    }

    const DOMAIN_WITHDRAW_AUTH_INSURANCE: u8 = 0;
    const DOMAIN_WITHDRAW_AUTH_BACKING: u8 = 1;

    #[inline(never)]
    fn verify_domain_withdrawal_preflight<'a>(
        program_id: &Pubkey,
        market_ai: &AccountInfo<'a>,
        authority: &AccountInfo<'a>,
        dest_token: &AccountInfo<'a>,
        vault_token: &AccountInfo<'a>,
        vault_authority_ai: &AccountInfo<'a>,
        domain: usize,
        amount: u128,
        require_live_mode: bool,
        authority_kind: u8,
    ) -> Result<(u8, u64), ProgramError> {
        let market_data = market_ai.try_borrow_data()?;
        let (cfg, mode, configured_slots, _) =
            state::read_market_config_mode_and_capacity(&market_data)?;
        let asset_index = domain / 2;
        if (require_live_mode && mode != MarketModeV16::Live)
            || domain >= configured_slots.saturating_mul(2)
            || asset_index >= configured_slots
        {
            return Err(PercolatorError::InvalidInstruction.into());
        }
        let profile = read_oracle_profile_for_asset(&market_data, &cfg, asset_index)?;
        let authorities = domain_authorities_from_profile(&cfg, &profile, asset_index);
        let local_authorized = match authority_kind {
            DOMAIN_WITHDRAW_AUTH_INSURANCE => {
                live_authority_matches(&authorities.insurance_operator, authority.key)
            }
            DOMAIN_WITHDRAW_AUTH_BACKING => {
                live_authority_matches(&authorities.backing_bucket_authority, authority.key)
            }
            _ => return Err(PercolatorError::InvalidInstruction.into()),
        };
        if !local_authorized && !live_authority_matches(&cfg.admin, authority.key) {
            return Err(PercolatorError::Unauthorized.into());
        }
        let (vault_authority, bump) = derive_vault_authority(program_id, market_ai.key);
        expect_key(vault_authority_ai, &vault_authority)?;
        verify_withdrawable_token_accounts(
            dest_token,
            authority.key,
            vault_token,
            &vault_authority,
            &cfg,
        )?;
        let amount_u64 = amount_to_u64(amount)?;
        require_token_balance(vault_token, amount_u64)?;
        Ok((bump, amount_u64))
    }

    #[inline(never)]
    // AUDIT MIRROR: the backing-bucket + BackingDomainLedger update sequence in
    // this handler (add_fresh_counterparty_backing_view + total_principal/
    // total_deposited increment + vault increment + validate_shape +
    // write_or_init_backing_domain_ledger) is mirrored inside
    // `handle_deposit_to_lp_vault` (LP Vault Phase D, Option A). Change BOTH
    // together. The differential test `lp_deposit_backing_state_matches_top_up`
    // fails mechanically if the two sequences ever drift.
    fn handle_top_up_backing_bucket<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        domain: u8,
        amount: u128,
        expiry_slot: u64,
    ) -> ProgramResult {
        let signer = account(accounts, 0)?;
        let market_ai = account(accounts, 1)?;
        let source_token = account(accounts, 2)?;
        let vault_token = account(accounts, 3)?;
        let token_program = account(accounts, 4)?;
        let ledger_ai = accounts.get(5);
        expect_signer(signer)?;
        expect_writable(market_ai)?;
        expect_writable(source_token)?;
        expect_writable(vault_token)?;
        expect_owner(market_ai, program_id)?;
        if let Some(ledger_ai) = ledger_ai {
            expect_writable(ledger_ai)?;
            expect_owner(ledger_ai, program_id)?;
        }
        verify_token_program(token_program)?;
        let domain_usize = domain as usize;
        let (cfg_pre, authorities) = {
            let market_data = market_ai.try_borrow_data()?;
            let (cfg, mode, configured_slots, _) =
                state::read_market_config_mode_and_capacity(&market_data)?;
            let asset_index = domain_usize / 2;
            if mode != MarketModeV16::Live
                || domain_usize >= configured_slots.saturating_mul(2)
                || asset_index >= configured_slots
            {
                return Err(PercolatorError::EngineLockActive.into());
            }
            let profile = read_oracle_profile_for_asset(&market_data, &cfg, asset_index)?;
            let authorities = domain_authorities_from_profile(&cfg, &profile, asset_index);
            (cfg, authorities)
        };
        expect_live_authority(&authorities.backing_bucket_authority, signer.key)?;
        let mint = primary_collateral_mint(&cfg_pre);
        let (vault_authority, _) = derive_vault_authority(program_id, market_ai.key);
        verify_user_token_account(source_token, signer.key, &mint)?;
        verify_vault_token_account(vault_token, &vault_authority, &mint)?;
        let amount_u64 = amount_to_u64(amount)?;
        require_token_balance(source_token, amount_u64)?;
        if amount != 0 {
            let backing_num = amount
                .checked_mul(BOUND_SCALE)
                .ok_or(PercolatorError::EngineArithmeticOverflow)?;
            let mut market_data = market_ai.try_borrow_mut_data()?;
            let (cfg, mut group) = state::market_view_mut(&mut market_data)?;
            if group.header.mode != 0 {
                return Err(PercolatorError::EngineLockActive.into());
            }
            reject_permissionless_resolve_matured_live_view(&cfg, &group)?;
            let authorities = domain_authorities_from_view(&group, &cfg, domain_usize)?;
            expect_live_authority(&authorities.backing_bucket_authority, signer.key)?;
            let mut ledger_data = if let Some(ledger_ai) = ledger_ai {
                Some(ledger_ai.try_borrow_mut_data()?)
            } else {
                None
            };
            let mut ledger_state = if let Some(data) = ledger_data.as_deref() {
                let (_, bucket) = backing_domain_parts_view(&group, domain as usize)?;
                let (mut ledger, initialized) = read_or_new_backing_domain_ledger(
                    data,
                    market_ai.key.to_bytes(),
                    authorities.backing_bucket_authority,
                    domain as u16,
                    &bucket,
                )?;
                sync_backing_domain_ledger(&mut ledger, &bucket)?;
                Some((ledger, initialized))
            } else {
                None
            };
            let next_vault = group
                .header
                .vault
                .get()
                .checked_add(amount)
                .ok_or(PercolatorError::EngineArithmeticOverflow)?;
            add_fresh_counterparty_backing_view(
                &mut group,
                domain_usize,
                backing_num,
                expiry_slot,
            )?;
            if let Some((ledger, _)) = ledger_state.as_mut() {
                ledger.total_principal_atoms = ledger
                    .total_principal_atoms
                    .checked_add(amount)
                    .ok_or(PercolatorError::EngineArithmeticOverflow)?;
                ledger.total_deposited_atoms = ledger
                    .total_deposited_atoms
                    .checked_add(amount)
                    .ok_or(PercolatorError::EngineArithmeticOverflow)?;
            }
            group.header.vault = percolator::V16PodU128::new(next_vault);
            group.validate_shape().map_err(map_v16_error)?;
            if let (Some(data), Some((ledger, initialized))) =
                (ledger_data.as_deref_mut(), ledger_state.as_ref())
            {
                write_or_init_backing_domain_ledger(data, ledger, *initialized)?;
            }
        }
        transfer_tokens(token_program, source_token, vault_token, signer, amount_u64)?;
        Ok(())
    }

    #[inline(never)]
    // AUDIT MIRROR: the backing-bucket + source + BackingDomainLedger decrement
    // sequence in this handler (bucket.fresh_unliened -= / status transition,
    // source.fresh_reserved -= / credit_epoch++, risk_epoch++, vault -=,
    // ledger.total_principal -= / total_principal_withdrawn +=, validate_shape,
    // vault_authority-signed transfer to dest) is mirrored inside
    // `handle_execute_redemption` (LP Vault Phase E, Option A). Change BOTH
    // together. The differential test
    // `lp_execute_redemption_backing_state_matches_withdraw` fails mechanically
    // if the two sequences ever drift.
    fn handle_withdraw_backing_bucket<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        domain: u8,
        amount: u128,
    ) -> ProgramResult {
        let authority = account(accounts, 0)?;
        let market_ai = account(accounts, 1)?;
        let dest_token = account(accounts, 2)?;
        let vault_token = account(accounts, 3)?;
        let vault_authority_ai = account(accounts, 4)?;
        let token_program = account(accounts, 5)?;
        let ledger_ai = accounts.get(6);
        expect_signer(authority)?;
        expect_writable(market_ai)?;
        expect_writable(dest_token)?;
        expect_writable(vault_token)?;
        expect_owner(market_ai, program_id)?;
        if let Some(ledger_ai) = ledger_ai {
            expect_writable(ledger_ai)?;
            expect_owner(ledger_ai, program_id)?;
        }
        verify_token_program(token_program)?;
        if amount == 0 {
            return Err(PercolatorError::InvalidInstruction.into());
        }

        let domain = domain as usize;
        let (bump, amount_u64) = verify_domain_withdrawal_preflight(
            program_id,
            market_ai,
            authority,
            dest_token,
            vault_token,
            vault_authority_ai,
            domain,
            amount,
            false,
            DOMAIN_WITHDRAW_AUTH_BACKING,
        )?;

        let backing_num = amount
            .checked_mul(BOUND_SCALE)
            .ok_or(PercolatorError::EngineArithmeticOverflow)?;
        {
            let mut market_data = market_ai.try_borrow_mut_data()?;
            let (cfg, group) = state::market_view_mut(&mut market_data)?;
            let authorities = domain_authorities_from_view(&group, &cfg, domain)?;
            let shutdown_drain = match group.header.mode {
                0 => live_domain_withdraw_health_or_shutdown_view(&cfg, &group, domain)?,
                1 => {
                    if group.header.materialized_portfolio_count.get() != 0
                        || group.header.c_tot.get() != 0
                    {
                        return Err(PercolatorError::EngineLockActive.into());
                    }
                    false
                }
                _ => return Err(PercolatorError::EngineLockActive.into()),
            };
            let local_authorized =
                live_authority_matches(&authorities.backing_bucket_authority, authority.key);
            let admin_shutdown_authorized =
                shutdown_drain && live_authority_matches(&cfg.admin, authority.key);
            if !local_authorized && !admin_shutdown_authorized {
                return Err(PercolatorError::Unauthorized.into());
            }
            let ledger_authority = if admin_shutdown_authorized && !local_authorized {
                cfg.admin
            } else {
                authorities.backing_bucket_authority
            };

            let asset_index = domain / 2;
            if asset_index >= group.markets.len()
                || asset_index >= group.header.config.max_market_slots.get() as usize
            {
                return Err(PercolatorError::InvalidInstruction.into());
            }
            let (source_acc, bucket_acc) = if domain % 2 == 0 {
                (
                    &mut group.markets[asset_index].engine.source_credit_long,
                    &mut group.markets[asset_index].engine.backing_long,
                )
            } else {
                (
                    &mut group.markets[asset_index].engine.source_credit_short,
                    &mut group.markets[asset_index].engine.backing_short,
                )
            };
            let mut source = source_acc.try_to_runtime().map_err(map_v16_error)?;
            let mut bucket = bucket_acc.try_to_runtime().map_err(map_v16_error)?;
            let mut ledger_data = if let Some(ledger_ai) = ledger_ai {
                Some(ledger_ai.try_borrow_mut_data()?)
            } else {
                None
            };
            let mut ledger_state = if let Some(data) = ledger_data.as_deref() {
                let (mut ledger, initialized) = read_or_new_backing_domain_ledger(
                    data,
                    market_ai.key.to_bytes(),
                    ledger_authority,
                    domain as u16,
                    &bucket,
                )?;
                sync_backing_domain_ledger(&mut ledger, &bucket)?;
                if amount > ledger.total_principal_atoms {
                    return Err(PercolatorError::EngineCounterUnderflow.into());
                }
                Some((ledger, initialized))
            } else {
                None
            };
            if source.positive_claim_bound_num != 0
                || source.exact_positive_claim_num != 0
                || bucket.status != BackingBucketStatusV16::Fresh
                || bucket.fresh_unliened_backing_num < backing_num
                || source.fresh_reserved_backing_num < backing_num
                || amount > group.header.vault.get()
            {
                return Err(PercolatorError::EngineLockActive.into());
            }
            bucket.fresh_unliened_backing_num = bucket
                .fresh_unliened_backing_num
                .checked_sub(backing_num)
                .ok_or(PercolatorError::EngineCounterUnderflow)?;
            if bucket.fresh_unliened_backing_num == 0 && bucket.valid_liened_backing_num == 0 {
                if bucket.impaired_liened_backing_num != 0 {
                    bucket.status = BackingBucketStatusV16::Impaired;
                } else if bucket.consumed_liened_backing_num != 0 {
                    bucket.status = BackingBucketStatusV16::Expired;
                } else {
                    bucket.status = BackingBucketStatusV16::Empty;
                    bucket.expiry_slot = 0;
                }
            }
            source.fresh_reserved_backing_num = source
                .fresh_reserved_backing_num
                .checked_sub(backing_num)
                .ok_or(PercolatorError::EngineCounterUnderflow)?;
            source.credit_rate_num = percolator::CREDIT_RATE_SCALE;
            source.credit_epoch = source
                .credit_epoch
                .checked_add(1)
                .ok_or(PercolatorError::EngineArithmeticOverflow)?;
            *source_acc = percolator::SourceCreditStateV16Account::from_runtime(&source);
            *bucket_acc = percolator::BackingBucketV16Account::from_runtime(&bucket);
            group.header.risk_epoch = percolator::V16PodU64::new(
                group
                    .header
                    .risk_epoch
                    .get()
                    .checked_add(1)
                    .ok_or(PercolatorError::EngineArithmeticOverflow)?,
            );
            group.header.vault = percolator::V16PodU128::new(
                group
                    .header
                    .vault
                    .get()
                    .checked_sub(amount)
                    .ok_or(PercolatorError::EngineCounterUnderflow)?,
            );
            if let Some((ledger, _)) = ledger_state.as_mut() {
                ledger.total_principal_atoms = ledger
                    .total_principal_atoms
                    .checked_sub(amount)
                    .ok_or(PercolatorError::EngineCounterUnderflow)?;
                ledger.total_principal_withdrawn_atoms = ledger
                    .total_principal_withdrawn_atoms
                    .checked_add(amount)
                    .ok_or(PercolatorError::EngineArithmeticOverflow)?;
            }
            group.validate_shape().map_err(map_v16_error)?;
            if let (Some(data), Some((ledger, initialized))) =
                (ledger_data.as_deref_mut(), ledger_state.as_ref())
            {
                write_or_init_backing_domain_ledger(data, ledger, *initialized)?;
            }
        }

        let bump_arr = [bump];
        let signer_seeds: &[&[&[u8]]] = &[&[b"vault", market_ai.key.as_ref(), &bump_arr]];
        transfer_tokens_signed(
            token_program,
            vault_token,
            dest_token,
            vault_authority_ai,
            amount_u64,
            signer_seeds,
        )?;
        Ok(())
    }

    #[inline(never)]
    fn handle_withdraw_backing_bucket_earnings<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        domain: u8,
        amount: u128,
    ) -> ProgramResult {
        let authority = account(accounts, 0)?;
        let market_ai = account(accounts, 1)?;
        let ledger_ai = account(accounts, 2)?;
        let dest_token = account(accounts, 3)?;
        let vault_token = account(accounts, 4)?;
        let vault_authority_ai = account(accounts, 5)?;
        let token_program = account(accounts, 6)?;
        expect_signer(authority)?;
        expect_writable(market_ai)?;
        expect_writable(ledger_ai)?;
        expect_writable(dest_token)?;
        expect_writable(vault_token)?;
        expect_owner(market_ai, program_id)?;
        expect_owner(ledger_ai, program_id)?;
        verify_token_program(token_program)?;
        if amount == 0 {
            return Err(PercolatorError::InvalidInstruction.into());
        }

        let domain_usize = domain as usize;
        let (bump, amount_u64) = verify_domain_withdrawal_preflight(
            program_id,
            market_ai,
            authority,
            dest_token,
            vault_token,
            vault_authority_ai,
            domain_usize,
            amount,
            false,
            DOMAIN_WITHDRAW_AUTH_BACKING,
        )?;

        {
            let mut market_data = market_ai.try_borrow_mut_data()?;
            let (cfg, mut group) = state::market_view_mut(&mut market_data)?;
            let authorities = domain_authorities_from_view(&group, &cfg, domain_usize)?;
            let shutdown_drain = match group.header.mode {
                0 => live_domain_withdraw_health_or_shutdown_view(&cfg, &group, domain_usize)?,
                1 => {
                    if group.header.materialized_portfolio_count.get() != 0
                        || group.header.c_tot.get() != 0
                    {
                        return Err(PercolatorError::EngineLockActive.into());
                    }
                    false
                }
                _ => return Err(PercolatorError::EngineLockActive.into()),
            };
            let local_authorized =
                live_authority_matches(&authorities.backing_bucket_authority, authority.key);
            let admin_shutdown_authorized =
                shutdown_drain && live_authority_matches(&cfg.admin, authority.key);
            if !local_authorized && !admin_shutdown_authorized {
                return Err(PercolatorError::Unauthorized.into());
            }
            let ledger_authority = if admin_shutdown_authorized && !local_authorized {
                cfg.admin
            } else {
                authorities.backing_bucket_authority
            };

            let (_, bucket) = backing_domain_parts_view(&group, domain_usize)?;
            if amount > bucket.utilization_fee_earnings || amount > group.header.vault.get() {
                return Err(PercolatorError::EngineLockActive.into());
            }
            let mut ledger_data = ledger_ai.try_borrow_mut_data()?;
            let (mut ledger, initialized) = read_or_new_backing_domain_ledger(
                &ledger_data,
                market_ai.key.to_bytes(),
                ledger_authority,
                domain as u16,
                &bucket,
            )?;
            sync_backing_domain_ledger(&mut ledger, &bucket)?;
            group
                .withdraw_backing_provider_earnings_not_atomic(domain_usize, amount)
                .map_err(map_v16_error)?;
            ledger.last_observed_bucket_earnings_atoms = ledger
                .last_observed_bucket_earnings_atoms
                .checked_sub(amount)
                .ok_or(PercolatorError::EngineCounterUnderflow)?;
            ledger.total_earnings_withdrawn_atoms = ledger
                .total_earnings_withdrawn_atoms
                .checked_add(amount)
                .ok_or(PercolatorError::EngineArithmeticOverflow)?;
            group.validate_shape().map_err(map_v16_error)?;
            write_or_init_backing_domain_ledger(&mut ledger_data, &ledger, initialized)?;
        }

        let bump_arr = [bump];
        let signer_seeds: &[&[&[u8]]] = &[&[b"vault", market_ai.key.as_ref(), &bump_arr]];
        transfer_tokens_signed(
            token_program,
            vault_token,
            dest_token,
            vault_authority_ai,
            amount_u64,
            signer_seeds,
        )?;
        Ok(())
    }

    #[inline(never)]
    fn handle_sync_backing_domain_ledger<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        domain: u8,
    ) -> ProgramResult {
        let authority = account(accounts, 0)?;
        let market_ai = account(accounts, 1)?;
        let ledger_ai = account(accounts, 2)?;
        expect_signer(authority)?;
        expect_writable(market_ai)?;
        expect_writable(ledger_ai)?;
        expect_owner(market_ai, program_id)?;
        expect_owner(ledger_ai, program_id)?;

        let mut market_data = market_ai.try_borrow_mut_data()?;
        let (cfg, group) = state::market_view_mut(&mut market_data)?;
        let domain_usize = domain as usize;
        let authorities = domain_authorities_from_view(&group, &cfg, domain_usize)?;
        expect_live_authority(&authorities.backing_bucket_authority, authority.key)?;
        let (_, bucket) = backing_domain_parts_view(&group, domain_usize)?;
        let mut ledger_data = ledger_ai.try_borrow_mut_data()?;
        let (mut ledger, initialized) = read_or_new_backing_domain_ledger(
            &ledger_data,
            market_ai.key.to_bytes(),
            authorities.backing_bucket_authority,
            domain as u16,
            &bucket,
        )?;
        sync_backing_domain_ledger(&mut ledger, &bucket)?;
        write_or_init_backing_domain_ledger(&mut ledger_data, &ledger, initialized)
    }

    #[inline(never)]
    fn handle_sync_insurance_ledger<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
    ) -> ProgramResult {
        let authority = account(accounts, 0)?;
        let market_ai = account(accounts, 1)?;
        let ledger_ai = account(accounts, 2)?;
        expect_signer(authority)?;
        expect_writable(market_ai)?;
        expect_writable(ledger_ai)?;
        expect_owner(market_ai, program_id)?;
        expect_owner(ledger_ai, program_id)?;

        let mut market_data = market_ai.try_borrow_mut_data()?;
        let (cfg, group) = state::market_view_mut(&mut market_data)?;
        expect_live_authority(&cfg.insurance_authority, authority.key)?;
        let mut ledger_data = ledger_ai.try_borrow_mut_data()?;
        let observed = market_insurance_remaining_view(&group, 0)?;
        let (mut ledger, initialized) = read_or_new_insurance_ledger(
            &ledger_data,
            market_ai.key.to_bytes(),
            cfg.insurance_authority,
            observed,
        )?;
        sync_insurance_ledger(&mut ledger, observed)?;
        write_or_init_insurance_ledger(&mut ledger_data, &ledger, initialized)
    }

    #[inline(never)]
    fn handle_withdraw_insurance<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        amount: u128,
    ) -> ProgramResult {
        let authority = account(accounts, 0)?;
        let market_ai = account(accounts, 1)?;
        let dest_token = account(accounts, 2)?;
        let vault_token = account(accounts, 3)?;
        let vault_authority_ai = account(accounts, 4)?;
        let token_program = account(accounts, 5)?;
        let ledger_ai = accounts.get(6);
        expect_signer(authority)?;
        expect_writable(market_ai)?;
        expect_writable(dest_token)?;
        expect_writable(vault_token)?;
        expect_owner(market_ai, program_id)?;
        if let Some(ledger_ai) = ledger_ai {
            expect_writable(ledger_ai)?;
            expect_owner(ledger_ai, program_id)?;
        }
        verify_token_program(token_program)?;
        if amount == 0 {
            return Err(PercolatorError::InvalidInstruction.into());
        }

        let cfg_pre = {
            let mut market_data = market_ai.try_borrow_mut_data()?;
            let (cfg, mut group) = state::market_view_mut(&mut market_data)?;
            let available_insurance =
                terminal_insurance_remaining_for_authority_view(&group, &cfg, authority.key)?;
            if group.header.mode != 1
                || group.header.materialized_portfolio_count.get() != 0
                || group.header.c_tot.get() != 0
                || amount > available_insurance
                || amount > group.header.vault.get()
            {
                return Err(PercolatorError::EngineLockActive.into());
            }
            let mut ledger_data = if let Some(ledger_ai) = ledger_ai {
                Some(ledger_ai.try_borrow_mut_data()?)
            } else {
                None
            };
            let mut ledger_state = if let Some(data) = ledger_data.as_deref() {
                let (mut ledger, initialized) = read_or_new_insurance_ledger(
                    data,
                    market_ai.key.to_bytes(),
                    authority.key.to_bytes(),
                    available_insurance,
                )?;
                sync_insurance_ledger(&mut ledger, available_insurance)?;
                Some((ledger, initialized))
            } else {
                None
            };
            group.header.insurance = percolator::V16PodU128::new(
                group
                    .header
                    .insurance
                    .get()
                    .checked_sub(amount)
                    .ok_or(PercolatorError::EngineCounterUnderflow)?,
            );
            group.header.vault = percolator::V16PodU128::new(
                group
                    .header
                    .vault
                    .get()
                    .checked_sub(amount)
                    .ok_or(PercolatorError::EngineCounterUnderflow)?,
            );
            debit_terminal_insurance_budgets_for_authority_view(
                &mut group,
                &cfg,
                authority.key,
                amount,
            )?;
            if let Some((ledger, _)) = ledger_state.as_mut() {
                ledger.total_withdrawn_atoms = ledger
                    .total_withdrawn_atoms
                    .checked_add(amount)
                    .ok_or(PercolatorError::EngineArithmeticOverflow)?;
                ledger.total_principal_atoms = ledger.total_principal_atoms.saturating_sub(amount);
                ledger.last_observed_insurance_atoms = ledger
                    .last_observed_insurance_atoms
                    .checked_sub(amount)
                    .ok_or(PercolatorError::EngineCounterUnderflow)?;
            }
            group.validate_shape().map_err(map_v16_error)?;
            if let (Some(data), Some((ledger, initialized))) =
                (ledger_data.as_deref_mut(), ledger_state.as_ref())
            {
                write_or_init_insurance_ledger(data, ledger, *initialized)?;
            }
            cfg
        };

        let (vault_authority, bump) = derive_vault_authority(program_id, market_ai.key);
        expect_key(vault_authority_ai, &vault_authority)?;
        verify_withdrawable_token_accounts(
            dest_token,
            authority.key,
            vault_token,
            &vault_authority,
            &cfg_pre,
        )?;
        let amount_u64 = amount_to_u64(amount)?;
        require_token_balance(vault_token, amount_u64)?;
        let bump_arr = [bump];
        let signer_seeds: &[&[&[u8]]] = &[&[b"vault", market_ai.key.as_ref(), &bump_arr]];
        transfer_tokens_signed(
            token_program,
            vault_token,
            dest_token,
            vault_authority_ai,
            amount_u64,
            signer_seeds,
        )?;
        Ok(())
    }

    #[inline(never)]
    fn handle_withdraw_insurance_domain<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        domain: u8,
        amount: u128,
    ) -> ProgramResult {
        let operator = account(accounts, 0)?;
        let market_ai = account(accounts, 1)?;
        let dest_token = account(accounts, 2)?;
        let vault_token = account(accounts, 3)?;
        let vault_authority_ai = account(accounts, 4)?;
        let token_program = account(accounts, 5)?;
        let ledger_ai = accounts.get(6);
        expect_signer(operator)?;
        expect_writable(market_ai)?;
        expect_writable(dest_token)?;
        expect_writable(vault_token)?;
        expect_owner(market_ai, program_id)?;
        if let Some(ledger_ai) = ledger_ai {
            expect_writable(ledger_ai)?;
            expect_owner(ledger_ai, program_id)?;
        }
        verify_token_program(token_program)?;
        if amount == 0 {
            return Err(PercolatorError::InvalidInstruction.into());
        }
        let domain = domain as usize;
        let (bump, amount_u64) = verify_domain_withdrawal_preflight(
            program_id,
            market_ai,
            operator,
            dest_token,
            vault_token,
            vault_authority_ai,
            domain,
            amount,
            true,
            DOMAIN_WITHDRAW_AUTH_INSURANCE,
        )?;
        {
            let mut market_data = market_ai.try_borrow_mut_data()?;
            let (cfg, mut group) = state::market_view_mut(&mut market_data)?;
            if group.header.mode != 0 {
                return Err(PercolatorError::EngineLockActive.into());
            }
            let shutdown_drain =
                live_domain_withdraw_health_or_shutdown_view(&cfg, &group, domain)?;
            let authorities = domain_authorities_from_view(&group, &cfg, domain)?;
            let local_authorized =
                live_authority_matches(&authorities.insurance_operator, operator.key);
            let admin_shutdown_authorized =
                shutdown_drain && live_authority_matches(&cfg.admin, operator.key);
            if !local_authorized && !admin_shutdown_authorized {
                return Err(PercolatorError::Unauthorized.into());
            }
            let ledger_authority = if admin_shutdown_authorized && !local_authorized {
                cfg.admin
            } else {
                authorities.insurance_authority
            };
            let available = domain_budget_remaining_view(&group, domain)?;
            if amount > available
                || amount > group.header.insurance.get()
                || amount > group.header.vault.get()
            {
                return Err(PercolatorError::EngineLockActive.into());
            }
            let mut ledger_data = if let Some(ledger_ai) = ledger_ai {
                Some(ledger_ai.try_borrow_mut_data()?)
            } else {
                None
            };
            let mut ledger_state = if let Some(data) = ledger_data.as_deref() {
                let (mut ledger, initialized) = read_or_new_insurance_ledger(
                    data,
                    market_ai.key.to_bytes(),
                    ledger_authority,
                    available,
                )?;
                sync_insurance_ledger(&mut ledger, available)?;
                Some((ledger, initialized))
            } else {
                None
            };
            group.header.insurance = percolator::V16PodU128::new(
                group
                    .header
                    .insurance
                    .get()
                    .checked_sub(amount)
                    .ok_or(PercolatorError::EngineCounterUnderflow)?,
            );
            group.header.vault = percolator::V16PodU128::new(
                group
                    .header
                    .vault
                    .get()
                    .checked_sub(amount)
                    .ok_or(PercolatorError::EngineCounterUnderflow)?,
            );
            let (budget, _) = domain_budget_parts_view(&group, domain)?;
            set_domain_budget_view(
                &mut group,
                domain,
                budget
                    .checked_sub(amount)
                    .ok_or(PercolatorError::EngineCounterUnderflow)?,
            )?;
            if let Some((ledger, _)) = ledger_state.as_mut() {
                ledger.total_withdrawn_atoms = ledger
                    .total_withdrawn_atoms
                    .checked_add(amount)
                    .ok_or(PercolatorError::EngineArithmeticOverflow)?;
                ledger.total_principal_atoms = ledger.total_principal_atoms.saturating_sub(amount);
                ledger.last_observed_insurance_atoms = ledger
                    .last_observed_insurance_atoms
                    .checked_sub(amount)
                    .ok_or(PercolatorError::EngineCounterUnderflow)?;
            }
            group.validate_shape().map_err(map_v16_error)?;
            if let (Some(data), Some((ledger, initialized))) =
                (ledger_data.as_deref_mut(), ledger_state.as_ref())
            {
                write_or_init_insurance_ledger(data, ledger, *initialized)?;
            }
        }
        let bump_arr = [bump];
        let signer_seeds: &[&[&[u8]]] = &[&[b"vault", market_ai.key.as_ref(), &bump_arr]];
        transfer_tokens_signed(
            token_program,
            vault_token,
            dest_token,
            vault_authority_ai,
            amount_u64,
            signer_seeds,
        )?;
        Ok(())
    }

    #[inline(never)]
    fn handle_close_slab<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
    ) -> ProgramResult {
        let admin_dest = account(accounts, 0)?;
        let market_ai = account(accounts, 1)?;
        let vault_token = account(accounts, 2)?;
        let vault_authority_ai = account(accounts, 3)?;
        let dest_token = account(accounts, 4)?;
        let token_program = account(accounts, 5)?;
        expect_signer(admin_dest)?;
        expect_writable(admin_dest)?;
        expect_writable(market_ai)?;
        expect_writable(vault_token)?;
        expect_owner(market_ai, program_id)?;
        verify_token_program(token_program)?;

        let cfg_pre = {
            let mut market_data = market_ai.try_borrow_mut_data()?;
            let (cfg, group) = state::market_view_mut(&mut market_data)?;
            expect_live_authority(&cfg.admin, admin_dest.key)?;
            if group.header.mode != 1 {
                return Err(PercolatorError::EngineLockActive.into());
            }
            if group.header.vault.get() != 0
                || group.header.insurance.get() != 0
                || group.header.c_tot.get() != 0
                || group.header.materialized_portfolio_count.get() != 0
            {
                return Err(PercolatorError::EngineLockActive.into());
            }
            cfg
        };

        let (vault_authority, bump) = derive_vault_authority(program_id, market_ai.key);
        expect_key(vault_authority_ai, &vault_authority)?;
        let vault_account =
            verify_withdrawable_vault_token_account(vault_token, &vault_authority, &cfg_pre)?;
        let stranded = vault_account.amount;
        if stranded > 0 {
            verify_user_token_account(dest_token, admin_dest.key, &vault_account.mint)?;
            let bump_arr = [bump];
            let signer_seeds: &[&[&[u8]]] = &[&[b"vault", market_ai.key.as_ref(), &bump_arr]];
            transfer_tokens_signed(
                token_program,
                vault_token,
                dest_token,
                vault_authority_ai,
                stranded,
                signer_seeds,
            )?;
        }

        let bump_arr = [bump];
        let signer_seeds: &[&[&[u8]]] = &[&[b"vault", market_ai.key.as_ref(), &bump_arr]];
        let close_ix = spl_token::instruction::close_account(
            token_program.key,
            vault_token.key,
            admin_dest.key,
            vault_authority_ai.key,
            &[],
        )?;
        invoke_signed(
            &close_ix,
            &[
                vault_token.clone(),
                admin_dest.clone(),
                vault_authority_ai.clone(),
                token_program.clone(),
            ],
            signer_seeds,
        )?;

        for b in market_ai.try_borrow_mut_data()?.iter_mut() {
            *b = 0;
        }
        let market_lamports = market_ai.lamports();
        **market_ai.lamports.borrow_mut() = 0;
        **admin_dest.lamports.borrow_mut() = admin_dest
            .lamports()
            .checked_add(market_lamports)
            .ok_or(PercolatorError::EngineArithmeticOverflow)?;
        Ok(())
    }

    #[inline(never)]
    fn handle_withdraw_insurance_limited<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        amount: u128,
    ) -> ProgramResult {
        let operator = account(accounts, 0)?;
        let market_ai = account(accounts, 1)?;
        let dest_token = account(accounts, 2)?;
        let vault_token = account(accounts, 3)?;
        let vault_authority_ai = account(accounts, 4)?;
        let token_program = account(accounts, 5)?;
        let ledger_ai = accounts.get(6);
        expect_signer(operator)?;
        expect_writable(market_ai)?;
        expect_writable(dest_token)?;
        expect_writable(vault_token)?;
        expect_owner(market_ai, program_id)?;
        if let Some(ledger_ai) = ledger_ai {
            expect_writable(ledger_ai)?;
            expect_owner(ledger_ai, program_id)?;
        }
        verify_token_program(token_program)?;
        if amount == 0 {
            return Err(PercolatorError::InvalidInstruction.into());
        }

        let (cfg_pre, mode, _, _) =
            state::read_market_config_mode_and_capacity(&market_ai.try_borrow_data()?)?;
        expect_live_authority(&cfg_pre.insurance_operator, operator.key)?;
        if mode != MarketModeV16::Live {
            return Err(PercolatorError::EngineLockActive.into());
        }
        let (vault_authority, bump) = derive_vault_authority(program_id, market_ai.key);
        expect_key(vault_authority_ai, &vault_authority)?;
        verify_withdrawable_token_accounts(
            dest_token,
            operator.key,
            vault_token,
            &vault_authority,
            &cfg_pre,
        )?;
        let amount_u64 = amount_to_u64(amount)?;
        require_token_balance(vault_token, amount_u64)?;
        let cfg_after = {
            let mut market_data = market_ai.try_borrow_mut_data()?;
            let (mut cfg, mut group) = state::market_view_mut(&mut market_data)?;
            expect_live_authority(&cfg.insurance_operator, operator.key)?;
            if group.header.mode != 0 {
                return Err(PercolatorError::EngineLockActive.into());
            }
            reject_permissionless_resolve_matured_live_view(&cfg, &group)?;
            if group.header.bankruptcy_hlock_active != 0
                || group.header.threshold_stress_active != 0
                || group.header.loss_stale_active != 0
                || group
                    .header
                    .recovery_reason
                    .try_to_runtime()
                    .map_err(map_v16_error)?
                    .is_some()
            {
                return Err(PercolatorError::EngineLockActive.into());
            }
            let clock_slot = Clock::get()
                .map(|c| c.slot)
                .unwrap_or(group.header.current_slot.get());
            if cfg.insurance_withdraw_max_bps == 0 {
                return Err(PercolatorError::EngineLockActive.into());
            }
            if cfg.last_insurance_withdraw_slot != 0
                && cfg.insurance_withdraw_cooldown_slots != 0
                && clock_slot.saturating_sub(cfg.last_insurance_withdraw_slot)
                    < cfg.insurance_withdraw_cooldown_slots
            {
                return Err(PercolatorError::EngineLockActive.into());
            }
            let insurance = market_insurance_remaining_view(&group, 0)?;
            let vault = group.header.vault.get();
            let mut cap = insurance
                .checked_mul(cfg.insurance_withdraw_max_bps as u128)
                .ok_or(PercolatorError::EngineArithmeticOverflow)?
                / 10_000;
            if cap == 0 && insurance >= constants::MIN_INSURANCE_WITHDRAW_FLOOR_UNITS {
                cap = constants::MIN_INSURANCE_WITHDRAW_FLOOR_UNITS;
            }
            if cfg.insurance_withdraw_deposits_only != 0 {
                cap = core::cmp::min(cap, cfg.insurance_withdraw_deposit_remaining);
            }
            if amount > cap
                || amount > insurance
                || amount > group.header.insurance.get()
                || amount > vault
            {
                return Err(PercolatorError::EngineLockActive.into());
            }
            let mut ledger_data = if let Some(ledger_ai) = ledger_ai {
                Some(ledger_ai.try_borrow_mut_data()?)
            } else {
                None
            };
            let mut ledger_state = if let Some(data) = ledger_data.as_deref() {
                let (mut ledger, initialized) = read_or_new_insurance_ledger(
                    data,
                    market_ai.key.to_bytes(),
                    cfg.insurance_authority,
                    insurance,
                )?;
                sync_insurance_ledger(&mut ledger, insurance)?;
                Some((ledger, initialized))
            } else {
                None
            };
            group.header.insurance = percolator::V16PodU128::new(
                group
                    .header
                    .insurance
                    .get()
                    .checked_sub(amount)
                    .ok_or(PercolatorError::EngineCounterUnderflow)?,
            );
            group.header.vault = percolator::V16PodU128::new(vault - amount);
            debit_market_insurance_budget_view(&mut group, 0, amount)?;
            if let Some((ledger, _)) = ledger_state.as_mut() {
                ledger.total_withdrawn_atoms = ledger
                    .total_withdrawn_atoms
                    .checked_add(amount)
                    .ok_or(PercolatorError::EngineArithmeticOverflow)?;
                ledger.total_principal_atoms = ledger.total_principal_atoms.saturating_sub(amount);
                ledger.last_observed_insurance_atoms = ledger
                    .last_observed_insurance_atoms
                    .checked_sub(amount)
                    .ok_or(PercolatorError::EngineCounterUnderflow)?;
            }
            if cfg.insurance_withdraw_deposits_only != 0 {
                cfg.insurance_withdraw_deposit_remaining = cfg
                    .insurance_withdraw_deposit_remaining
                    .checked_sub(amount)
                    .ok_or(PercolatorError::EngineCounterUnderflow)?;
            }
            cfg.last_insurance_withdraw_slot = clock_slot;
            group.validate_shape().map_err(map_v16_error)?;
            if let (Some(data), Some((ledger, initialized))) =
                (ledger_data.as_deref_mut(), ledger_state.as_ref())
            {
                write_or_init_insurance_ledger(data, ledger, *initialized)?;
            }
            cfg
        };
        let bump_arr = [bump];
        let signer_seeds: &[&[&[u8]]] = &[&[b"vault", market_ai.key.as_ref(), &bump_arr]];
        transfer_tokens_signed(
            token_program,
            vault_token,
            dest_token,
            vault_authority_ai,
            amount_u64,
            signer_seeds,
        )?;
        state::write_wrapper_config(&mut market_ai.try_borrow_mut_data()?, &cfg_after)
    }

    #[inline(never)]
    fn handle_convert_released_pnl<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        amount: u128,
    ) -> ProgramResult {
        if amount == 0 {
            return Err(PercolatorError::InvalidInstruction.into());
        }
        with_one_portfolio_view(program_id, accounts, true, |group, portfolio, cfg| {
            if group.header.mode != 0 {
                return Err(V16Error::LockActive);
            }
            if permissionless_resolve_matured_now_view(cfg, group) {
                return Err(V16Error::LockActive);
            }
            // The v16 engine converts the currently released residual-bounded
            // amount atomically. Preserve the wrapper caller cap by staging the
            // conversion and only committing it when the converted amount fits.
            let converted = group.convert_released_pnl_to_capital_not_atomic(portfolio)?;
            if converted == 0 || converted > amount {
                return Err(V16Error::LockActive);
            }
            Ok(())
        })
    }

    #[inline(never)]
    fn handle_cure_and_cancel_close<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        optional_deposit: u128,
    ) -> ProgramResult {
        let owner = account(accounts, 0)?;
        let market_ai = account(accounts, 1)?;
        let portfolio_ai = account(accounts, 2)?;
        expect_signer(owner)?;
        expect_writable(market_ai)?;
        expect_writable(portfolio_ai)?;
        expect_owner(market_ai, program_id)?;
        expect_owner(portfolio_ai, program_id)?;

        let (_, _, max_market_slots, _) =
            state::read_market_config_mode_and_capacity(&market_ai.try_borrow_data()?)?;
        ensure_portfolio_storage_for_market_slots(portfolio_ai, max_market_slots)?;

        let amount_u64 = if optional_deposit != 0 {
            let source_token = account(accounts, 3)?;
            let vault_token = account(accounts, 4)?;
            let token_program = account(accounts, 5)?;
            expect_writable(source_token)?;
            expect_writable(vault_token)?;
            verify_token_program(token_program)?;
            let cfg_pre =
                state::read_market_config_mode_and_capacity(&market_ai.try_borrow_data()?)?.0;
            let mint = primary_collateral_mint(&cfg_pre);
            let (vault_authority, _) = derive_vault_authority(program_id, market_ai.key);
            verify_user_token_account(source_token, owner.key, &mint)?;
            verify_vault_token_account(vault_token, &vault_authority, &mint)?;
            let amount_u64 = amount_to_u64(optional_deposit)?;
            require_token_balance(source_token, amount_u64)?;
            Some((amount_u64, source_token, vault_token, token_program))
        } else {
            None
        };

        {
            let mut market_data = market_ai.try_borrow_mut_data()?;
            let (cfg, mut group) = state::market_view_mut(&mut market_data)?;
            if group.header.mode != 0 {
                return Err(PercolatorError::EngineLockActive.into());
            }
            reject_permissionless_resolve_matured_live_view(&cfg, &group)?;
            let mut portfolio_data = portfolio_ai.try_borrow_mut_data()?;
            let mut portfolio =
                state::portfolio_view_mut_for_market_slots(&mut portfolio_data, max_market_slots)?;
            expect_portfolio_view_account_key(&portfolio, portfolio_ai.key)?;
            expect_portfolio_view_owner(&portfolio, owner.key)?;
            group
                .cure_and_cancel_close_not_atomic(&mut portfolio, optional_deposit)
                .map_err(map_v16_error)?;
        }

        if let Some((amount_u64, source_token, vault_token, token_program)) = amount_u64 {
            transfer_tokens(token_program, source_token, vault_token, owner, amount_u64)?;
        }
        Ok(())
    }

    #[inline(never)]
    fn handle_forfeit_recovery_leg<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        asset_index: u16,
        b_delta_budget: u128,
    ) -> ProgramResult {
        if b_delta_budget == 0 {
            return Err(PercolatorError::InvalidInstruction.into());
        }
        with_one_portfolio_view(program_id, accounts, true, |group, portfolio, _cfg| {
            group
                .forfeit_recovery_leg_not_atomic(portfolio, asset_index as usize, b_delta_budget)
                .map(|_| ())
        })
    }

    #[inline(never)]
    fn handle_rebalance_reduce<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        asset_index: u16,
        reduce_q: u128,
    ) -> ProgramResult {
        if reduce_q == 0 {
            return Err(PercolatorError::InvalidInstruction.into());
        }
        with_one_portfolio_view(program_id, accounts, true, |group, portfolio, _cfg| {
            group
                .rebalance_reduce_position_not_atomic(
                    portfolio,
                    RebalanceRequestV16 {
                        asset_index: asset_index as usize,
                        reduce_q,
                    },
                )
                .map(|_| ())
        })
    }

    #[inline(never)]
    fn handle_sync_maintenance_fee<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        now_slot: u64,
    ) -> ProgramResult {
        let market_ai = account(accounts, 0)?;
        let portfolio_ai = account(accounts, 1)?;
        expect_writable(market_ai)?;
        expect_writable(portfolio_ai)?;
        expect_owner(market_ai, program_id)?;
        expect_owner(portfolio_ai, program_id)?;

        let (cfg_pre, _mode, max_market_slots, _) =
            state::read_market_config_mode_and_capacity(&market_ai.try_borrow_data()?)?;
        ensure_portfolio_storage_for_market_slots(portfolio_ai, max_market_slots)?;
        if let Some(cranker_portfolio_ai) = accounts.get(2) {
            expect_writable(cranker_portfolio_ai)?;
            expect_owner(cranker_portfolio_ai, program_id)?;
            if cranker_portfolio_ai.key != portfolio_ai.key {
                ensure_portfolio_storage_for_market_slots(cranker_portfolio_ai, max_market_slots)?;
            }
        }
        let authenticated_now_slot = authenticated_slot_or_fallback(now_slot);

        let mut market_data = market_ai.try_borrow_mut_data()?;
        let (cfg, mut group) = state::market_view_mut(&mut market_data)?;
        if group.header.mode == 0 {
            reject_permissionless_resolve_matured_live_view(&cfg, &group)?;
        }
        let mut portfolio_data = portfolio_ai.try_borrow_mut_data()?;
        let mut portfolio =
            state::portfolio_view_mut_for_market_slots(&mut portfolio_data, max_market_slots)?;
        expect_portfolio_view_account_key(&portfolio, portfolio_ai.key)?;

        if let Some(cranker_portfolio_ai) = accounts.get(2) {
            if cranker_portfolio_ai.key == portfolio_ai.key {
                let charged = group
                    .sync_account_fee_to_slot_not_atomic(
                        &mut portfolio,
                        authenticated_now_slot,
                        cfg_pre.maintenance_fee_per_slot,
                    )
                    .map_err(map_v16_error)?;
                let reward = charged
                    .checked_mul(cfg_pre.maintenance_cranker_fee_share_bps as u128)
                    .ok_or(PercolatorError::EngineArithmeticOverflow)?
                    / 10_000;
                if reward != 0 {
                    group.header.insurance = percolator::V16PodU128::new(
                        group
                            .header
                            .insurance
                            .get()
                            .checked_sub(reward)
                            .ok_or(PercolatorError::EngineArithmeticOverflow)?,
                    );
                    group.header.c_tot = percolator::V16PodU128::new(
                        group
                            .header
                            .c_tot
                            .get()
                            .checked_add(reward)
                            .ok_or(PercolatorError::EngineArithmeticOverflow)?,
                    );
                    portfolio.header.capital = percolator::V16PodU128::new(
                        portfolio
                            .header
                            .capital
                            .get()
                            .checked_add(reward)
                            .ok_or(PercolatorError::EngineArithmeticOverflow)?,
                    );
                    portfolio.header.health_cert.valid = 0;
                    group.validate_shape().map_err(map_v16_error)?;
                    portfolio
                        .validate_with_market(&group.as_view())
                        .map_err(map_v16_error)?;
                }
                let retained = charged
                    .checked_sub(reward)
                    .ok_or(PercolatorError::EngineCounterUnderflow)?;
                credit_maintenance_fee_to_active_market_budgets_view(&cfg, &mut group, retained)?;
                group.validate_shape().map_err(map_v16_error)?;
                portfolio
                    .validate_with_market(&group.as_view())
                    .map_err(map_v16_error)?;
            } else {
                let mut cranker_data = cranker_portfolio_ai.try_borrow_mut_data()?;
                let cranker = state::portfolio_view_mut_for_market_slots(
                    &mut cranker_data,
                    max_market_slots,
                )?;
                expect_portfolio_view_account_key(&cranker, cranker_portfolio_ai.key)?;
                cranker
                    .validate_with_market(&group.as_view())
                    .map_err(map_v16_error)?;
                let charged = group
                    .sync_account_fee_to_slot_not_atomic(
                        &mut portfolio,
                        authenticated_now_slot,
                        cfg_pre.maintenance_fee_per_slot,
                    )
                    .map_err(map_v16_error)?;
                let reward = charged
                    .checked_mul(cfg_pre.maintenance_cranker_fee_share_bps as u128)
                    .ok_or(PercolatorError::EngineArithmeticOverflow)?
                    / 10_000;
                if reward != 0 {
                    group.header.insurance = percolator::V16PodU128::new(
                        group
                            .header
                            .insurance
                            .get()
                            .checked_sub(reward)
                            .ok_or(PercolatorError::EngineArithmeticOverflow)?,
                    );
                    group.header.c_tot = percolator::V16PodU128::new(
                        group
                            .header
                            .c_tot
                            .get()
                            .checked_add(reward)
                            .ok_or(PercolatorError::EngineArithmeticOverflow)?,
                    );
                    cranker.header.capital = percolator::V16PodU128::new(
                        cranker
                            .header
                            .capital
                            .get()
                            .checked_add(reward)
                            .ok_or(PercolatorError::EngineArithmeticOverflow)?,
                    );
                    cranker.header.health_cert.valid = 0;
                    group.validate_shape().map_err(map_v16_error)?;
                    portfolio
                        .validate_with_market(&group.as_view())
                        .map_err(map_v16_error)?;
                    cranker
                        .validate_with_market(&group.as_view())
                        .map_err(map_v16_error)?;
                }
                let retained = charged
                    .checked_sub(reward)
                    .ok_or(PercolatorError::EngineCounterUnderflow)?;
                credit_maintenance_fee_to_active_market_budgets_view(&cfg, &mut group, retained)?;
                group.validate_shape().map_err(map_v16_error)?;
                portfolio
                    .validate_with_market(&group.as_view())
                    .map_err(map_v16_error)?;
                cranker
                    .validate_with_market(&group.as_view())
                    .map_err(map_v16_error)?;
            }
        } else {
            let charged = group
                .sync_account_fee_to_slot_not_atomic(
                    &mut portfolio,
                    authenticated_now_slot,
                    cfg_pre.maintenance_fee_per_slot,
                )
                .map_err(map_v16_error)?;
            credit_maintenance_fee_to_active_market_budgets_view(&cfg, &mut group, charged)?;
            group.validate_shape().map_err(map_v16_error)?;
            portfolio
                .validate_with_market(&group.as_view())
                .map_err(map_v16_error)?;
        }
        Ok(())
    }

    #[inline(never)]
    fn handle_resolve_market<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
    ) -> ProgramResult {
        let admin = account(accounts, 0)?;
        let market_ai = account(accounts, 1)?;
        expect_signer(admin)?;
        expect_writable(market_ai)?;
        expect_owner(market_ai, program_id)?;
        let mut market_data = market_ai.try_borrow_mut_data()?;
        let (cfg, group) = state::market_view_mut(&mut market_data)?;
        if group.header.mode != 0 {
            return Err(PercolatorError::EngineLockActive.into());
        }
        expect_live_authority(&cfg.admin, admin.key)?;
        let slot = Clock::get()
            .map(|c| c.slot)
            .unwrap_or(group.header.current_slot.get());
        if slot < group.header.current_slot.get() {
            return Err(PercolatorError::EngineStale.into());
        }
        group.header.mode = 1;
        group.header.resolved_slot = percolator::V16PodU64::new(slot);
        group.header.current_slot = percolator::V16PodU64::new(slot);
        group.header.loss_stale_active = 0;
        group.validate_shape().map_err(map_v16_error)?;
        Ok(())
    }

    #[inline(never)]
    fn handle_update_authority<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        kind: u8,
        new_pubkey: [u8; 32],
    ) -> ProgramResult {
        let current = account(accounts, 0)?;
        let new_authority = account(accounts, 1)?;
        let market_ai = account(accounts, 2)?;
        expect_signer(current)?;
        expect_writable(market_ai)?;
        expect_owner(market_ai, program_id)?;

        if new_pubkey != [0u8; 32] {
            expect_signer(new_authority)?;
            if new_authority.key.to_bytes() != new_pubkey {
                return Err(PercolatorError::Unauthorized.into());
            }
        }

        let (mut cfg, mode, _, _) =
            state::read_market_config_mode_and_capacity(&market_ai.try_borrow_data()?)?;
        match kind {
            AUTHORITY_ADMIN => {
                expect_live_authority(&cfg.admin, current.key)?;
                if new_pubkey == [0u8; 32]
                    && (mode == MarketModeV16::Live
                        && (cfg.permissionless_resolve_stale_slots == 0
                            || cfg.force_close_delay_slots == 0))
                {
                    return Err(PercolatorError::InvalidInstruction.into());
                }
                cfg.admin = new_pubkey;
            }
            AUTHORITY_MARK => {
                expect_live_authority(&cfg.mark_authority, current.key)?;
                cfg.mark_authority = new_pubkey;
            }
            AUTHORITY_INSURANCE => {
                expect_live_authority(&cfg.insurance_authority, current.key)?;
                cfg.insurance_authority = new_pubkey;
            }
            AUTHORITY_BACKING_BUCKET => {
                expect_live_authority(&cfg.backing_bucket_authority, current.key)?;
                cfg.backing_bucket_authority = new_pubkey;
            }
            AUTHORITY_ASSET => {
                expect_live_authority(&cfg.asset_authority, current.key)?;
                cfg.asset_authority = new_pubkey;
            }
            AUTHORITY_INSURANCE_OPERATOR => {
                expect_live_authority(&cfg.insurance_operator, current.key)?;
                cfg.insurance_operator = new_pubkey;
            }
            AUTHORITY_BASE_UNIT => {
                expect_live_authority(&cfg.base_unit_authority, current.key)?;
                cfg.base_unit_authority = new_pubkey;
            }
            _ => return Err(PercolatorError::InvalidInstruction.into()),
        }

        state::write_wrapper_config(&mut market_ai.try_borrow_mut_data()?, &cfg)
    }

    #[inline(never)]
    fn handle_update_base_unit_mints<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        primary_mint: [u8; 32],
        secondary_mint: [u8; 32],
    ) -> ProgramResult {
        let authority = account(accounts, 0)?;
        let market_ai = account(accounts, 1)?;
        let primary_mint_ai = account(accounts, 2)?;
        let secondary_mint_ai = account(accounts, 3)?;
        expect_signer(authority)?;
        expect_writable(market_ai)?;
        expect_owner(market_ai, program_id)?;
        if primary_mint == [0u8; 32]
            || secondary_mint == [0u8; 32]
            || primary_mint == secondary_mint
        {
            return Err(PercolatorError::InvalidMint.into());
        }
        let primary_key = Pubkey::new_from_array(primary_mint);
        let secondary_key = Pubkey::new_from_array(secondary_mint);
        expect_key(primary_mint_ai, &primary_key)?;
        expect_key(secondary_mint_ai, &secondary_key)?;
        verify_mint(primary_mint_ai)?;
        verify_mint(secondary_mint_ai)?;

        // RESYNC(0925ed4, F32): rotating the collateral mints while the market
        // holds any custody (vault/c_tot/insurance != 0) would orphan deposited
        // funds under the old mint — reject. Single mutable borrow of `data`
        // (cfg derived in an inner scope, written back to the same `data`) to
        // avoid a RefCell double-borrow panic.
        let mut data = market_ai.try_borrow_mut_data()?;
        let mut cfg = {
            let (cfg, group) = state::market_view_mut(&mut data)?;
            expect_live_authority(&cfg.base_unit_authority, authority.key)?;
            if group.header.vault.get() != 0
                || group.header.c_tot.get() != 0
                || group.header.insurance.get() != 0
            {
                return Err(PercolatorError::EngineLockActive.into());
            }
            cfg
        };
        cfg.collateral_mint = primary_mint;
        cfg.secondary_collateral_mint = secondary_mint;
        state::write_wrapper_config(&mut data, &cfg)
    }

    #[inline(never)]
    fn handle_swap_secondary_for_primary<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        amount: u128,
    ) -> ProgramResult {
        let authority = account(accounts, 0)?;
        let market_ai = account(accounts, 1)?;
        let primary_source_token = account(accounts, 2)?;
        let primary_vault_token = account(accounts, 3)?;
        let secondary_dest_token = account(accounts, 4)?;
        let secondary_vault_token = account(accounts, 5)?;
        let vault_authority_ai = account(accounts, 6)?;
        let token_program = account(accounts, 7)?;
        expect_signer(authority)?;
        expect_writable(primary_source_token)?;
        expect_writable(primary_vault_token)?;
        expect_writable(secondary_dest_token)?;
        expect_writable(secondary_vault_token)?;
        expect_owner(market_ai, program_id)?;
        verify_token_program(token_program)?;
        if amount == 0 {
            return Err(PercolatorError::InvalidInstruction.into());
        }
        let amount_u64 = amount_to_u64(amount)?;

        let (cfg, _, _, _) =
            state::read_market_config_mode_and_capacity(&market_ai.try_borrow_data()?)?;
        expect_live_authority(&cfg.base_unit_authority, authority.key)?;
        let primary_mint = primary_collateral_mint(&cfg);
        let secondary_mint = secondary_collateral_mint(&cfg)?;
        let (vault_authority, bump) = derive_vault_authority(program_id, market_ai.key);
        expect_key(vault_authority_ai, &vault_authority)?;
        verify_user_token_account(primary_source_token, authority.key, &primary_mint)?;
        verify_vault_token_account(primary_vault_token, &vault_authority, &primary_mint)?;
        verify_user_token_account(secondary_dest_token, authority.key, &secondary_mint)?;
        verify_vault_token_account(secondary_vault_token, &vault_authority, &secondary_mint)?;
        require_token_balance(primary_source_token, amount_u64)?;
        require_token_balance(secondary_vault_token, amount_u64)?;

        transfer_tokens(
            token_program,
            primary_source_token,
            primary_vault_token,
            authority,
            amount_u64,
        )?;
        let bump_arr = [bump];
        let signer_seeds: &[&[&[u8]]] = &[&[b"vault", market_ai.key.as_ref(), &bump_arr]];
        transfer_tokens_signed(
            token_program,
            secondary_vault_token,
            secondary_dest_token,
            vault_authority_ai,
            amount_u64,
            signer_seeds,
        )
    }

    #[inline(never)]
    fn handle_update_asset_lifecycle<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        action: u8,
        asset_index: u16,
        now_slot: u64,
        initial_price: u64,
        insurance_authority: [u8; 32],
        insurance_operator: [u8; 32],
        backing_bucket_authority: [u8; 32],
        oracle_authority: [u8; 32],
    ) -> ProgramResult {
        let authority = account(accounts, 0)?;
        let market_ai = account(accounts, 1)?;
        expect_signer(authority)?;
        expect_writable(market_ai)?;
        expect_owner(market_ai, program_id)?;

        let asset_index = asset_index as usize;
        let (cfg_pre, mode_pre, configured_slots_pre, capacity_pre) =
            state::read_market_config_mode_and_capacity(&market_ai.try_borrow_data()?)?;
        if mode_pre != MarketModeV16::Live {
            return Err(PercolatorError::EngineLockActive.into());
        }
        let is_asset_authority = cfg_pre.asset_authority != [0u8; 32]
            && cfg_pre.asset_authority == authority.key.to_bytes();
        let permissionless_reuse_target = action == ASSET_ACTION_ACTIVATE
            && !is_asset_authority
            && asset_index < configured_slots_pre
            && cfg_pre.free_market_slot_count != 0;
        if action == ASSET_ACTION_ACTIVATE
            && (asset_index == configured_slots_pre || permissionless_reuse_target)
        {
            let authenticated_slot = authenticated_slot_or_fallback(now_slot);
            let append_activation = asset_index == configured_slots_pre;
            if append_activation && cfg_pre.free_market_slot_count != 0 {
                return Err(PercolatorError::EngineLockActive.into());
            }
            let init_fee = if is_asset_authority {
                0
            } else {
                let fee = permissionless_market_init_fee_for_asset(
                    cfg_pre.permissionless_market_init_fee,
                    asset_index,
                )?;
                if fee == 0 {
                    return Err(PercolatorError::Unauthorized.into());
                }
                fee
            };
            let transfer_accounts = if init_fee == 0 {
                None
            } else {
                let source_token = account(accounts, 2)?;
                let vault_token = account(accounts, 3)?;
                let token_program = account(accounts, 4)?;
                expect_writable(source_token)?;
                expect_writable(vault_token)?;
                verify_token_program(token_program)?;
                let mint = primary_collateral_mint(&cfg_pre);
                let (vault_authority, _) = derive_vault_authority(program_id, market_ai.key);
                verify_user_token_account(source_token, authority.key, &mint)?;
                verify_vault_token_account(vault_token, &vault_authority, &mint)?;
                let amount_u64 = amount_to_u64(init_fee)?;
                require_token_balance(source_token, amount_u64)?;
                Some((source_token, vault_token, token_program, amount_u64))
            };
            if asset_index >= capacity_pre {
                let new_len = state::market_account_len_for_capacity(asset_index + 1)?;
                market_ai.realloc(new_len, true)?;
            }
            {
                let mut data = market_ai.try_borrow_mut_data()?;
                let mut reuse_cfg_after = None;
                let mut reuse_activated = false;
                {
                    let (mut cfg, mut group) = state::market_view_mut(&mut data)?;
                    let still_asset_authority = cfg.asset_authority != [0u8; 32]
                        && cfg.asset_authority == authority.key.to_bytes();
                    if !still_asset_authority {
                        let expected_fee = permissionless_market_init_fee_for_asset(
                            cfg.permissionless_market_init_fee,
                            asset_index,
                        )?;
                        if expected_fee == 0 || expected_fee != init_fee {
                            return Err(PercolatorError::Unauthorized.into());
                        }
                    }
                    if group.header.mode != 0 {
                        return Err(PercolatorError::EngineLockActive.into());
                    }
                    let configured_slots = group.header.config.max_market_slots.get() as usize;
                    if asset_index == configured_slots {
                        if cfg.free_market_slot_count != 0 {
                            return Err(PercolatorError::EngineLockActive.into());
                        }
                    } else if !still_asset_authority
                        && asset_index < configured_slots
                        && cfg.free_market_slot_count != 0
                    {
                        if group.markets[asset_index].engine.asset.lifecycle
                            != ASSET_LIFECYCLE_RETIRED
                        {
                            return Err(PercolatorError::EngineLockActive.into());
                        }
                        group
                            .header
                            .activate_empty_market_slot_not_atomic(
                                asset_index as u32,
                                &mut group.markets[asset_index],
                                initial_price,
                                authenticated_slot,
                            )
                            .map_err(map_v16_error)?;
                        clear_asset_domain_budget_counters_view(&mut group, asset_index)?;
                        cfg.free_market_slot_count = cfg
                            .free_market_slot_count
                            .checked_sub(1)
                            .ok_or(PercolatorError::EngineCounterUnderflow)?;
                        let mut profile =
                            state::manual_asset_oracle_profile(initial_price, authenticated_slot);
                        profile.insurance_authority = insurance_authority;
                        profile.insurance_operator = insurance_operator;
                        profile.backing_bucket_authority = backing_bucket_authority;
                        profile.oracle_authority = oracle_authority;
                        write_oracle_profile_to_view_if_separate(
                            &mut group,
                            asset_index,
                            &profile,
                        )?;
                        group.validate_shape().map_err(map_v16_error)?;
                        reuse_cfg_after = Some(cfg);
                        reuse_activated = true;
                    } else {
                        return Err(PercolatorError::EngineLockActive.into());
                    }
                }
                if let Some(cfg) = reuse_cfg_after {
                    state::write_wrapper_config(&mut data, &cfg)?;
                }
                if reuse_activated && init_fee != 0 {
                    let (_cfg, mut group) = state::market_view_mut(&mut data)?;
                    group.header.insurance = percolator::V16PodU128::new(
                        group
                            .header
                            .insurance
                            .get()
                            .checked_add(init_fee)
                            .ok_or(PercolatorError::EngineArithmeticOverflow)?,
                    );
                    group.header.vault = percolator::V16PodU128::new(
                        group
                            .header
                            .vault
                            .get()
                            .checked_add(init_fee)
                            .ok_or(PercolatorError::EngineArithmeticOverflow)?,
                    );
                    credit_market_insurance_budget_view(&mut group, 0, init_fee)?;
                    group.validate_shape().map_err(map_v16_error)?;
                }
                let (_cfg, _mode, configured_slots, _) =
                    state::read_market_config_mode_and_capacity(&data)?;
                if !reuse_activated && asset_index == configured_slots {
                    let profile = state::activate_dynamic_asset_slot(
                        &mut data,
                        asset_index,
                        authenticated_slot,
                        initial_price,
                        insurance_authority,
                        insurance_operator,
                        backing_bucket_authority,
                        oracle_authority,
                    )?;
                    state::write_asset_oracle_profile(&mut data, asset_index, &profile)?;
                    if init_fee != 0 {
                        let (_cfg, mut group) = state::market_view_mut(&mut data)?;
                        group.header.insurance = percolator::V16PodU128::new(
                            group
                                .header
                                .insurance
                                .get()
                                .checked_add(init_fee)
                                .ok_or(PercolatorError::EngineArithmeticOverflow)?,
                        );
                        group.header.vault = percolator::V16PodU128::new(
                            group
                                .header
                                .vault
                                .get()
                                .checked_add(init_fee)
                                .ok_or(PercolatorError::EngineArithmeticOverflow)?,
                        );
                        credit_market_insurance_budget_view(&mut group, 0, init_fee)?;
                        group.validate_shape().map_err(map_v16_error)?;
                    }
                }
            }
            if let Some((source_token, vault_token, token_program, amount_u64)) = transfer_accounts
            {
                transfer_tokens(
                    token_program,
                    source_token,
                    vault_token,
                    authority,
                    amount_u64,
                )?;
            }
            return Ok(());
        }
        if action == ASSET_ACTION_SHUTDOWN {
            expect_live_authority(&cfg_pre.admin, authority.key)?;
            if asset_index == 0
                || now_slot == 0
                || initial_price != 0
                || cfg_pre.force_close_delay_slots == 0
            {
                return Err(PercolatorError::InvalidInstruction.into());
            }
            let authenticated_slot = authenticated_slot_or_fallback(now_slot);
            let mut data = market_ai.try_borrow_mut_data()?;
            let (cfg, mut group) = state::market_view_mut(&mut data)?;
            expect_live_authority(&cfg.admin, authority.key)?;
            if group.header.mode != 0 {
                return Err(PercolatorError::EngineLockActive.into());
            }
            let configured_slots = group.header.config.max_market_slots.get() as usize;
            if asset_index >= configured_slots || asset_index >= group.markets.len() {
                return Err(PercolatorError::InvalidInstruction.into());
            }
            if authenticated_slot < group.header.current_slot.get() {
                return Err(PercolatorError::EngineStale.into());
            }
            match group.markets[asset_index].engine.asset.lifecycle {
                ASSET_LIFECYCLE_ACTIVE | ASSET_LIFECYCLE_DRAIN_ONLY => {
                    let frozen_mark = group.markets[asset_index]
                        .engine
                        .asset
                        .effective_price
                        .get();
                    if frozen_mark == 0 || frozen_mark > percolator::MAX_ORACLE_PRICE {
                        return Err(PercolatorError::OracleInvalid.into());
                    }
                    group.markets[asset_index].engine.asset.lifecycle = ASSET_LIFECYCLE_RECOVERY;
                    group.markets[asset_index]
                        .engine
                        .asset
                        .raw_oracle_target_price = percolator::V16PodU64::new(frozen_mark);
                    let mut profile = read_oracle_profile_from_view(&group, &cfg, asset_index)?;
                    profile.mark_ewma_e6 = frozen_mark;
                    profile.mark_ewma_last_slot = authenticated_slot;
                    profile.oracle_target_price_e6 = frozen_mark;
                    profile.oracle_target_publish_time = 0;
                    profile.last_good_oracle_slot = authenticated_slot;
                    write_oracle_profile_to_view_if_separate(&mut group, asset_index, &profile)?;
                    group.header.asset_set_epoch = percolator::V16PodU64::new(
                        group
                            .header
                            .asset_set_epoch
                            .get()
                            .checked_add(1)
                            .ok_or(PercolatorError::EngineCounterOverflow)?,
                    );
                    group.header.risk_epoch = percolator::V16PodU64::new(
                        group
                            .header
                            .risk_epoch
                            .get()
                            .checked_add(1)
                            .ok_or(PercolatorError::EngineCounterOverflow)?,
                    );
                }
                ASSET_LIFECYCLE_RECOVERY => {}
                _ => return Err(PercolatorError::EngineLockActive.into()),
            }
            return group.validate_shape().map_err(map_v16_error);
        }
        let asset_authorized_pre = live_authority_matches(&cfg_pre.asset_authority, authority.key);
        let admin_retire_candidate = action == ASSET_ACTION_RETIRE
            && !asset_authorized_pre
            && live_authority_matches(&cfg_pre.admin, authority.key);
        if !asset_authorized_pre && !admin_retire_candidate {
            return Err(PercolatorError::Unauthorized.into());
        }

        let cfg_after = {
            let mut data = market_ai.try_borrow_mut_data()?;
            let (mut cfg, mut group) = state::market_view_mut(&mut data)?;
            let asset_authorized = live_authority_matches(&cfg.asset_authority, authority.key);
            let admin_retire = action == ASSET_ACTION_RETIRE
                && !asset_authorized
                && live_authority_matches(&cfg.admin, authority.key);
            if !asset_authorized && !admin_retire {
                return Err(PercolatorError::Unauthorized.into());
            }
            if group.header.mode != 0 {
                return Err(PercolatorError::EngineLockActive.into());
            }
            let configured_slots = group.header.config.max_market_slots.get() as usize;
            if asset_index >= configured_slots {
                return Err(PercolatorError::InvalidInstruction.into());
            }
            let authenticated_slot = authenticated_slot_or_fallback(now_slot);
            let existing_profile = read_oracle_profile_from_view(&group, &cfg, asset_index)?;
            let mut reset_profile = None;
            match action {
                ASSET_ACTION_ACTIVATE => {
                    let was_retired = group.markets[asset_index].engine.asset.lifecycle
                        == ASSET_LIFECYCLE_RETIRED;
                    group
                        .header
                        .activate_empty_market_slot_not_atomic(
                            asset_index as u32,
                            &mut group.markets[asset_index],
                            initial_price,
                            authenticated_slot,
                        )
                        .map_err(map_v16_error)?;
                    clear_asset_domain_budget_counters_view(&mut group, asset_index)?;
                    if was_retired && cfg.free_market_slot_count != 0 {
                        cfg.free_market_slot_count -= 1;
                    }
                    let profile = preserve_backing_fee_policy(
                        state::manual_asset_oracle_profile(initial_price, authenticated_slot),
                        &existing_profile,
                    );
                    if asset_index == 0 {
                        mirror_manual_profile_to_base_config(&mut cfg, &profile, true);
                    }
                    reset_profile = Some(profile);
                }
                ASSET_ACTION_DRAIN_ONLY => {
                    if now_slot != 0 || initial_price != 0 {
                        return Err(PercolatorError::InvalidInstruction.into());
                    }
                    let lifecycle = group.markets[asset_index].engine.asset.lifecycle;
                    match lifecycle {
                        ASSET_LIFECYCLE_ACTIVE => {
                            group.markets[asset_index].engine.asset.lifecycle =
                                ASSET_LIFECYCLE_DRAIN_ONLY;
                            group.header.asset_set_epoch = percolator::V16PodU64::new(
                                group
                                    .header
                                    .asset_set_epoch
                                    .get()
                                    .checked_add(1)
                                    .ok_or(PercolatorError::EngineCounterOverflow)?,
                            );
                            group.header.risk_epoch = percolator::V16PodU64::new(
                                group
                                    .header
                                    .risk_epoch
                                    .get()
                                    .checked_add(1)
                                    .ok_or(PercolatorError::EngineCounterOverflow)?,
                            );
                        }
                        ASSET_LIFECYCLE_DRAIN_ONLY => {}
                        _ => return Err(PercolatorError::EngineLockActive.into()),
                    }
                }
                ASSET_ACTION_RETIRE => {
                    // RESYNC(0925ed4, F29): asset_index 0 is the base collateral
                    // slot and must never be retired (matches toly's guard).
                    if asset_index == 0 || now_slot == 0 || initial_price != 0 {
                        return Err(PercolatorError::InvalidInstruction.into());
                    }
                    if admin_retire {
                        shutdown_asset_empty_and_matured_at_slot_view(
                            &cfg,
                            &group,
                            asset_index,
                            authenticated_slot,
                        )?;
                    }
                    if authenticated_slot < group.header.current_slot.get() {
                        return Err(PercolatorError::EngineStale.into());
                    }
                    let lifecycle = group.markets[asset_index].engine.asset.lifecycle;
                    match lifecycle {
                        ASSET_LIFECYCLE_ACTIVE
                        | ASSET_LIFECYCLE_DRAIN_ONLY
                        | ASSET_LIFECYCLE_RECOVERY => {
                            require_empty_asset_lifecycle_state_view(&group, asset_index)?;
                            canonicalize_empty_asset_lifecycle_state_view(&mut group, asset_index);
                            group.markets[asset_index].engine.asset.lifecycle =
                                ASSET_LIFECYCLE_RETIRED;
                            group.markets[asset_index].engine.asset.retired_slot =
                                percolator::V16PodU64::new(authenticated_slot);
                            cfg.free_market_slot_count = cfg
                                .free_market_slot_count
                                .checked_add(1)
                                .ok_or(PercolatorError::EngineCounterOverflow)?;
                            group.header.current_slot =
                                percolator::V16PodU64::new(authenticated_slot);
                            group.header.asset_set_epoch = percolator::V16PodU64::new(
                                group
                                    .header
                                    .asset_set_epoch
                                    .get()
                                    .checked_add(1)
                                    .ok_or(PercolatorError::EngineCounterOverflow)?,
                            );
                            group.header.risk_epoch = percolator::V16PodU64::new(
                                group
                                    .header
                                    .risk_epoch
                                    .get()
                                    .checked_add(1)
                                    .ok_or(PercolatorError::EngineCounterOverflow)?,
                            );
                        }
                        ASSET_LIFECYCLE_RETIRED => {
                            require_empty_asset_lifecycle_state_view(&group, asset_index)?;
                        }
                        _ => return Err(PercolatorError::EngineLockActive.into()),
                    }
                    let price = group.markets[asset_index]
                        .engine
                        .asset
                        .effective_price
                        .get();
                    let profile = preserve_backing_fee_policy(
                        state::manual_asset_oracle_profile(price, authenticated_slot),
                        &existing_profile,
                    );
                    if asset_index == 0 {
                        mirror_manual_profile_to_base_config(&mut cfg, &profile, false);
                    }
                    reset_profile = Some(profile);
                }
                _ => return Err(PercolatorError::InvalidInstruction.into()),
            }
            if let Some(profile) = reset_profile {
                write_oracle_profile_to_view_if_separate(&mut group, asset_index, &profile)?;
            }
            group.validate_shape().map_err(map_v16_error)?;
            cfg
        };
        state::write_wrapper_config(&mut market_ai.try_borrow_mut_data()?, &cfg_after)
    }

    #[inline(never)]
    fn handle_finalize_reset_side<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        asset_index: u16,
        side: u8,
    ) -> ProgramResult {
        let market_ai = account(accounts, 0)?;
        expect_writable(market_ai)?;
        expect_owner(market_ai, program_id)?;
        let side = decode_side(side)?;
        let mut data = market_ai.try_borrow_mut_data()?;
        let (_cfg, group) = state::market_view_mut(&mut data)?;
        let asset_index = asset_index as usize;
        if asset_index >= group.header.config.max_market_slots.get() as usize
            || asset_index >= group.markets.len()
        {
            return Err(PercolatorError::EngineInvalidLeg.into());
        }
        let asset = &mut group.markets[asset_index].engine.asset;
        match side {
            SideV16::Long => {
                if asset.mode_long == 2 {
                    if asset.stored_pos_count_long.get() != 0
                        || asset.stale_account_count_long.get() != 0
                    {
                        return Err(PercolatorError::EngineStale.into());
                    }
                    asset.mode_long = SIDE_MODE_NORMAL;
                }
            }
            SideV16::Short => {
                if asset.mode_short == 2 {
                    if asset.stored_pos_count_short.get() != 0
                        || asset.stale_account_count_short.get() != 0
                    {
                        return Err(PercolatorError::EngineStale.into());
                    }
                    asset.mode_short = SIDE_MODE_NORMAL;
                }
            }
        }
        group.validate_shape().map_err(map_v16_error)
    }

    #[inline(never)]
    fn handle_refine_resolved_unreceipted_bound<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        decrease_num: u128,
    ) -> ProgramResult {
        let admin = account(accounts, 0)?;
        let market_ai = account(accounts, 1)?;
        expect_signer(admin)?;
        expect_writable(market_ai)?;
        expect_owner(market_ai, program_id)?;
        if decrease_num == 0 {
            return Err(PercolatorError::InvalidInstruction.into());
        }
        let mut data = market_ai.try_borrow_mut_data()?;
        let (cfg, group) = state::market_view_mut(&mut data)?;
        expect_live_authority(&cfg.admin, admin.key)?;
        if group.header.mode != 1 || group.header.payout_snapshot_captured == 0 {
            return Err(PercolatorError::EngineLockActive.into());
        }
        let ledger = &mut group.header.resolved_payout_ledger;
        if ledger.payout_halted > 1 || ledger.finalized > 1 {
            return Err(ProgramError::InvalidAccountData);
        }
        let old_num = ledger.current_payout_rate_num.get();
        let old_den = ledger.current_payout_rate_den.get();
        let next_bound = ledger
            .terminal_claim_bound_unreceipted_num
            .get()
            .checked_sub(decrease_num)
            .ok_or(PercolatorError::EngineCounterUnderflow)?;
        ledger.terminal_claim_bound_unreceipted_num = percolator::V16PodU128::new(next_bound);
        let total_bound_num = ledger
            .terminal_claim_exact_receipts_num
            .get()
            .checked_add(next_bound)
            .ok_or(PercolatorError::EngineArithmeticOverflow)?;
        if total_bound_num == 0 {
            ledger.current_payout_rate_num = percolator::V16PodU128::new(1);
            ledger.current_payout_rate_den = percolator::V16PodU128::new(1);
        } else {
            let next_num = ledger
                .snapshot_residual
                .get()
                .checked_mul(BOUND_SCALE)
                .ok_or(PercolatorError::EngineArithmeticOverflow)?
                .min(total_bound_num);
            ledger.current_payout_rate_num = percolator::V16PodU128::new(next_num);
            ledger.current_payout_rate_den = percolator::V16PodU128::new(total_bound_num);
        }
        if !fraction_ge_wide(
            ledger.current_payout_rate_num.get(),
            ledger.current_payout_rate_den.get(),
            old_num,
            old_den,
        )? {
            return Err(PercolatorError::EngineInvalidConfig.into());
        }
        group.validate_shape().map_err(map_v16_error)
    }

    #[inline(never)]
    fn handle_update_insurance_policy<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        max_bps: u16,
        deposits_only: u8,
        cooldown_slots: u64,
    ) -> ProgramResult {
        let admin = account(accounts, 0)?;
        let market_ai = account(accounts, 1)?;
        expect_signer(admin)?;
        expect_writable(market_ai)?;
        expect_owner(market_ai, program_id)?;
        if !state::insurance_withdraw_policy_shape_ok(max_bps, deposits_only, cooldown_slots) {
            return Err(PercolatorError::InvalidInstruction.into());
        }
        let (mut cfg, _, _, _) =
            state::read_market_config_mode_and_capacity(&market_ai.try_borrow_data()?)?;
        expect_live_authority(&cfg.admin, admin.key)?;
        cfg.insurance_withdraw_max_bps = max_bps;
        cfg.insurance_withdraw_deposits_only = deposits_only;
        cfg.insurance_withdraw_cooldown_slots = cooldown_slots;
        if deposits_only == 0 {
            cfg.insurance_withdraw_deposit_remaining = 0;
        }
        state::write_wrapper_config(&mut market_ai.try_borrow_mut_data()?, &cfg)
    }

    #[inline(never)]
    fn handle_update_liquidation_fee_policy<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        cranker_share_bps: u16,
    ) -> ProgramResult {
        let admin = account(accounts, 0)?;
        let market_ai = account(accounts, 1)?;
        expect_signer(admin)?;
        expect_writable(market_ai)?;
        expect_owner(market_ai, program_id)?;
        if cranker_share_bps > 10_000 {
            return Err(PercolatorError::InvalidInstruction.into());
        }
        let (mut cfg, _, _, _) =
            state::read_market_config_mode_and_capacity(&market_ai.try_borrow_data()?)?;
        expect_live_authority(&cfg.admin, admin.key)?;
        cfg.liquidation_cranker_fee_share_bps = cranker_share_bps;
        state::write_wrapper_config(&mut market_ai.try_borrow_mut_data()?, &cfg)
    }

    #[inline(never)]
    fn handle_update_maintenance_fee_policy<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        cranker_share_bps: u16,
    ) -> ProgramResult {
        let admin = account(accounts, 0)?;
        let market_ai = account(accounts, 1)?;
        expect_signer(admin)?;
        expect_writable(market_ai)?;
        expect_owner(market_ai, program_id)?;
        if cranker_share_bps > 10_000 {
            return Err(PercolatorError::InvalidInstruction.into());
        }
        let (mut cfg, _, _, _) =
            state::read_market_config_mode_and_capacity(&market_ai.try_borrow_data()?)?;
        expect_live_authority(&cfg.admin, admin.key)?;
        cfg.maintenance_cranker_fee_share_bps = cranker_share_bps;
        state::write_wrapper_config(&mut market_ai.try_borrow_mut_data()?, &cfg)
    }

    #[inline(never)]
    fn handle_update_backing_fee_policy<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        domain: u8,
        fee_bps: u16,
        insurance_share_bps: u16,
    ) -> ProgramResult {
        let authority = account(accounts, 0)?;
        let market_ai = account(accounts, 1)?;
        expect_signer(authority)?;
        expect_writable(market_ai)?;
        expect_owner(market_ai, program_id)?;
        let domain = domain as usize;
        let asset_index = domain / 2;
        let (mut cfg, _, _, _, max_trading_fee_bps) =
            state::read_market_trade_preflight(&market_ai.try_borrow_data()?, asset_index)?;
        {
            let market_data = market_ai.try_borrow_data()?;
            let profile = read_oracle_profile_for_asset(&market_data, &cfg, asset_index)?;
            let authorities = domain_authorities_from_profile(&cfg, &profile, asset_index);
            expect_live_authority(&authorities.insurance_authority, authority.key)?;
        }
        if fee_bps > 10_000
            || insurance_share_bps > 10_000
            || (fee_bps == 0 && insurance_share_bps != 0)
            || fee_bps as u64 > max_trading_fee_bps
            || fee_bps as u64 > constants::MAX_DYNAMIC_TRADE_FEE_BPS
        {
            return Err(PercolatorError::InvalidInstruction.into());
        }
        let long_side = domain % 2 == 0;
        let adjust_policy_count =
            |cfg: &mut WrapperConfigV16, old_fee: u16, new_fee: u16| -> ProgramResult {
                if old_fee == 0 && new_fee != 0 {
                    cfg.backing_trade_fee_policy_count = cfg
                        .backing_trade_fee_policy_count
                        .checked_add(1)
                        .ok_or(PercolatorError::EngineArithmeticOverflow)?;
                } else if old_fee != 0 && new_fee == 0 {
                    cfg.backing_trade_fee_policy_count = cfg
                        .backing_trade_fee_policy_count
                        .checked_sub(1)
                        .ok_or(PercolatorError::EngineCounterUnderflow)?;
                }
                Ok(())
            };
        let mut market_data = market_ai.try_borrow_mut_data()?;
        if asset_index == 0 {
            let old_fee = if long_side {
                cfg.backing_trade_fee_bps_long
            } else {
                cfg.backing_trade_fee_bps_short
            };
            adjust_policy_count(&mut cfg, old_fee, fee_bps)?;
            if long_side {
                cfg.backing_trade_fee_bps_long = fee_bps;
                cfg.backing_trade_fee_insurance_share_bps_long = insurance_share_bps;
            } else {
                cfg.backing_trade_fee_bps_short = fee_bps;
                cfg.backing_trade_fee_insurance_share_bps_short = insurance_share_bps;
            }
            state::write_wrapper_config(&mut market_data, &cfg)
        } else {
            let mut profile = state::read_asset_oracle_profile(&market_data, asset_index)?;
            let old_fee = if long_side {
                profile.backing_trade_fee_bps_long
            } else {
                profile.backing_trade_fee_bps_short
            };
            adjust_policy_count(&mut cfg, old_fee, fee_bps)?;
            if long_side {
                profile.backing_trade_fee_bps_long = fee_bps;
                profile.backing_trade_fee_insurance_share_bps_long = insurance_share_bps;
            } else {
                profile.backing_trade_fee_bps_short = fee_bps;
                profile.backing_trade_fee_insurance_share_bps_short = insurance_share_bps;
            }
            state::write_wrapper_config(&mut market_data, &cfg)?;
            state::write_asset_oracle_profile(&mut market_data, asset_index, &profile)
        }
    }

    #[inline(never)]
    fn handle_update_trade_fee_policy<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        trade_fee_base_bps: u64,
    ) -> ProgramResult {
        let authority = account(accounts, 0)?;
        let market_ai = account(accounts, 1)?;
        expect_signer(authority)?;
        expect_writable(market_ai)?;
        expect_owner(market_ai, program_id)?;
        let (mut cfg, _, _, _, max_trading_fee_bps) =
            state::read_market_trade_preflight(&market_ai.try_borrow_data()?, 0)?;
        expect_live_authority(&cfg.insurance_authority, authority.key)?;
        if trade_fee_base_bps > max_trading_fee_bps
            || trade_fee_base_bps > constants::MAX_DYNAMIC_TRADE_FEE_BPS
        {
            return Err(PercolatorError::InvalidInstruction.into());
        }
        cfg.trade_fee_base_bps = trade_fee_base_bps;
        state::write_wrapper_config(&mut market_ai.try_borrow_mut_data()?, &cfg)
    }

    #[inline(never)]
    fn handle_update_fee_redirect_policy<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        redirect_bps: u16,
    ) -> ProgramResult {
        let admin = account(accounts, 0)?;
        let market_ai = account(accounts, 1)?;
        expect_signer(admin)?;
        expect_writable(market_ai)?;
        expect_owner(market_ai, program_id)?;
        if redirect_bps > 10_000 {
            return Err(PercolatorError::InvalidInstruction.into());
        }
        let (mut cfg, _, _, _) =
            state::read_market_config_mode_and_capacity(&market_ai.try_borrow_data()?)?;
        expect_live_authority(&cfg.admin, admin.key)?;
        cfg.fee_redirect_to_market_0_bps = redirect_bps;
        state::write_wrapper_config(&mut market_ai.try_borrow_mut_data()?, &cfg)
    }

    #[inline(never)]
    fn handle_update_market_init_fee_policy<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        min_init_fee: u128,
    ) -> ProgramResult {
        let admin = account(accounts, 0)?;
        let market_ai = account(accounts, 1)?;
        expect_signer(admin)?;
        expect_writable(market_ai)?;
        expect_owner(market_ai, program_id)?;
        let _ = amount_to_u64(min_init_fee)?;
        let (mut cfg, _, _, _) =
            state::read_market_config_mode_and_capacity(&market_ai.try_borrow_data()?)?;
        expect_live_authority(&cfg.admin, admin.key)?;
        cfg.permissionless_market_init_fee = min_init_fee;
        state::write_wrapper_config(&mut market_ai.try_borrow_mut_data()?, &cfg)
    }

    #[inline(never)]
    fn handle_configure_permissionless_resolve<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        stale_slots: u64,
        force_close_delay_slots: u64,
    ) -> ProgramResult {
        let admin = account(accounts, 0)?;
        let market_ai = account(accounts, 1)?;
        expect_signer(admin)?;
        expect_writable(market_ai)?;
        expect_owner(market_ai, program_id)?;
        if stale_slots == 0
            || stale_slots > constants::MAX_PERMISSIONLESS_RESOLVE_STALE_SLOTS
            || force_close_delay_slots == 0
            || force_close_delay_slots > constants::MAX_FORCE_CLOSE_DELAY_SLOTS
        {
            return Err(PercolatorError::InvalidInstruction.into());
        }
        let (mut cfg, mode, _, _) =
            state::read_market_config_mode_and_capacity(&market_ai.try_borrow_data()?)?;
        expect_live_authority(&cfg.admin, admin.key)?;
        if mode != MarketModeV16::Live {
            return Err(PercolatorError::EngineLockActive.into());
        }
        cfg.permissionless_resolve_stale_slots = stale_slots;
        cfg.force_close_delay_slots = force_close_delay_slots;
        state::write_wrapper_config(&mut market_ai.try_borrow_mut_data()?, &cfg)
    }

    #[inline(never)]
    fn handle_resolve_stale_permissionless<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        now_slot: u64,
    ) -> ProgramResult {
        let market_ai = account(accounts, 0)?;
        expect_writable(market_ai)?;
        expect_owner(market_ai, program_id)?;
        let authenticated_slot = authenticated_slot_or_fallback(now_slot);
        let mut market_data = market_ai.try_borrow_mut_data()?;
        let (cfg, group) = state::market_view_mut(&mut market_data)?;
        if group.header.mode != 0 {
            return Err(PercolatorError::EngineLockActive.into());
        }
        if authenticated_slot < group.header.current_slot.get() {
            return Err(PercolatorError::EngineStale.into());
        }
        if !oracle_v16::permissionless_stale_matured(&cfg, authenticated_slot) {
            return Err(PercolatorError::OracleStale.into());
        }
        group.header.mode = 1;
        group.header.resolved_slot = percolator::V16PodU64::new(authenticated_slot);
        group.header.current_slot = percolator::V16PodU64::new(authenticated_slot);
        group.header.loss_stale_active = 0;
        group.validate_shape().map_err(map_v16_error)?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    #[inline(never)]
    fn handle_configure_hybrid_oracle<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        asset_index: u16,
        now_slot: u64,
        now_unix_ts: i64,
        oracle_leg_count: u8,
        oracle_leg_flags: u8,
        max_staleness_secs: u64,
        hybrid_soft_stale_slots: u64,
        mark_ewma_halflife_slots: u64,
        mark_min_fee: u64,
        invert: u8,
        unit_scale: u32,
        conf_filter_bps: u16,
        oracle_leg_feeds: [[u8; 32]; constants::ORACLE_LEG_CAP],
    ) -> ProgramResult {
        let admin = account(accounts, 0)?;
        let market_ai = account(accounts, 1)?;
        expect_signer(admin)?;
        expect_writable(market_ai)?;
        expect_owner(market_ai, program_id)?;
        if oracle_leg_count == 0
            || oracle_leg_count as usize > constants::ORACLE_LEG_CAP
            || !oracle_v16::oracle_leg_config_ok(
                oracle_leg_count,
                oracle_leg_flags,
                &oracle_leg_feeds,
            )
            || max_staleness_secs == 0
            || hybrid_soft_stale_slots == 0
            || invert > 1
        {
            return Err(PercolatorError::InvalidInstruction.into());
        }
        let oracle_accounts = accounts
            .get(2..2 + oracle_leg_count as usize)
            .ok_or(ProgramError::NotEnoughAccountKeys)?;
        let asset_index_usize = asset_index as usize;
        let authenticated_slot = authenticated_slot_or_fallback(now_slot);
        let authenticated_unix_ts = Clock::get()
            .map(|c| c.unix_timestamp)
            .unwrap_or(now_unix_ts);
        let cfg_after = {
            let mut data = market_ai.try_borrow_mut_data()?;
            let (mut cfg, mut group) = state::market_view_mut(&mut data)?;
            if asset_index_usize >= group.header.config.max_market_slots.get() as usize {
                return Err(PercolatorError::InvalidInstruction.into());
            }
            if authenticated_slot < group.header.current_slot.get() {
                return Err(PercolatorError::EngineStale.into());
            }
            if group.header.mode != 0 {
                return Err(PercolatorError::EngineLockActive.into());
            }
            require_asset_active_for_oracle_reconfiguration_view(&group, asset_index_usize)?;
            let group_had_position_or_loss_state = group_has_position_or_loss_state_view(&group);
            let existing_profile = read_oracle_profile_from_view(&group, &cfg, asset_index_usize)?;
            if asset_index_usize == 0 {
                expect_live_authority(&cfg.admin, admin.key)?;
            } else {
                expect_live_authority(&existing_profile.oracle_authority, admin.key)?;
            }

            let mut profile = state::AssetOracleProfileV16 {
                oracle_mode: constants::ORACLE_MODE_HYBRID_AFTER_HOURS,
                oracle_leg_count,
                oracle_leg_flags,
                invert,
                unit_scale,
                conf_filter_bps,
                backing_trade_fee_bps_long: existing_profile.backing_trade_fee_bps_long,
                backing_trade_fee_bps_short: existing_profile.backing_trade_fee_bps_short,
                backing_trade_fee_insurance_share_bps_long: existing_profile
                    .backing_trade_fee_insurance_share_bps_long,
                backing_trade_fee_insurance_share_bps_short: existing_profile
                    .backing_trade_fee_insurance_share_bps_short,
                _padding0: [0u8; 6],
                insurance_authority: existing_profile.insurance_authority,
                insurance_operator: existing_profile.insurance_operator,
                backing_bucket_authority: existing_profile.backing_bucket_authority,
                oracle_authority: existing_profile.oracle_authority,
                max_staleness_secs,
                hybrid_soft_stale_slots,
                mark_ewma_e6: 0,
                mark_ewma_last_slot: 0,
                mark_ewma_halflife_slots,
                mark_min_fee,
                oracle_target_price_e6: 0,
                oracle_target_publish_time: 0,
                last_good_oracle_slot: 0,
                oracle_leg_feeds,
                oracle_leg_prices_e6: [0u64; constants::ORACLE_LEG_CAP],
                oracle_leg_publish_times: [0i64; constants::ORACLE_LEG_CAP],
            };

            let (price, publish_time, advanced) = oracle_v16::read_external_price_e6_profile(
                &mut profile,
                oracle_accounts,
                authenticated_unix_ts,
            )?;
            if !advanced || price == 0 {
                return Err(PercolatorError::OracleInvalid.into());
            }
            profile.last_good_oracle_slot = authenticated_slot;
            profile.oracle_target_price_e6 = price;
            profile.oracle_target_publish_time = publish_time;
            profile.mark_ewma_e6 = price;
            profile.mark_ewma_last_slot = authenticated_slot;
            let asset = &mut group.markets[asset_index_usize].engine.asset;
            asset.raw_oracle_target_price = percolator::V16PodU64::new(price);
            asset.effective_price = percolator::V16PodU64::new(price);
            asset.fund_px_last = percolator::V16PodU64::new(price);
            asset.slot_last = percolator::V16PodU64::new(authenticated_slot);
            cfg.last_good_oracle_slot =
                core::cmp::max(cfg.last_good_oracle_slot, authenticated_slot);
            if asset_index_usize == 0 {
                cfg.oracle_mode = profile.oracle_mode;
                cfg.oracle_leg_count = profile.oracle_leg_count;
                cfg.oracle_leg_flags = profile.oracle_leg_flags;
                cfg.invert = profile.invert;
                cfg.unit_scale = profile.unit_scale;
                cfg.conf_filter_bps = profile.conf_filter_bps;
                cfg.max_staleness_secs = profile.max_staleness_secs;
                cfg.hybrid_soft_stale_slots = profile.hybrid_soft_stale_slots;
                cfg.mark_ewma_halflife_slots = profile.mark_ewma_halflife_slots;
                cfg.mark_min_fee = profile.mark_min_fee;
                cfg.oracle_leg_feeds = profile.oracle_leg_feeds;
                cfg.oracle_leg_prices_e6 = profile.oracle_leg_prices_e6;
                cfg.oracle_leg_publish_times = profile.oracle_leg_publish_times;
                cfg.oracle_target_price_e6 = profile.oracle_target_price_e6;
                cfg.oracle_target_publish_time = profile.oracle_target_publish_time;
                cfg.mark_ewma_e6 = profile.mark_ewma_e6;
                cfg.mark_ewma_last_slot = profile.mark_ewma_last_slot;
            } else {
                write_oracle_profile_to_view_if_separate(&mut group, asset_index_usize, &profile)?;
            }
            group.header.current_slot = percolator::V16PodU64::new(authenticated_slot);
            if !group_had_position_or_loss_state {
                group.header.slot_last = percolator::V16PodU64::new(authenticated_slot);
            }
            group.validate_shape().map_err(map_v16_error)?;
            cfg
        };
        state::write_wrapper_config(&mut market_ai.try_borrow_mut_data()?, &cfg_after)
    }

    #[inline(never)]
    fn handle_configure_ewma_mark<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        asset_index: u16,
        now_slot: u64,
        initial_mark_e6: u64,
        mark_ewma_halflife_slots: u64,
        mark_min_fee: u64,
    ) -> ProgramResult {
        let admin = account(accounts, 0)?;
        let market_ai = account(accounts, 1)?;
        expect_signer(admin)?;
        expect_writable(market_ai)?;
        expect_owner(market_ai, program_id)?;
        if initial_mark_e6 == 0
            || initial_mark_e6 > percolator::MAX_ORACLE_PRICE
            || mark_ewma_halflife_slots == 0
        {
            return Err(PercolatorError::InvalidInstruction.into());
        }
        let asset_index_usize = asset_index as usize;
        let authenticated_slot = authenticated_slot_or_fallback(now_slot);
        let cfg_after = {
            let mut data = market_ai.try_borrow_mut_data()?;
            let (mut cfg, mut group) = state::market_view_mut(&mut data)?;
            if asset_index_usize >= group.header.config.max_market_slots.get() as usize {
                return Err(PercolatorError::InvalidInstruction.into());
            }
            if authenticated_slot < group.header.current_slot.get() {
                return Err(PercolatorError::EngineStale.into());
            }
            if group.header.mode != 0 {
                return Err(PercolatorError::EngineLockActive.into());
            }
            require_asset_active_for_oracle_reconfiguration_view(&group, asset_index_usize)?;
            let group_had_position_or_loss_state = group_has_position_or_loss_state_view(&group);
            let existing_profile = read_oracle_profile_from_view(&group, &cfg, asset_index_usize)?;
            if asset_index_usize == 0 {
                expect_live_authority(&cfg.admin, admin.key)?;
            } else {
                expect_live_authority(&existing_profile.oracle_authority, admin.key)?;
            }

            let profile = state::AssetOracleProfileV16 {
                oracle_mode: constants::ORACLE_MODE_EWMA_MARK,
                oracle_leg_count: 0,
                oracle_leg_flags: 0,
                invert: 0,
                unit_scale: 0,
                conf_filter_bps: 0,
                backing_trade_fee_bps_long: existing_profile.backing_trade_fee_bps_long,
                backing_trade_fee_bps_short: existing_profile.backing_trade_fee_bps_short,
                backing_trade_fee_insurance_share_bps_long: existing_profile
                    .backing_trade_fee_insurance_share_bps_long,
                backing_trade_fee_insurance_share_bps_short: existing_profile
                    .backing_trade_fee_insurance_share_bps_short,
                _padding0: [0u8; 6],
                insurance_authority: existing_profile.insurance_authority,
                insurance_operator: existing_profile.insurance_operator,
                backing_bucket_authority: existing_profile.backing_bucket_authority,
                oracle_authority: existing_profile.oracle_authority,
                max_staleness_secs: 0,
                hybrid_soft_stale_slots: 0,
                mark_ewma_e6: initial_mark_e6,
                mark_ewma_last_slot: authenticated_slot,
                mark_ewma_halflife_slots,
                mark_min_fee,
                oracle_target_price_e6: initial_mark_e6,
                oracle_target_publish_time: 0,
                last_good_oracle_slot: authenticated_slot,
                oracle_leg_feeds: [[0u8; 32]; constants::ORACLE_LEG_CAP],
                oracle_leg_prices_e6: [0u64; constants::ORACLE_LEG_CAP],
                oracle_leg_publish_times: [0i64; constants::ORACLE_LEG_CAP],
            };

            let asset = &mut group.markets[asset_index_usize].engine.asset;
            asset.raw_oracle_target_price = percolator::V16PodU64::new(initial_mark_e6);
            asset.effective_price = percolator::V16PodU64::new(initial_mark_e6);
            asset.fund_px_last = percolator::V16PodU64::new(initial_mark_e6);
            asset.slot_last = percolator::V16PodU64::new(authenticated_slot);
            cfg.last_good_oracle_slot =
                core::cmp::max(cfg.last_good_oracle_slot, authenticated_slot);
            if asset_index_usize == 0 {
                cfg.oracle_mode = profile.oracle_mode;
                cfg.oracle_leg_count = 0;
                cfg.oracle_leg_flags = 0;
                cfg.invert = 0;
                cfg.unit_scale = 0;
                cfg.conf_filter_bps = 0;
                cfg.max_staleness_secs = 0;
                cfg.hybrid_soft_stale_slots = 0;
                cfg.mark_ewma_e6 = profile.mark_ewma_e6;
                cfg.mark_ewma_last_slot = profile.mark_ewma_last_slot;
                cfg.mark_ewma_halflife_slots = profile.mark_ewma_halflife_slots;
                cfg.mark_min_fee = profile.mark_min_fee;
                cfg.oracle_target_price_e6 = profile.oracle_target_price_e6;
                cfg.oracle_target_publish_time = 0;
                cfg.oracle_leg_feeds = [[0u8; 32]; constants::ORACLE_LEG_CAP];
                cfg.oracle_leg_prices_e6 = [0u64; constants::ORACLE_LEG_CAP];
                cfg.oracle_leg_publish_times = [0i64; constants::ORACLE_LEG_CAP];
            } else {
                write_oracle_profile_to_view_if_separate(&mut group, asset_index_usize, &profile)?;
            }
            group.header.current_slot = percolator::V16PodU64::new(authenticated_slot);
            if !group_had_position_or_loss_state {
                group.header.slot_last = percolator::V16PodU64::new(authenticated_slot);
            }
            group.validate_shape().map_err(map_v16_error)?;
            cfg
        };
        state::write_wrapper_config(&mut market_ai.try_borrow_mut_data()?, &cfg_after)
    }

    #[inline(never)]
    fn handle_configure_auth_mark<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        asset_index: u16,
        now_slot: u64,
        initial_mark_e6: u64,
    ) -> ProgramResult {
        let authority = account(accounts, 0)?;
        let market_ai = account(accounts, 1)?;
        expect_signer(authority)?;
        expect_writable(market_ai)?;
        expect_owner(market_ai, program_id)?;
        if initial_mark_e6 == 0 || initial_mark_e6 > percolator::MAX_ORACLE_PRICE {
            return Err(PercolatorError::InvalidInstruction.into());
        }
        let asset_index_usize = asset_index as usize;
        let authenticated_slot = authenticated_slot_or_fallback(now_slot);
        let cfg_after = {
            let mut data = market_ai.try_borrow_mut_data()?;
            let (mut cfg, mut group) = state::market_view_mut(&mut data)?;
            if asset_index_usize >= group.header.config.max_market_slots.get() as usize {
                return Err(PercolatorError::InvalidInstruction.into());
            }
            if authenticated_slot < group.header.current_slot.get() {
                return Err(PercolatorError::EngineStale.into());
            }
            if group.header.mode != 0 {
                return Err(PercolatorError::EngineLockActive.into());
            }
            require_asset_active_for_oracle_reconfiguration_view(&group, asset_index_usize)?;
            let group_had_position_or_loss_state = group_has_position_or_loss_state_view(&group);
            let existing_profile = read_oracle_profile_from_view(&group, &cfg, asset_index_usize)?;
            if asset_index_usize == 0 {
                expect_live_authority(&cfg.admin, authority.key)?;
            } else {
                expect_live_authority(&existing_profile.oracle_authority, authority.key)?;
            }

            let profile = state::AssetOracleProfileV16 {
                oracle_mode: constants::ORACLE_MODE_AUTH_MARK,
                oracle_leg_count: 0,
                oracle_leg_flags: 0,
                invert: 0,
                unit_scale: 0,
                conf_filter_bps: 0,
                backing_trade_fee_bps_long: existing_profile.backing_trade_fee_bps_long,
                backing_trade_fee_bps_short: existing_profile.backing_trade_fee_bps_short,
                backing_trade_fee_insurance_share_bps_long: existing_profile
                    .backing_trade_fee_insurance_share_bps_long,
                backing_trade_fee_insurance_share_bps_short: existing_profile
                    .backing_trade_fee_insurance_share_bps_short,
                _padding0: [0u8; 6],
                insurance_authority: existing_profile.insurance_authority,
                insurance_operator: existing_profile.insurance_operator,
                backing_bucket_authority: existing_profile.backing_bucket_authority,
                oracle_authority: existing_profile.oracle_authority,
                max_staleness_secs: 0,
                hybrid_soft_stale_slots: 0,
                mark_ewma_e6: initial_mark_e6,
                mark_ewma_last_slot: authenticated_slot,
                mark_ewma_halflife_slots: 0,
                mark_min_fee: 0,
                oracle_target_price_e6: initial_mark_e6,
                oracle_target_publish_time: 0,
                last_good_oracle_slot: authenticated_slot,
                oracle_leg_feeds: [[0u8; 32]; constants::ORACLE_LEG_CAP],
                oracle_leg_prices_e6: [0u64; constants::ORACLE_LEG_CAP],
                oracle_leg_publish_times: [0i64; constants::ORACLE_LEG_CAP],
            };

            let asset = &mut group.markets[asset_index_usize].engine.asset;
            asset.raw_oracle_target_price = percolator::V16PodU64::new(initial_mark_e6);
            asset.effective_price = percolator::V16PodU64::new(initial_mark_e6);
            asset.fund_px_last = percolator::V16PodU64::new(initial_mark_e6);
            asset.slot_last = percolator::V16PodU64::new(authenticated_slot);
            cfg.last_good_oracle_slot =
                core::cmp::max(cfg.last_good_oracle_slot, authenticated_slot);
            if asset_index_usize == 0 {
                cfg.oracle_mode = profile.oracle_mode;
                cfg.oracle_leg_count = 0;
                cfg.oracle_leg_flags = 0;
                cfg.invert = 0;
                cfg.unit_scale = 0;
                cfg.conf_filter_bps = 0;
                cfg.max_staleness_secs = 0;
                cfg.hybrid_soft_stale_slots = 0;
                cfg.mark_ewma_e6 = profile.mark_ewma_e6;
                cfg.mark_ewma_last_slot = profile.mark_ewma_last_slot;
                cfg.mark_ewma_halflife_slots = 0;
                cfg.mark_min_fee = 0;
                cfg.oracle_target_price_e6 = profile.oracle_target_price_e6;
                cfg.oracle_target_publish_time = 0;
                cfg.oracle_leg_feeds = [[0u8; 32]; constants::ORACLE_LEG_CAP];
                cfg.oracle_leg_prices_e6 = [0u64; constants::ORACLE_LEG_CAP];
                cfg.oracle_leg_publish_times = [0i64; constants::ORACLE_LEG_CAP];
            } else {
                write_oracle_profile_to_view_if_separate(&mut group, asset_index_usize, &profile)?;
            }
            group.header.current_slot = percolator::V16PodU64::new(authenticated_slot);
            if !group_had_position_or_loss_state {
                group.header.slot_last = percolator::V16PodU64::new(authenticated_slot);
            }
            group.validate_shape().map_err(map_v16_error)?;
            cfg
        };
        state::write_wrapper_config(&mut market_ai.try_borrow_mut_data()?, &cfg_after)
    }

    #[inline(never)]
    fn handle_push_ewma_mark<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        asset_index: u16,
        now_slot: u64,
        mark_e6: u64,
    ) -> ProgramResult {
        let authority = account(accounts, 0)?;
        let market_ai = account(accounts, 1)?;
        expect_signer(authority)?;
        expect_writable(market_ai)?;
        expect_owner(market_ai, program_id)?;
        if mark_e6 == 0 || mark_e6 > percolator::MAX_ORACLE_PRICE {
            return Err(PercolatorError::OracleInvalid.into());
        }
        let asset_index_usize = asset_index as usize;
        let authenticated_slot = authenticated_slot_or_fallback(now_slot);
        let cfg_after = {
            let mut data = market_ai.try_borrow_mut_data()?;
            let (mut cfg, mut group) = state::market_view_mut(&mut data)?;
            if asset_index_usize >= group.header.config.max_market_slots.get() as usize {
                return Err(PercolatorError::InvalidInstruction.into());
            }
            let mut profile = read_oracle_profile_from_view(&group, &cfg, asset_index_usize)?;
            if group.header.mode != 0 {
                return Err(PercolatorError::EngineLockActive.into());
            }
            require_asset_mark_pushable_view(&group, asset_index_usize)?;
            if !oracle_v16::profile_is_ewma_mark(&profile) {
                return Err(PercolatorError::Unauthorized.into());
            }
            let authorities = domain_authorities_from_profile(&cfg, &profile, asset_index_usize);
            expect_live_authority(&authorities.oracle_authority, authority.key)?;
            if authenticated_slot < profile.mark_ewma_last_slot
                || authenticated_slot < group.header.current_slot.get()
            {
                return Err(PercolatorError::EngineStale.into());
            }
            let full_weight_fee = if profile.mark_min_fee == 0 {
                0
            } else {
                profile.mark_min_fee
            };
            let next_mark = policy_v16::ewma_update(
                profile.mark_ewma_e6,
                mark_e6,
                profile.mark_ewma_halflife_slots,
                profile.mark_ewma_last_slot,
                authenticated_slot,
                full_weight_fee,
                profile.mark_min_fee,
            );
            if next_mark == 0 || next_mark > percolator::MAX_ORACLE_PRICE {
                return Err(PercolatorError::OracleInvalid.into());
            }
            profile.mark_ewma_e6 = next_mark;
            profile.mark_ewma_last_slot = authenticated_slot;
            profile.oracle_target_price_e6 = next_mark;
            profile.oracle_target_publish_time = 0;
            profile.last_good_oracle_slot = authenticated_slot;
            cfg.last_good_oracle_slot =
                core::cmp::max(cfg.last_good_oracle_slot, authenticated_slot);
            if asset_index_usize == 0 {
                cfg.mark_ewma_e6 = profile.mark_ewma_e6;
                cfg.mark_ewma_last_slot = profile.mark_ewma_last_slot;
                cfg.oracle_target_price_e6 = profile.oracle_target_price_e6;
                cfg.oracle_target_publish_time = 0;
            } else {
                write_oracle_profile_to_view_if_separate(&mut group, asset_index_usize, &profile)?;
            }
            group.validate_shape().map_err(map_v16_error)?;
            cfg
        };
        state::write_wrapper_config(&mut market_ai.try_borrow_mut_data()?, &cfg_after)
    }

    #[inline(never)]
    fn handle_push_auth_mark<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        asset_index: u16,
        now_slot: u64,
        mark_e6: u64,
    ) -> ProgramResult {
        let authority = account(accounts, 0)?;
        let market_ai = account(accounts, 1)?;
        expect_signer(authority)?;
        expect_writable(market_ai)?;
        expect_owner(market_ai, program_id)?;
        if mark_e6 == 0 || mark_e6 > percolator::MAX_ORACLE_PRICE {
            return Err(PercolatorError::OracleInvalid.into());
        }
        let asset_index_usize = asset_index as usize;
        let authenticated_slot = authenticated_slot_or_fallback(now_slot);
        let cfg_after = {
            let mut data = market_ai.try_borrow_mut_data()?;
            let (mut cfg, mut group) = state::market_view_mut(&mut data)?;
            if asset_index_usize >= group.header.config.max_market_slots.get() as usize {
                return Err(PercolatorError::InvalidInstruction.into());
            }
            let mut profile = read_oracle_profile_from_view(&group, &cfg, asset_index_usize)?;
            if group.header.mode != 0 {
                return Err(PercolatorError::EngineLockActive.into());
            }
            require_asset_mark_pushable_view(&group, asset_index_usize)?;
            if !oracle_v16::profile_is_auth_mark(&profile) {
                return Err(PercolatorError::Unauthorized.into());
            }
            let authorities = domain_authorities_from_profile(&cfg, &profile, asset_index_usize);
            expect_live_authority(&authorities.oracle_authority, authority.key)?;
            if authenticated_slot < profile.mark_ewma_last_slot
                || authenticated_slot < group.header.current_slot.get()
            {
                return Err(PercolatorError::EngineStale.into());
            }
            profile.mark_ewma_e6 = mark_e6;
            profile.mark_ewma_last_slot = authenticated_slot;
            profile.oracle_target_price_e6 = mark_e6;
            profile.oracle_target_publish_time = 0;
            profile.last_good_oracle_slot = authenticated_slot;
            cfg.last_good_oracle_slot =
                core::cmp::max(cfg.last_good_oracle_slot, authenticated_slot);
            if asset_index_usize == 0 {
                cfg.mark_ewma_e6 = profile.mark_ewma_e6;
                cfg.mark_ewma_last_slot = profile.mark_ewma_last_slot;
                cfg.mark_ewma_halflife_slots = 0;
                cfg.mark_min_fee = 0;
                cfg.oracle_target_price_e6 = profile.oracle_target_price_e6;
                cfg.oracle_target_publish_time = 0;
            } else {
                write_oracle_profile_to_view_if_separate(&mut group, asset_index_usize, &profile)?;
            }
            group.validate_shape().map_err(map_v16_error)?;
            cfg
        };
        state::write_wrapper_config(&mut market_ai.try_borrow_mut_data()?, &cfg_after)
    }

    #[inline(never)]
    fn handle_close_resolved<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        _fee_rate_per_slot: u128,
    ) -> ProgramResult {
        let owner = account(accounts, 0)?;
        let market_ai = account(accounts, 1)?;
        let portfolio_ai = account(accounts, 2)?;
        expect_writable(market_ai)?;
        expect_writable(portfolio_ai)?;
        expect_owner(market_ai, program_id)?;
        expect_owner(portfolio_ai, program_id)?;

        let (cfg_after, payout) = {
            let mut market_data = market_ai.try_borrow_mut_data()?;
            let (cfg, mut group) = state::market_view_mut(&mut market_data)?;
            let max_market_slots = group.header.config.max_market_slots.get() as usize;
            ensure_portfolio_storage_for_market_slots(portfolio_ai, max_market_slots)?;
            let mut portfolio_data = portfolio_ai.try_borrow_mut_data()?;
            let mut portfolio =
                state::portfolio_view_mut_for_market_slots(&mut portfolio_data, max_market_slots)?;
            expect_portfolio_view_account_key(&portfolio, portfolio_ai.key)?;
            expect_portfolio_view_owner(&portfolio, owner.key)?;
            if group.header.mode != 1 {
                return Err(PercolatorError::EngineLockActive.into());
            }
            if cfg.force_close_delay_slots != 0
                && authenticated_market_slot_or_fallback_view(&group)
                    .saturating_sub(group.header.resolved_slot.get())
                    < cfg.force_close_delay_slots
            {
                expect_signer(owner)?;
            }
            let outcome = group
                .close_resolved_account_not_atomic(&mut portfolio, cfg.maintenance_fee_per_slot)
                .map_err(map_v16_error)?;
            let payout = match outcome {
                percolator::ResolvedCloseOutcomeV16::ProgressOnly => 0,
                percolator::ResolvedCloseOutcomeV16::Closed { payout } => payout,
            };
            (cfg, payout)
        };
        if payout != 0 {
            let dest_token = account(accounts, 3)?;
            let vault_token = account(accounts, 4)?;
            let vault_authority_ai = account(accounts, 5)?;
            let token_program = account(accounts, 6)?;
            expect_writable(dest_token)?;
            expect_writable(vault_token)?;
            verify_token_program(token_program)?;
            let (vault_authority, bump) = derive_vault_authority(program_id, market_ai.key);
            expect_key(vault_authority_ai, &vault_authority)?;
            verify_withdrawable_token_accounts(
                dest_token,
                owner.key,
                vault_token,
                &vault_authority,
                &cfg_after,
            )?;
            let payout_u64 = amount_to_u64(payout)?;
            require_token_balance(vault_token, payout_u64)?;
            let bump_arr = [bump];
            let signer_seeds: &[&[&[u8]]] = &[&[b"vault", market_ai.key.as_ref(), &bump_arr]];
            transfer_tokens_signed(
                token_program,
                vault_token,
                dest_token,
                vault_authority_ai,
                payout_u64,
                signer_seeds,
            )?;
        }
        Ok(())
    }

    #[inline(never)]
    fn handle_claim_resolved_payout_topup<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
    ) -> ProgramResult {
        let owner = account(accounts, 0)?;
        let market_ai = account(accounts, 1)?;
        let portfolio_ai = account(accounts, 2)?;
        expect_writable(market_ai)?;
        expect_writable(portfolio_ai)?;
        expect_owner(market_ai, program_id)?;
        expect_owner(portfolio_ai, program_id)?;

        let (_, _, max_market_slots, _) =
            state::read_market_config_mode_and_capacity(&market_ai.try_borrow_data()?)?;
        ensure_portfolio_storage_for_market_slots(portfolio_ai, max_market_slots)?;
        let (cfg, payout) = {
            let mut market_data = market_ai.try_borrow_mut_data()?;
            let (cfg, group) = state::market_view_mut(&mut market_data)?;
            let mut portfolio_data = portfolio_ai.try_borrow_mut_data()?;
            let portfolio =
                state::portfolio_view_mut_for_market_slots(&mut portfolio_data, max_market_slots)?;
            expect_portfolio_view_account_key(&portfolio, portfolio_ai.key)?;
            expect_portfolio_view_owner(&portfolio, owner.key)?;
            portfolio
                .validate_with_market(&group.as_view())
                .map_err(map_v16_error)?;
            if group.header.mode != 1 || group.header.payout_snapshot_captured == 0 {
                return Err(PercolatorError::EngineLockActive.into());
            }
            let receipt = portfolio
                .header
                .resolved_payout_receipt
                .try_to_runtime()
                .map_err(map_v16_error)?;
            let payout =
                resolved_receipt_claimable_now_view(&group, receipt)?.min(group.header.vault.get());
            if payout != 0 {
                portfolio.header.resolved_payout_receipt.paid_effective =
                    percolator::V16PodU128::new(
                        receipt
                            .paid_effective
                            .checked_add(payout)
                            .ok_or(PercolatorError::EngineArithmeticOverflow)?,
                    );
                if portfolio
                    .header
                    .resolved_payout_receipt
                    .paid_effective
                    .get()
                    == receipt.terminal_positive_claim_face
                {
                    portfolio.header.resolved_payout_receipt.finalized = 1;
                }
                let vault_before = group.header.vault.get();
                group.header.vault = percolator::V16PodU128::new(
                    vault_before
                        .checked_sub(payout)
                        .ok_or(PercolatorError::EngineCounterUnderflow)?,
                );
                percolator::TokenValueFlowProofV16::capital_and_resolved_payout_to_external_out(
                    0,
                    payout,
                    payout,
                    vault_before,
                    group.header.vault.get(),
                )
                .map_err(map_v16_error)?
                .validate()
                .map_err(map_v16_error)?;
                group.validate_shape().map_err(map_v16_error)?;
                portfolio
                    .validate_with_market(&group.as_view())
                    .map_err(map_v16_error)?;
            }
            (cfg, payout)
        };
        if payout != 0 {
            let dest_token = account(accounts, 3)?;
            let vault_token = account(accounts, 4)?;
            let vault_authority_ai = account(accounts, 5)?;
            let token_program = account(accounts, 6)?;
            expect_writable(dest_token)?;
            expect_writable(vault_token)?;
            verify_token_program(token_program)?;
            let (vault_authority, bump) = derive_vault_authority(program_id, market_ai.key);
            expect_key(vault_authority_ai, &vault_authority)?;
            verify_withdrawable_token_accounts(
                dest_token,
                owner.key,
                vault_token,
                &vault_authority,
                &cfg,
            )?;
            let payout_u64 = amount_to_u64(payout)?;
            require_token_balance(vault_token, payout_u64)?;
            let bump_arr = [bump];
            let signer_seeds: &[&[&[u8]]] = &[&[b"vault", market_ai.key.as_ref(), &bump_arr]];
            transfer_tokens_signed(
                token_program,
                vault_token,
                dest_token,
                vault_authority_ai,
                payout_u64,
                signer_seeds,
            )?;
        }
        let _ = cfg;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    #[inline(never)]
    fn handle_permissionless_crank_zero_copy<'a>(
        program_id: &Pubkey,
        owner: &AccountInfo<'a>,
        market_ai: &AccountInfo<'a>,
        portfolio_ai: &AccountInfo<'a>,
        tail: &[AccountInfo<'a>],
        action: u8,
        asset_index: u16,
        now_slot: u64,
        funding_rate_e9: i128,
        close_q: u128,
        fee_bps: u64,
        recovery_reason: u8,
        max_market_slots: usize,
    ) -> ProgramResult {
        if funding_rate_e9 != 0 || recovery_reason != 0 {
            return Err(PercolatorError::InvalidInstruction.into());
        }
        if action == 1 && fee_bps != 0 {
            return Err(PercolatorError::InvalidInstruction.into());
        }
        if action > 2 {
            return Err(PercolatorError::InvalidInstruction.into());
        }
        ensure_portfolio_storage_for_market_slots(portfolio_ai, max_market_slots)?;
        let authenticated_now_slot = authenticated_slot_or_fallback(now_slot);
        let asset_index_usize = asset_index as usize;
        let cfg_after;
        {
            let mut market_data = market_ai.try_borrow_mut_data()?;
            let (mut cfg, mut group) = state::market_view_mut(&mut market_data)?;
            if asset_index_usize >= group.header.config.max_market_slots.get() as usize {
                return Err(PercolatorError::InvalidInstruction.into());
            }
            let crank_action = match action {
                0 => PermissionlessCrankActionV16::Refresh,
                1 => PermissionlessCrankActionV16::Liquidate(percolator::LiquidationRequestV16 {
                    asset_index: asset_index_usize,
                    close_q,
                    fee_bps: group.header.config.liquidation_fee_bps.get(),
                }),
                2 => PermissionlessCrankActionV16::SettleB {
                    asset_index: asset_index_usize,
                },
                _ => return Err(PercolatorError::InvalidInstruction.into()),
            };
            let mut oracle_profile =
                read_oracle_profile_from_view(&group, &cfg, asset_index_usize)?;
            let now_unix_ts = Clock::get().map(|c| c.unix_timestamp).unwrap_or_else(|_| {
                let elapsed_slots =
                    authenticated_now_slot.saturating_sub(oracle_profile.last_good_oracle_slot);
                oracle_profile
                    .oracle_target_publish_time
                    .saturating_add(i64::try_from(elapsed_slots).unwrap_or(i64::MAX))
            });
            let reward_enabled = action == 1 && cfg.liquidation_cranker_fee_share_bps != 0;
            let mut oracle_tail = tail;
            let mut cranker_portfolio_ai = None;
            if reward_enabled {
                if let Some((last, rest)) = tail.split_last() {
                    if last.owner == program_id {
                        expect_signer(owner)?;
                        expect_writable(last)?;
                        if last.key == portfolio_ai.key {
                            return Err(PercolatorError::InvalidInstruction.into());
                        }
                        ensure_portfolio_storage_for_market_slots(last, max_market_slots)?;
                        cranker_portfolio_ai = Some(last);
                        oracle_tail = rest;
                    }
                }
            }
            reject_permissionless_resolve_matured_live_for_profile_view(
                &cfg,
                &oracle_profile,
                &group,
            )?;
            let crank_price = hybrid_effective_price_for_crank_view(
                &cfg,
                &mut oracle_profile,
                &group,
                asset_index_usize,
                authenticated_now_slot,
                now_unix_ts,
                oracle_tail,
            )?;
            group.markets[asset_index_usize]
                .engine
                .asset
                .raw_oracle_target_price =
                percolator::V16PodU64::new(oracle_profile.oracle_target_price_e6);
            cfg.last_good_oracle_slot = core::cmp::max(
                cfg.last_good_oracle_slot,
                oracle_profile.last_good_oracle_slot,
            );
            if asset_index_usize == 0 && oracle_v16::profile_is_price_managed(&oracle_profile) {
                cfg.oracle_mode = oracle_profile.oracle_mode;
                cfg.oracle_leg_count = oracle_profile.oracle_leg_count;
                cfg.oracle_leg_flags = oracle_profile.oracle_leg_flags;
                cfg.invert = oracle_profile.invert;
                cfg.unit_scale = oracle_profile.unit_scale;
                cfg.conf_filter_bps = oracle_profile.conf_filter_bps;
                cfg.max_staleness_secs = oracle_profile.max_staleness_secs;
                cfg.hybrid_soft_stale_slots = oracle_profile.hybrid_soft_stale_slots;
                cfg.mark_ewma_e6 = oracle_profile.mark_ewma_e6;
                cfg.mark_ewma_last_slot = oracle_profile.mark_ewma_last_slot;
                cfg.mark_ewma_halflife_slots = oracle_profile.mark_ewma_halflife_slots;
                cfg.mark_min_fee = oracle_profile.mark_min_fee;
                cfg.oracle_target_price_e6 = oracle_profile.oracle_target_price_e6;
                cfg.oracle_target_publish_time = oracle_profile.oracle_target_publish_time;
                cfg.oracle_leg_feeds = oracle_profile.oracle_leg_feeds;
                cfg.oracle_leg_prices_e6 = oracle_profile.oracle_leg_prices_e6;
                cfg.oracle_leg_publish_times = oracle_profile.oracle_leg_publish_times;
            } else {
                write_oracle_profile_to_view_if_separate(
                    &mut group,
                    asset_index_usize,
                    &oracle_profile,
                )?;
            }

            let mut portfolio_data = portfolio_ai.try_borrow_mut_data()?;
            let mut portfolio =
                state::portfolio_view_mut_for_market_slots(&mut portfolio_data, max_market_slots)?;
            expect_portfolio_view_account_key(&portfolio, portfolio_ai.key)?;
            let insurance_before = group.header.insurance.get();
            let is_liquidation = matches!(crank_action, PermissionlessCrankActionV16::Liquidate(_));
            if let Some(cranker_ai) = cranker_portfolio_ai {
                let mut cranker_data = cranker_ai.try_borrow_mut_data()?;
                let cranker = state::portfolio_view_mut_for_market_slots(
                    &mut cranker_data,
                    max_market_slots,
                )?;
                expect_portfolio_view_account_key(&cranker, cranker_ai.key)?;
                expect_portfolio_view_owner(&cranker, owner.key)?;
                cranker
                    .validate_with_market(&group.as_view())
                    .map_err(map_v16_error)?;
                if let PermissionlessCrankActionV16::Liquidate(liq) = crank_action {
                    group
                        .accrue_asset_to_not_atomic(
                            asset_index_usize,
                            authenticated_now_slot,
                            crank_price,
                            funding_rate_e9,
                            true,
                        )
                        .map_err(map_v16_error)?;
                    group
                        .liquidate_account_not_atomic(&mut portfolio, liq)
                        .map_err(map_v16_error)?;
                } else {
                    group
                        .permissionless_crank_not_atomic(
                            &mut portfolio,
                            PermissionlessCrankRequestV16 {
                                now_slot: authenticated_now_slot,
                                asset_index: asset_index_usize,
                                effective_price: crank_price,
                                funding_rate_e9,
                                action: crank_action,
                            },
                        )
                        .map_err(map_v16_error)?;
                }
                let retained_fee = group
                    .header
                    .insurance
                    .get()
                    .saturating_sub(insurance_before);
                let reward = retained_fee
                    .checked_mul(cfg.liquidation_cranker_fee_share_bps as u128)
                    .ok_or(PercolatorError::EngineArithmeticOverflow)?
                    / 10_000;
                let reward = core::cmp::min(reward, retained_fee);
                if reward != 0 {
                    group.header.insurance = percolator::V16PodU128::new(
                        group
                            .header
                            .insurance
                            .get()
                            .checked_sub(reward)
                            .ok_or(PercolatorError::EngineArithmeticOverflow)?,
                    );
                    group.header.c_tot = percolator::V16PodU128::new(
                        group
                            .header
                            .c_tot
                            .get()
                            .checked_add(reward)
                            .ok_or(PercolatorError::EngineArithmeticOverflow)?,
                    );
                    cranker.header.capital = percolator::V16PodU128::new(
                        cranker
                            .header
                            .capital
                            .get()
                            .checked_add(reward)
                            .ok_or(PercolatorError::EngineArithmeticOverflow)?,
                    );
                    cranker.header.health_cert.valid = 0;
                }
                let retained_after_reward = retained_fee
                    .checked_sub(reward)
                    .ok_or(PercolatorError::EngineCounterUnderflow)?;
                credit_market_fee_split_across_domains_view(
                    &cfg,
                    &mut group,
                    asset_index_usize,
                    retained_after_reward,
                )?;
                group.validate_shape().map_err(map_v16_error)?;
                cranker
                    .validate_with_market(&group.as_view())
                    .map_err(map_v16_error)?;
            } else {
                if let PermissionlessCrankActionV16::Liquidate(liq) = crank_action {
                    group
                        .accrue_asset_to_not_atomic(
                            asset_index_usize,
                            authenticated_now_slot,
                            crank_price,
                            funding_rate_e9,
                            true,
                        )
                        .map_err(map_v16_error)?;
                    group
                        .liquidate_account_not_atomic(&mut portfolio, liq)
                        .map_err(map_v16_error)?;
                } else {
                    group
                        .permissionless_crank_not_atomic(
                            &mut portfolio,
                            PermissionlessCrankRequestV16 {
                                now_slot: authenticated_now_slot,
                                asset_index: asset_index_usize,
                                effective_price: crank_price,
                                funding_rate_e9,
                                action: crank_action,
                            },
                        )
                        .map_err(map_v16_error)?;
                }
                let retained_fee = group
                    .header
                    .insurance
                    .get()
                    .saturating_sub(insurance_before);
                credit_market_fee_split_across_domains_view(
                    &cfg,
                    &mut group,
                    asset_index_usize,
                    retained_fee,
                )?;
                if is_liquidation {
                    group.validate_shape().map_err(map_v16_error)?;
                }
            }
            cfg_after = cfg;
        }
        state::write_wrapper_config(&mut market_ai.try_borrow_mut_data()?, &cfg_after)?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    #[inline(never)]
    fn handle_permissionless_crank<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        action: u8,
        asset_index: u16,
        now_slot: u64,
        funding_rate_e9: i128,
        close_q: u128,
        fee_bps: u64,
        recovery_reason: u8,
    ) -> ProgramResult {
        let owner = account(accounts, 0)?;
        let market_ai = account(accounts, 1)?;
        let portfolio_ai = account(accounts, 2)?;
        expect_writable(market_ai)?;
        expect_writable(portfolio_ai)?;
        expect_owner(market_ai, program_id)?;
        expect_owner(portfolio_ai, program_id)?;
        let (_, _, max_market_slots, _) =
            state::read_market_config_mode_and_capacity(&market_ai.try_borrow_data()?)?;
        handle_permissionless_crank_zero_copy(
            program_id,
            owner,
            market_ai,
            portfolio_ai,
            accounts.get(3..).unwrap_or(&[]),
            action,
            asset_index,
            now_slot,
            funding_rate_e9,
            close_q,
            fee_bps,
            recovery_reason,
            max_market_slots,
        )
    }

    fn account<'a>(
        accounts: &'a [AccountInfo<'a>],
        idx: usize,
    ) -> Result<&'a AccountInfo<'a>, ProgramError> {
        accounts.get(idx).ok_or(ProgramError::NotEnoughAccountKeys)
    }

    fn expect_signer(ai: &AccountInfo) -> Result<(), ProgramError> {
        if !ai.is_signer {
            return Err(PercolatorError::ExpectedSigner.into());
        }
        Ok(())
    }

    fn expect_writable(ai: &AccountInfo) -> Result<(), ProgramError> {
        if !ai.is_writable {
            return Err(PercolatorError::ExpectedWritable.into());
        }
        Ok(())
    }

    fn expect_owner(ai: &AccountInfo, owner: &Pubkey) -> Result<(), ProgramError> {
        if ai.owner != owner {
            return Err(ProgramError::IncorrectProgramId);
        }
        Ok(())
    }

    fn expect_live_authority(expected: &[u8; 32], signer: &Pubkey) -> Result<(), ProgramError> {
        if !live_authority_matches(expected, signer) {
            return Err(PercolatorError::Unauthorized.into());
        }
        Ok(())
    }

    fn live_authority_matches(expected: &[u8; 32], signer: &Pubkey) -> bool {
        *expected != [0u8; 32] && *expected == signer.to_bytes()
    }

    fn expect_portfolio_view_owner(
        portfolio: &percolator::PortfolioV16ViewMut<'_>,
        owner: &Pubkey,
    ) -> Result<(), ProgramError> {
        if portfolio.header.owner != owner.to_bytes() {
            return Err(PercolatorError::Unauthorized.into());
        }
        Ok(())
    }

    fn expect_portfolio_view_account_key(
        portfolio: &percolator::PortfolioV16ViewMut<'_>,
        key: &Pubkey,
    ) -> Result<(), ProgramError> {
        let header = portfolio
            .header
            .provenance_header
            .try_to_runtime()
            .map_err(map_v16_error)?;
        if header.portfolio_account_id != key.to_bytes() {
            return Err(PercolatorError::EngineProvenanceMismatch.into());
        }
        Ok(())
    }

    fn ensure_portfolio_storage_for_market_slots(
        portfolio_ai: &AccountInfo,
        max_market_slots: usize,
    ) -> ProgramResult {
        let required = state::portfolio_account_len_for_market_slots(max_market_slots)?;
        if portfolio_ai.data_len() < required {
            portfolio_ai.realloc(required, true)?;
        }
        Ok(())
    }

    fn with_one_portfolio_view<'a, F>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        owner_must_sign: bool,
        f: F,
    ) -> ProgramResult
    where
        for<'m, 'p> F: FnOnce(
            &mut state::MarketViewMutV16<'m>,
            &mut percolator::PortfolioV16ViewMut<'p>,
            &WrapperConfigV16,
        ) -> Result<(), V16Error>,
    {
        let owner = account(accounts, 0)?;
        let market_ai = account(accounts, 1)?;
        let portfolio_ai = account(accounts, 2)?;
        if owner_must_sign {
            expect_signer(owner)?;
        }
        expect_writable(market_ai)?;
        expect_writable(portfolio_ai)?;
        expect_owner(market_ai, program_id)?;
        expect_owner(portfolio_ai, program_id)?;
        let (_, _, max_market_slots, _) =
            state::read_market_config_mode_and_capacity(&market_ai.try_borrow_data()?)?;
        ensure_portfolio_storage_for_market_slots(portfolio_ai, max_market_slots)?;
        let mut market_data = market_ai.try_borrow_mut_data()?;
        let (cfg, mut group) = state::market_view_mut(&mut market_data)?;
        let mut portfolio_data = portfolio_ai.try_borrow_mut_data()?;
        let mut portfolio =
            state::portfolio_view_mut_for_market_slots(&mut portfolio_data, max_market_slots)?;
        expect_portfolio_view_account_key(&portfolio, portfolio_ai.key)?;
        if owner_must_sign {
            expect_portfolio_view_owner(&portfolio, owner.key)?;
        }
        f(&mut group, &mut portfolio, &cfg).map_err(map_v16_error)?;
        group.validate_shape().map_err(map_v16_error)?;
        portfolio
            .validate_with_market(&group.as_view())
            .map_err(map_v16_error)
    }

    type SourceBackingSnapshot = alloc::boxed::Box<[u128]>;
    type DomainFeeTotals = alloc::boxed::Box<[u128]>;

    fn source_counterparty_backing_snapshot_view(
        account: &percolator::PortfolioV16ViewMut<'_>,
    ) -> Result<SourceBackingSnapshot, ProgramError> {
        let mut out = Vec::with_capacity(account.source_domains.len());
        let mut d = 0usize;
        while d < account.source_domains.len() {
            out.push(
                account.source_domains[d]
                    .source_lien_counterparty_backing_num
                    .get(),
            );
            d += 1;
        }
        Ok(out.into_boxed_slice())
    }

    fn domain_fee_totals_zeroed(domain_count: usize) -> Result<DomainFeeTotals, ProgramError> {
        let mut out = Vec::with_capacity(domain_count);
        let mut d = 0usize;
        while d < domain_count {
            out.push(0);
            d += 1;
        }
        Ok(out.into_boxed_slice())
    }

    fn backing_fee_policy_for_domain_view(
        group: &state::MarketViewMutV16<'_>,
        cfg: &WrapperConfigV16,
        domain: usize,
    ) -> Result<(u16, u16), ProgramError> {
        let asset_index = domain / 2;
        let long_side = domain % 2 == 0;
        let profile = read_oracle_profile_from_view(group, cfg, asset_index)?;
        Ok(if long_side {
            (
                profile.backing_trade_fee_bps_long,
                profile.backing_trade_fee_insurance_share_bps_long,
            )
        } else {
            (
                profile.backing_trade_fee_bps_short,
                profile.backing_trade_fee_insurance_share_bps_short,
            )
        })
    }

    fn fee_bps_ceil(amount: u128, bps: u16) -> Result<u128, ProgramError> {
        if amount == 0 || bps == 0 {
            return Ok(0);
        }
        let num = amount
            .checked_mul(bps as u128)
            .and_then(|v| v.checked_add(9_999))
            .ok_or(PercolatorError::EngineArithmeticOverflow)?;
        Ok(num / 10_000)
    }

    fn collect_backing_domain_fees_for_account_view(
        group: &state::MarketViewMutV16<'_>,
        cfg: &WrapperConfigV16,
        account: &percolator::PortfolioV16ViewMut<'_>,
        before: &[u128],
        fees_by_domain: &mut [u128],
    ) -> Result<u128, ProgramError> {
        let max_slots = group.header.config.max_market_slots.get() as usize;
        let domain_count = max_slots
            .checked_mul(2)
            .ok_or(PercolatorError::EngineArithmeticOverflow)?;
        let mut total = 0u128;
        let mut d = 0usize;
        while d < domain_count {
            if d >= before.len() || d >= fees_by_domain.len() || d >= account.source_domains.len() {
                return Err(PercolatorError::EngineInvalidConfig.into());
            }
            let after = account.source_domains[d]
                .source_lien_counterparty_backing_num
                .get();
            if after > before[d] {
                let delta_num = after - before[d];
                if delta_num % BOUND_SCALE != 0 {
                    return Err(PercolatorError::EngineInvalidLeg.into());
                }
                let delta = delta_num / BOUND_SCALE;
                let (bps, _) = backing_fee_policy_for_domain_view(group, cfg, d)?;
                let fee = fee_bps_ceil(delta, bps)?;
                if fee != 0 {
                    fees_by_domain[d] = fees_by_domain[d]
                        .checked_add(fee)
                        .ok_or(PercolatorError::EngineArithmeticOverflow)?;
                    total = total
                        .checked_add(fee)
                        .ok_or(PercolatorError::EngineArithmeticOverflow)?;
                }
            }
            d += 1;
        }
        Ok(total)
    }

    fn fee_share_floor(amount: u128, share_bps: u16) -> Result<u128, ProgramError> {
        if amount == 0 || share_bps == 0 {
            return Ok(0);
        }
        amount
            .checked_mul(share_bps as u128)
            .map(|v| v / 10_000)
            .ok_or(PercolatorError::EngineArithmeticOverflow.into())
    }

    fn precheck_account_view_can_pay_backing_domain_fee(
        account: &percolator::PortfolioV16ViewMut<'_>,
        fee: u128,
    ) -> Result<(), ProgramError> {
        if fee == 0 {
            return Ok(());
        }
        let cert = account
            .header
            .health_cert
            .try_to_runtime()
            .map_err(map_v16_error)?;
        if cert.valid == false || account.header.pnl.get() < 0 || account.header.capital.get() < fee
        {
            return Err(PercolatorError::EngineLockActive.into());
        }
        let fee_i128 =
            i128::try_from(fee).map_err(|_| PercolatorError::EngineArithmeticOverflow)?;
        let next_equity = cert
            .certified_equity
            .checked_sub(fee_i128)
            .ok_or(PercolatorError::EngineArithmeticOverflow)?;
        if next_equity < 0 || (next_equity as u128) < cert.certified_initial_req {
            return Err(PercolatorError::EngineLockActive.into());
        }
        Ok(())
    }

    fn debit_account_view_backing_domain_fee(
        group: &mut state::MarketViewMutV16<'_>,
        account: &mut percolator::PortfolioV16ViewMut<'_>,
        fee: u128,
    ) -> Result<(), ProgramError> {
        if fee == 0 {
            return Ok(());
        }
        let fee_i128 =
            i128::try_from(fee).map_err(|_| PercolatorError::EngineArithmeticOverflow)?;
        let next_equity = account
            .header
            .health_cert
            .certified_equity
            .get()
            .checked_sub(fee_i128)
            .ok_or(PercolatorError::EngineArithmeticOverflow)?;
        account.header.capital = percolator::V16PodU128::new(
            account
                .header
                .capital
                .get()
                .checked_sub(fee)
                .ok_or(PercolatorError::EngineCounterUnderflow)?,
        );
        group.header.c_tot = percolator::V16PodU128::new(
            group
                .header
                .c_tot
                .get()
                .checked_sub(fee)
                .ok_or(PercolatorError::EngineCounterUnderflow)?,
        );
        account.header.health_cert.certified_equity = percolator::V16PodI128::new(next_equity);
        account.header.health_cert.certified_liq_deficit =
            percolator::V16PodU128::new(if next_equity < 0 {
                next_equity.unsigned_abs()
            } else {
                account
                    .header
                    .health_cert
                    .certified_maintenance_req
                    .get()
                    .saturating_sub(next_equity as u128)
            });
        Ok(())
    }

    fn apply_backing_domain_fees_after_trade_view(
        cfg: &WrapperConfigV16,
        group: &mut state::MarketViewMutV16<'_>,
        account_a: &mut percolator::PortfolioV16ViewMut<'_>,
        before_a: &[u128],
        account_b: &mut percolator::PortfolioV16ViewMut<'_>,
        before_b: &[u128],
    ) -> Result<u128, ProgramError> {
        let domain_count =
            v16_domain_count_for_market_slots(group.header.config.max_market_slots.get())
                .map_err(map_v16_error)?;
        let mut fees_by_domain = domain_fee_totals_zeroed(domain_count)?;
        let fee_a = collect_backing_domain_fees_for_account_view(
            group,
            cfg,
            account_a,
            before_a,
            fees_by_domain.as_mut(),
        )?;
        let fee_b = collect_backing_domain_fees_for_account_view(
            group,
            cfg,
            account_b,
            before_b,
            fees_by_domain.as_mut(),
        )?;
        if fee_a == 0 && fee_b == 0 {
            return Ok(0);
        }
        precheck_account_view_can_pay_backing_domain_fee(account_a, fee_a)?;
        precheck_account_view_can_pay_backing_domain_fee(account_b, fee_b)?;
        debit_account_view_backing_domain_fee(group, account_a, fee_a)?;
        debit_account_view_backing_domain_fee(group, account_b, fee_b)?;

        let mut d = 0usize;
        while d < domain_count {
            let fee = fees_by_domain[d];
            if fee != 0 {
                let asset_index = d / 2;
                let (_, insurance_share_bps) = backing_fee_policy_for_domain_view(group, cfg, d)?;
                let insurance_fee = fee_share_floor(fee, insurance_share_bps)?;
                let provider_fee = fee
                    .checked_sub(insurance_fee)
                    .ok_or(PercolatorError::EngineCounterUnderflow)?;
                let bucket_acc = if d % 2 == 0 {
                    &mut group.markets[asset_index].engine.backing_long
                } else {
                    &mut group.markets[asset_index].engine.backing_short
                };
                let mut bucket = bucket_acc.try_to_runtime().map_err(map_v16_error)?;
                if bucket.status != BackingBucketStatusV16::Fresh
                    || bucket.expiry_slot <= group.header.current_slot.get()
                {
                    return Err(PercolatorError::EngineLockActive.into());
                }
                if insurance_fee != 0 {
                    group.header.insurance = percolator::V16PodU128::new(
                        group
                            .header
                            .insurance
                            .get()
                            .checked_add(insurance_fee)
                            .ok_or(PercolatorError::EngineArithmeticOverflow)?,
                    );
                }
                if provider_fee != 0 {
                    bucket.utilization_fee_earnings = bucket
                        .utilization_fee_earnings
                        .checked_add(provider_fee)
                        .ok_or(PercolatorError::EngineArithmeticOverflow)?;
                    *bucket_acc = percolator::BackingBucketV16Account::from_runtime(&bucket);
                }
                if insurance_fee != 0 {
                    credit_fee_to_domain_budget_view(cfg, group, d, insurance_fee)?;
                }
            }
            d += 1;
        }
        if account_a.header.health_cert.valid != 0 {
            account_a.header.health_cert.cert_risk_epoch = group.header.risk_epoch;
        }
        if account_b.header.health_cert.valid != 0 {
            account_b.header.health_cert.cert_risk_epoch = group.header.risk_epoch;
        }
        group.validate_shape().map_err(map_v16_error)?;
        account_a
            .validate_with_market(&group.as_view())
            .map_err(map_v16_error)?;
        account_b
            .validate_with_market(&group.as_view())
            .map_err(map_v16_error)?;
        fee_a
            .checked_add(fee_b)
            .ok_or(PercolatorError::EngineArithmeticOverflow.into())
    }

    fn asset_local_has_position_or_loss_state_view(
        group: &state::MarketViewMutV16<'_>,
        asset_index: usize,
    ) -> bool {
        if asset_index >= group.header.config.max_market_slots.get() as usize
            || asset_index >= group.markets.len()
        {
            return true;
        }
        let asset = &group.markets[asset_index].engine.asset;
        asset.oi_eff_long_q.get() != 0
            || asset.oi_eff_short_q.get() != 0
            || asset.stored_pos_count_long.get() != 0
            || asset.stored_pos_count_short.get() != 0
            || asset.stale_account_count_long.get() != 0
            || asset.stale_account_count_short.get() != 0
            || asset.b_long_num.get() != 0
            || asset.b_short_num.get() != 0
            || asset.b_epoch_start_long_num.get() != 0
            || asset.b_epoch_start_short_num.get() != 0
            || asset.loss_weight_sum_long.get() != 0
            || asset.loss_weight_sum_short.get() != 0
            || asset.social_loss_remainder_long_num.get() != 0
            || asset.social_loss_remainder_short_num.get() != 0
            || asset.social_loss_dust_long_num.get() != 0
            || asset.social_loss_dust_short_num.get() != 0
            || asset.explicit_unallocated_loss_long.get() != 0
            || asset.explicit_unallocated_loss_short.get() != 0
            || asset.mode_long != 0
            || asset.mode_short != 0
    }

    fn require_empty_asset_lifecycle_state_view(
        group: &state::MarketViewMutV16<'_>,
        asset_index: usize,
    ) -> ProgramResult {
        fn backing_bucket_empty_amount_shape(bucket: percolator::BackingBucketV16) -> bool {
            bucket.fresh_unliened_backing_num == 0
                && bucket.valid_liened_backing_num == 0
                && bucket.consumed_liened_backing_num == 0
                && bucket.impaired_liened_backing_num == 0
                && bucket.utilization_fee_earnings == 0
                && bucket.expiry_slot == 0
                && bucket.status == BackingBucketStatusV16::Empty
        }

        fn source_credit_empty_amount_shape(source: SourceCreditStateV16) -> bool {
            source.positive_claim_bound_num == 0
                && source.exact_positive_claim_num == 0
                && source.fresh_reserved_backing_num == 0
                && source.spent_backing_num == 0
                && source.provider_receivable_num == 0
                && source.valid_liened_backing_num == 0
                && source.impaired_liened_backing_num == 0
                && source.insurance_credit_reserved_num == 0
                && source.valid_liened_insurance_num == 0
                && source.impaired_liened_insurance_num == 0
        }

        if asset_index >= group.header.config.max_market_slots.get() as usize
            || asset_index >= group.markets.len()
        {
            return Err(PercolatorError::EngineInvalidLeg.into());
        }
        let slot = &group.markets[asset_index].engine;
        let asset = slot.asset;
        let market_id = asset.market_id.get();
        let long_bucket = slot.backing_long.try_to_runtime().map_err(map_v16_error)?;
        let short_bucket = slot.backing_short.try_to_runtime().map_err(map_v16_error)?;
        let source_credit_long = slot
            .source_credit_long
            .try_to_runtime()
            .map_err(map_v16_error)?;
        let source_credit_short = slot
            .source_credit_short
            .try_to_runtime()
            .map_err(map_v16_error)?;
        if slot.pending_domain_loss_barrier_long.get() != 0
            || slot.pending_domain_loss_barrier_short.get() != 0
            || asset.mode_long != SIDE_MODE_NORMAL
            || asset.mode_short != SIDE_MODE_NORMAL
            || !((asset.a_long.get() == percolator::ADL_ONE
                && asset.a_short.get() == percolator::ADL_ONE)
                || (asset.a_long.get() == 0 && asset.a_short.get() == 0))
            || asset.k_long.get() != 0
            || asset.k_short.get() != 0
            || asset.f_long_num.get() != 0
            || asset.f_short_num.get() != 0
            || asset.k_epoch_start_long.get() != 0
            || asset.k_epoch_start_short.get() != 0
            || asset.f_epoch_start_long_num.get() != 0
            || asset.f_epoch_start_short_num.get() != 0
            || asset.b_long_num.get() != 0
            || asset.b_short_num.get() != 0
            || asset.b_epoch_start_long_num.get() != 0
            || asset.b_epoch_start_short_num.get() != 0
            || asset.oi_eff_long_q.get() != 0
            || asset.oi_eff_short_q.get() != 0
            || asset.stored_pos_count_long.get() != 0
            || asset.stored_pos_count_short.get() != 0
            || asset.stale_account_count_long.get() != 0
            || asset.stale_account_count_short.get() != 0
            || asset.pending_obligation_count_long.get() != 0
            || asset.pending_obligation_count_short.get() != 0
            || asset.loss_weight_sum_long.get() != 0
            || asset.loss_weight_sum_short.get() != 0
            || asset.social_loss_remainder_long_num.get() != 0
            || asset.social_loss_remainder_short_num.get() != 0
            || asset.social_loss_dust_long_num.get() != 0
            || asset.social_loss_dust_short_num.get() != 0
            || asset.explicit_unallocated_loss_long.get() != 0
            || asset.explicit_unallocated_loss_short.get() != 0
            || slot.insurance_domain_budget_long.get() != 0
            || slot.insurance_domain_budget_short.get() != 0
            || slot.insurance_domain_spent_long.get() != 0
            || slot.insurance_domain_spent_short.get() != 0
            || !source_credit_empty_amount_shape(source_credit_long)
            || !source_credit_empty_amount_shape(source_credit_short)
            || !backing_bucket_empty_amount_shape(long_bucket)
            || !backing_bucket_empty_amount_shape(short_bucket)
            || long_bucket.market_id != market_id
            || short_bucket.market_id != market_id
            || slot
                .insurance_reservation_long
                .try_to_runtime()
                .map_err(map_v16_error)?
                != percolator::InsuranceCreditReservationV16::EMPTY
            || slot
                .insurance_reservation_short
                .try_to_runtime()
                .map_err(map_v16_error)?
                != percolator::InsuranceCreditReservationV16::EMPTY
        {
            return Err(PercolatorError::EngineLockActive.into());
        }
        Ok(())
    }

    fn canonicalize_empty_asset_lifecycle_state_view(
        group: &mut state::MarketViewMutV16<'_>,
        asset_index: usize,
    ) {
        let slot = &mut group.markets[asset_index].engine;
        slot.source_credit_long =
            percolator::SourceCreditStateV16Account::from_runtime(&SourceCreditStateV16::EMPTY);
        slot.source_credit_short =
            percolator::SourceCreditStateV16Account::from_runtime(&SourceCreditStateV16::EMPTY);
    }

    fn group_has_position_or_loss_state_view(group: &state::MarketViewMutV16<'_>) -> bool {
        if group.header.pnl_pos_tot.get() != 0
            || group.header.stale_certificate_count.get() != 0
            || group.header.b_stale_account_count.get() != 0
            || group.header.negative_pnl_account_count.get() != 0
            || group.header.bankruptcy_hlock_active != 0
            || group.header.threshold_stress_active != 0
            || group.header.loss_stale_active != 0
            || group
                .header
                .recovery_reason
                .try_to_runtime()
                .ok()
                .flatten()
                .is_some()
        {
            return true;
        }
        let n = group.header.config.max_market_slots.get() as usize;
        let mut i = 0usize;
        while i < n {
            if asset_local_has_position_or_loss_state_view(group, i) {
                return true;
            }
            i += 1;
        }
        false
    }

    fn trade_notional_floor(size_q: u128, price: u64) -> Result<u128, ProgramError> {
        Ok(size_q
            .checked_mul(price as u128)
            .ok_or(PercolatorError::EngineArithmeticOverflow)?
            / percolator::POS_SCALE)
    }

    fn risk_notional_ceil(size_q: u128, price: u64) -> Result<u128, ProgramError> {
        let num = size_q
            .checked_mul(price as u128)
            .ok_or(PercolatorError::EngineArithmeticOverflow)?;
        Ok(num
            .checked_add(percolator::POS_SCALE - 1)
            .ok_or(PercolatorError::EngineArithmeticOverflow)?
            / percolator::POS_SCALE)
    }

    fn hybrid_segment_dt_view(
        group: &state::MarketViewMutV16<'_>,
        now_slot: u64,
    ) -> Result<u64, ProgramError> {
        if now_slot < group.header.slot_last.get() {
            return Err(PercolatorError::EngineStale.into());
        }
        Ok(core::cmp::min(
            now_slot - group.header.slot_last.get(),
            group.header.config.max_accrual_dt_slots.get(),
        ))
    }

    fn hybrid_trade_fee_bps_view(
        cfg: &WrapperConfigV16,
        profile: &state::AssetOracleProfileV16,
        group: &state::MarketViewMutV16<'_>,
        asset_index: usize,
        size_q_abs: u128,
        exec_price: u64,
        caller_fee_bps: u64,
    ) -> Result<u64, ProgramError> {
        let base = core::cmp::max(caller_fee_bps, cfg.trade_fee_base_bps);
        let max_trading_fee_bps = group.header.config.max_trading_fee_bps.get();
        if base > max_trading_fee_bps {
            return Err(PercolatorError::InvalidInstruction.into());
        }
        if !oracle_v16::profile_is_price_managed(profile) {
            return Ok(base);
        }
        if oracle_v16::profile_is_auth_mark(profile) {
            return Ok(base);
        }
        let now_slot = Clock::get()
            .map(|c| c.slot)
            .unwrap_or(group.header.current_slot.get());
        if oracle_v16::profile_is_hybrid(profile)
            && !oracle_v16::profile_hybrid_soft_stale_matured(profile, now_slot)
        {
            return Ok(base);
        }
        if asset_index >= group.header.config.max_market_slots.get() as usize
            || profile.mark_ewma_e6 == 0
        {
            return Ok(base);
        }
        let asset = group.markets[asset_index].engine.asset;
        let effective_price = asset.effective_price.get();
        let trade_notional = trade_notional_floor(size_q_abs, exec_price)?;
        let clamped_exec = oracle_v16::clamp_toward_engine_dt(
            effective_price,
            exec_price,
            group.header.config.max_price_move_bps_per_slot.get(),
            1,
        );
        let max_side_oi_q = core::cmp::max(asset.oi_eff_long_q.get(), asset.oi_eff_short_q.get());
        let max_side_notional = risk_notional_ceil(max_side_oi_q, effective_price)?;
        let mark_externality_notional = core::cmp::max(max_side_notional, trade_notional)
            .checked_mul(2)
            .ok_or(PercolatorError::EngineArithmeticOverflow)?;
        let segment_dt = core::cmp::max(1, hybrid_segment_dt_view(group, now_slot)?);
        let min_externality_bps = group
            .header
            .config
            .max_price_move_bps_per_slot
            .get()
            .checked_mul(segment_dt)
            .ok_or(PercolatorError::EngineArithmeticOverflow)?;
        let required = policy_v16::dynamic_fee_bps_with_externality_floor(
            base,
            profile.mark_ewma_e6,
            clamped_exec,
            profile.mark_ewma_halflife_slots,
            profile.mark_ewma_last_slot,
            now_slot,
            trade_notional,
            mark_externality_notional,
            profile.mark_min_fee,
            min_externality_bps,
        )
        .ok_or(PercolatorError::EngineInvalidConfig)?;
        let fee = core::cmp::max(base, required);
        if fee > max_trading_fee_bps {
            return Err(PercolatorError::EngineInvalidConfig.into());
        }
        Ok(fee)
    }

    fn hybrid_effective_price_for_crank_view(
        cfg: &WrapperConfigV16,
        profile: &mut state::AssetOracleProfileV16,
        group: &state::MarketViewMutV16<'_>,
        asset_index: usize,
        now_slot: u64,
        now_unix_ts: i64,
        oracle_accounts: &[AccountInfo],
    ) -> Result<u64, ProgramError> {
        if asset_index >= group.header.config.max_market_slots.get() as usize {
            return Err(PercolatorError::InvalidInstruction.into());
        }
        if oracle_v16::profile_is_ewma_mark(profile) || oracle_v16::profile_is_auth_mark(profile) {
            let target = profile.mark_ewma_e6;
            if target == 0 {
                return Err(PercolatorError::OracleInvalid.into());
            }
            let asset = group.markets[asset_index].engine.asset;
            let exposed = asset.oi_eff_long_q.get() != 0 || asset.oi_eff_short_q.get() != 0;
            let price = oracle_v16::effective_price_from_target(
                asset.effective_price.get(),
                target,
                group.header.config.max_price_move_bps_per_slot.get(),
                hybrid_segment_dt_view(group, now_slot)?,
                exposed,
            );
            profile.oracle_target_price_e6 = target;
            return Ok(price);
        }
        if !oracle_v16::profile_is_hybrid(profile) {
            let price = group.markets[asset_index]
                .engine
                .asset
                .effective_price
                .get();
            if price == 0 {
                return Err(PercolatorError::OracleInvalid.into());
            }
            profile.oracle_target_price_e6 = price;
            return Ok(price);
        }
        if cfg.permissionless_resolve_stale_slots != 0
            && now_slot.saturating_sub(profile.last_good_oracle_slot)
                >= cfg.permissionless_resolve_stale_slots
        {
            return Err(PercolatorError::EngineRecoveryRequired.into());
        }
        let count = profile.oracle_leg_count as usize;
        let read = if oracle_accounts.len() >= count {
            oracle_v16::read_external_price_e6_profile(profile, oracle_accounts, now_unix_ts)
        } else {
            Err(ProgramError::NotEnoughAccountKeys)
        };
        let target = match read {
            Ok((price, publish_time, advanced)) => {
                profile.oracle_target_price_e6 = price;
                profile.oracle_target_publish_time = publish_time;
                if advanced {
                    profile.last_good_oracle_slot = now_slot;
                }
                price
            }
            Err(e)
                if e == ProgramError::from(PercolatorError::OracleStale)
                    || e == ProgramError::NotEnoughAccountKeys =>
            {
                if !oracle_v16::profile_hybrid_soft_stale_matured(profile, now_slot) {
                    return Err(e);
                }
                profile.mark_ewma_e6
            }
            Err(e) => return Err(e),
        };
        if target == 0 {
            return Err(PercolatorError::OracleInvalid.into());
        }
        let asset = group.markets[asset_index].engine.asset;
        let exposed = asset.oi_eff_long_q.get() != 0 || asset.oi_eff_short_q.get() != 0;
        let price = oracle_v16::effective_price_from_target(
            asset.effective_price.get(),
            target,
            group.header.config.max_price_move_bps_per_slot.get(),
            hybrid_segment_dt_view(group, now_slot)?,
            exposed,
        );
        profile.oracle_target_price_e6 = target;
        if !oracle_v16::profile_hybrid_soft_stale_matured(profile, now_slot) {
            profile.mark_ewma_e6 = price;
            profile.mark_ewma_last_slot = now_slot;
        }
        Ok(price)
    }

    fn update_hybrid_mark_after_trade_view(
        profile: &mut state::AssetOracleProfileV16,
        group: &state::MarketViewMutV16<'_>,
        asset_index: usize,
        exec_price: u64,
        fee_paid: u128,
    ) -> Result<(), ProgramError> {
        let now_slot = Clock::get()
            .map(|c| c.slot)
            .unwrap_or(group.header.current_slot.get());
        let ewma_updates_from_trade = oracle_v16::profile_is_ewma_mark(profile)
            || (oracle_v16::profile_is_hybrid(profile)
                && oracle_v16::profile_hybrid_soft_stale_matured(profile, now_slot));
        if !ewma_updates_from_trade
            || asset_index >= group.header.config.max_market_slots.get() as usize
        {
            return Ok(());
        }
        let fee_paid = u64::try_from(fee_paid).unwrap_or(u64::MAX);
        let effective_price = group.markets[asset_index]
            .engine
            .asset
            .effective_price
            .get();
        let clamped_exec = oracle_v16::clamp_toward_engine_dt(
            effective_price,
            exec_price,
            group.header.config.max_price_move_bps_per_slot.get(),
            1,
        );
        let old = profile.mark_ewma_e6;
        let new_mark = policy_v16::ewma_update(
            old,
            clamped_exec,
            profile.mark_ewma_halflife_slots,
            profile.mark_ewma_last_slot,
            now_slot,
            fee_paid,
            profile.mark_min_fee,
        );
        if new_mark > percolator::MAX_ORACLE_PRICE {
            return Err(PercolatorError::OracleInvalid.into());
        }
        if new_mark != 0 && new_mark != old {
            profile.mark_ewma_e6 = new_mark;
            profile.mark_ewma_last_slot = now_slot;
        }
        Ok(())
    }

    fn derive_matcher_delegate(
        program_id: &Pubkey,
        market_key: &Pubkey,
        maker_account: &Pubkey,
        matcher_program: &Pubkey,
        matcher_context: &Pubkey,
    ) -> (Pubkey, u8) {
        Pubkey::find_program_address(
            &[
                b"matcher",
                market_key.as_ref(),
                maker_account.as_ref(),
                matcher_program.as_ref(),
                matcher_context.as_ref(),
            ],
            program_id,
        )
    }

    fn matcher_lp_account_id(delegate: &Pubkey) -> u64 {
        let bytes = delegate.to_bytes();
        u64::from_le_bytes(bytes[0..8].try_into().unwrap())
    }

    fn invoke_matcher<'a>(
        matcher_prog: &AccountInfo<'a>,
        matcher_ctx: &AccountInfo<'a>,
        matcher_delegate: &AccountInfo<'a>,
        tail: &[AccountInfo<'a>],
        req_id: u64,
        asset_index: u16,
        lp_account_id: u64,
        oracle_price_e6: u64,
        req_size: i128,
        seeds: &[&[u8]],
    ) -> ProgramResult {
        let mut data = [0u8; 67];
        data[0] = 0;
        data[1..9].copy_from_slice(&req_id.to_le_bytes());
        data[9..11].copy_from_slice(&(asset_index as u16).to_le_bytes());
        data[11..19].copy_from_slice(&lp_account_id.to_le_bytes());
        data[19..27].copy_from_slice(&oracle_price_e6.to_le_bytes());
        data[27..43].copy_from_slice(&req_size.to_le_bytes());

        let mut metas = Vec::with_capacity(2 + tail.len());
        metas.push(AccountMeta::new_readonly(*matcher_delegate.key, true));
        metas.push(AccountMeta::new(*matcher_ctx.key, false));
        for ai in tail {
            if ai.is_writable {
                metas.push(AccountMeta::new(*ai.key, ai.is_signer));
            } else {
                metas.push(AccountMeta::new_readonly(*ai.key, ai.is_signer));
            }
        }

        let ix = SolInstruction {
            program_id: *matcher_prog.key,
            accounts: metas,
            data: data.to_vec(),
        };
        let mut infos = Vec::with_capacity(3 + tail.len());
        infos.push(matcher_delegate.clone());
        infos.push(matcher_ctx.clone());
        infos.push(matcher_prog.clone());
        for ai in tail {
            infos.push(ai.clone());
        }
        invoke_signed(&ix, &infos, &[seeds])
    }

    fn amount_to_u64(amount: u128) -> Result<u64, ProgramError> {
        u64::try_from(amount).map_err(|_| PercolatorError::InvalidInstruction.into())
    }

    fn derive_vault_authority(program_id: &Pubkey, market_key: &Pubkey) -> (Pubkey, u8) {
        Pubkey::find_program_address(&[b"vault", market_key.as_ref()], program_id)
    }

    fn expect_key(ai: &AccountInfo, expected: &Pubkey) -> Result<(), ProgramError> {
        if ai.key != expected {
            return Err(ProgramError::InvalidArgument);
        }
        Ok(())
    }

    fn verify_mint(mint_ai: &AccountInfo) -> Result<(), ProgramError> {
        if mint_ai.owner != &spl_token::ID {
            return Err(PercolatorError::InvalidMint.into());
        }
        if mint_ai.data_len() != spl_token::state::Mint::LEN {
            return Err(PercolatorError::InvalidMint.into());
        }
        let data = mint_ai.try_borrow_data()?;
        spl_token::state::Mint::unpack(&data)
            .map(|_| ())
            .map_err(|_| PercolatorError::InvalidMint.into())
    }

    fn verify_token_program(token_program: &AccountInfo) -> Result<(), ProgramError> {
        if *token_program.key != spl_token::ID || !token_program.executable {
            return Err(PercolatorError::InvalidTokenProgram.into());
        }
        Ok(())
    }

    fn unpack_token_account(
        token_ai: &AccountInfo,
    ) -> Result<spl_token::state::Account, ProgramError> {
        if token_ai.owner != &spl_token::ID {
            return Err(PercolatorError::InvalidTokenAccount.into());
        }
        if token_ai.data_len() != spl_token::state::Account::LEN {
            return Err(PercolatorError::InvalidTokenAccount.into());
        }
        let data = token_ai.try_borrow_data()?;
        spl_token::state::Account::unpack(&data)
            .map_err(|_| PercolatorError::InvalidTokenAccount.into())
    }

    fn verify_user_token_account(
        token_ai: &AccountInfo,
        expected_owner: &Pubkey,
        expected_mint: &Pubkey,
    ) -> Result<(), ProgramError> {
        let token = unpack_token_account(token_ai)?;
        if token.mint != *expected_mint {
            return Err(PercolatorError::InvalidMint.into());
        }
        if token.owner != *expected_owner
            || token.state != spl_token::state::AccountState::Initialized
        {
            return Err(PercolatorError::InvalidTokenAccount.into());
        }
        Ok(())
    }

    fn primary_collateral_mint(cfg: &WrapperConfigV16) -> Pubkey {
        Pubkey::new_from_array(cfg.collateral_mint)
    }

    fn secondary_collateral_mint(cfg: &WrapperConfigV16) -> Result<Pubkey, ProgramError> {
        if cfg.secondary_collateral_mint == [0u8; 32] {
            return Err(PercolatorError::InvalidMint.into());
        }
        Ok(Pubkey::new_from_array(cfg.secondary_collateral_mint))
    }

    fn is_withdrawable_collateral_mint(cfg: &WrapperConfigV16, mint: &Pubkey) -> bool {
        mint.to_bytes() == cfg.collateral_mint
            || (cfg.secondary_collateral_mint != [0u8; 32]
                && mint.to_bytes() == cfg.secondary_collateral_mint)
    }

    fn verify_withdrawable_vault_token_account(
        token_ai: &AccountInfo,
        expected_owner: &Pubkey,
        cfg: &WrapperConfigV16,
    ) -> Result<spl_token::state::Account, ProgramError> {
        let token = unpack_token_account(token_ai)?;
        if !is_withdrawable_collateral_mint(cfg, &token.mint) {
            return Err(PercolatorError::InvalidMint.into());
        }
        if token.owner != *expected_owner
            || token.state != spl_token::state::AccountState::Initialized
            || token.delegate.is_some()
            || token.close_authority.is_some()
        {
            return Err(PercolatorError::InvalidVaultAccount.into());
        }
        Ok(token)
    }

    fn verify_withdrawable_token_accounts(
        dest_token_ai: &AccountInfo,
        expected_dest_owner: &Pubkey,
        vault_token_ai: &AccountInfo,
        expected_vault_owner: &Pubkey,
        cfg: &WrapperConfigV16,
    ) -> Result<(), ProgramError> {
        let dest = unpack_token_account(dest_token_ai)?;
        let vault = unpack_token_account(vault_token_ai)?;
        if dest.mint != vault.mint || !is_withdrawable_collateral_mint(cfg, &dest.mint) {
            return Err(PercolatorError::InvalidMint.into());
        }
        if dest.owner != *expected_dest_owner
            || dest.state != spl_token::state::AccountState::Initialized
        {
            return Err(PercolatorError::InvalidTokenAccount.into());
        }
        if vault.owner != *expected_vault_owner
            || vault.state != spl_token::state::AccountState::Initialized
            || vault.delegate.is_some()
            || vault.close_authority.is_some()
        {
            return Err(PercolatorError::InvalidVaultAccount.into());
        }
        Ok(())
    }

    fn verify_vault_token_account(
        token_ai: &AccountInfo,
        expected_owner: &Pubkey,
        expected_mint: &Pubkey,
    ) -> Result<(), ProgramError> {
        let token = unpack_token_account(token_ai)?;
        if token.mint != *expected_mint {
            return Err(PercolatorError::InvalidMint.into());
        }
        if token.owner != *expected_owner
            || token.state != spl_token::state::AccountState::Initialized
            || token.delegate.is_some()
            || token.close_authority.is_some()
        {
            return Err(PercolatorError::InvalidVaultAccount.into());
        }
        Ok(())
    }

    fn require_token_balance(token_ai: &AccountInfo, amount: u64) -> Result<(), ProgramError> {
        let token = unpack_token_account(token_ai)?;
        if token.amount < amount {
            return Err(PercolatorError::InvalidTokenAccount.into());
        }
        Ok(())
    }

    fn transfer_tokens<'a>(
        token_program: &AccountInfo<'a>,
        source: &AccountInfo<'a>,
        dest: &AccountInfo<'a>,
        authority: &AccountInfo<'a>,
        amount: u64,
    ) -> Result<(), ProgramError> {
        if amount == 0 {
            return Ok(());
        }
        let ix = spl_token::instruction::transfer(
            token_program.key,
            source.key,
            dest.key,
            authority.key,
            &[],
            amount,
        )?;
        invoke(
            &ix,
            &[
                source.clone(),
                dest.clone(),
                authority.clone(),
                token_program.clone(),
            ],
        )
    }

    fn transfer_tokens_signed<'a>(
        token_program: &AccountInfo<'a>,
        source: &AccountInfo<'a>,
        dest: &AccountInfo<'a>,
        authority: &AccountInfo<'a>,
        amount: u64,
        signer_seeds: &[&[&[u8]]],
    ) -> Result<(), ProgramError> {
        if amount == 0 {
            return Ok(());
        }
        let ix = spl_token::instruction::transfer(
            token_program.key,
            source.key,
            dest.key,
            authority.key,
            &[],
            amount,
        )?;
        invoke_signed(
            &ix,
            &[
                source.clone(),
                dest.clone(),
                authority.clone(),
                token_program.clone(),
            ],
            signer_seeds,
        )
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use alloc::vec;
        use percolator::HealthCertV16;

        fn test_wrapper_config(price: u64) -> state::WrapperConfigV16 {
            let mut cfg = state::WrapperConfigV16::default();
            cfg.admin = [1u8; 32];
            cfg.collateral_mint = [2u8; 32];
            cfg.insurance_authority = [1u8; 32];
            cfg.insurance_operator = [1u8; 32];
            cfg.backing_bucket_authority = [1u8; 32];
            cfg.asset_authority = [1u8; 32];
            cfg.mark_authority = [1u8; 32];
            cfg.oracle_mode = constants::ORACLE_MODE_MANUAL;
            cfg.last_good_oracle_slot = 0;
            cfg.mark_ewma_e6 = price;
            cfg.mark_ewma_halflife_slots = constants::DEFAULT_MARK_EWMA_HALFLIFE_SLOTS;
            cfg.oracle_target_price_e6 = price;
            cfg
        }

        fn test_engine_config() -> V16Config {
            let mut cfg = V16Config::public_user_fund(1, 0, 10);
            cfg.min_nonzero_mm_req = 1;
            cfg.min_nonzero_im_req = 2;
            cfg.maintenance_margin_bps = 10_000;
            cfg.initial_margin_bps = 10_000;
            cfg.max_trading_fee_bps = 10_000;
            cfg.max_price_move_bps_per_slot = 10_000;
            cfg.max_accrual_dt_slots = 1;
            cfg.min_funding_lifetime_slots = 1;
            cfg
        }

        #[test]
        fn backing_domain_fee_split_routes_to_insurance_and_provider_earnings() {
            let mut cfg = test_wrapper_config(100);
            cfg.backing_trade_fee_bps_short = 10;
            cfg.backing_trade_fee_insurance_share_bps_short = 2_500;
            cfg.backing_trade_fee_policy_count = 1;

            let mut market_data = vec![0u8; state::market_account_len_for_capacity(1).unwrap()];
            state::init_market_account_zero_copy(
                &mut market_data,
                &cfg,
                test_engine_config(),
                [9u8; 32],
                100,
                0,
            )
            .unwrap();
            let portfolio_len = state::portfolio_account_len_for_market_slots(1).unwrap();
            let mut account_a_data = vec![0u8; portfolio_len];
            let mut account_b_data = vec![0u8; portfolio_len];
            state::init_portfolio_account_zero_copy(
                &mut account_a_data,
                [9u8; 32],
                [10u8; 32],
                [11u8; 32],
                0,
                1,
            )
            .unwrap();
            state::init_portfolio_account_zero_copy(
                &mut account_b_data,
                [9u8; 32],
                [12u8; 32],
                [13u8; 32],
                0,
                1,
            )
            .unwrap();

            {
                let (cfg_pre, mut group) = state::read_market(&market_data).unwrap();
                let mut account_a = state::read_portfolio(&account_a_data).unwrap();
                let mut account_b = state::read_portfolio(&account_b_data).unwrap();
                group.deposit_not_atomic(&mut account_a, 50_000).unwrap();
                group.deposit_not_atomic(&mut account_b, 50_000).unwrap();
                group.vault += 20_000;
                group
                    .add_fresh_counterparty_backing_not_atomic(1, 20_000 * BOUND_SCALE, 10)
                    .unwrap();
                group
                    .add_account_source_positive_pnl_not_atomic(&mut account_a, 1, 20_000)
                    .unwrap();

                let locked_atoms = 10_000u128;
                let locked_num = locked_atoms * BOUND_SCALE;
                group
                    .create_source_credit_lien_from_counterparty_not_atomic(1, locked_num)
                    .unwrap();
                account_a.source_claim_liened_num[1] = locked_num;
                account_a.source_claim_counterparty_liened_num[1] = locked_num;
                account_a.source_lien_effective_reserved[1] = locked_atoms;
                account_a.source_lien_counterparty_backing_num[1] = locked_num;
                account_a.source_lien_fee_last_slot[1] = group.current_slot;
                account_a.health_cert = HealthCertV16 {
                    certified_equity: 70_000,
                    certified_initial_req: 0,
                    certified_maintenance_req: 0,
                    certified_liq_deficit: 0,
                    certified_worst_case_loss: 0,
                    cert_oracle_epoch: group.oracle_epoch,
                    cert_funding_epoch: group.funding_epoch,
                    cert_risk_epoch: group.risk_epoch,
                    cert_asset_set_epoch: group.asset_set_epoch,
                    active_bitmap_at_cert: account_a.active_bitmap,
                    valid: true,
                };
                account_b.health_cert = HealthCertV16 {
                    certified_equity: 50_000,
                    certified_initial_req: 0,
                    certified_maintenance_req: 0,
                    certified_liq_deficit: 0,
                    certified_worst_case_loss: 0,
                    cert_oracle_epoch: group.oracle_epoch,
                    cert_funding_epoch: group.funding_epoch,
                    cert_risk_epoch: group.risk_epoch,
                    cert_asset_set_epoch: group.asset_set_epoch,
                    active_bitmap_at_cert: account_b.active_bitmap,
                    valid: true,
                };
                state::write_market(&mut market_data, &cfg_pre, &group).unwrap();
                state::write_portfolio(&mut account_a_data, &account_a).unwrap();
                state::write_portfolio(&mut account_b_data, &account_b).unwrap();
            }

            let before_a = vec![0u128, 0u128].into_boxed_slice();
            let before_b = vec![0u128, 0u128].into_boxed_slice();
            {
                let (cfg_view, mut group) = state::market_view_mut(&mut market_data).unwrap();
                let mut account_a =
                    state::portfolio_view_mut_for_market_slots(&mut account_a_data, 1).unwrap();
                let mut account_b =
                    state::portfolio_view_mut_for_market_slots(&mut account_b_data, 1).unwrap();
                let charged = apply_backing_domain_fees_after_trade_view(
                    &cfg_view,
                    &mut group,
                    &mut account_a,
                    before_a.as_ref(),
                    &mut account_b,
                    before_b.as_ref(),
                )
                .unwrap();
                assert_eq!(charged, 10);
            }

            let (_, group) = state::read_market(&market_data).unwrap();
            let account_a = state::read_portfolio(&account_a_data).unwrap();
            assert_eq!(group.insurance, 2);
            assert_eq!(group.source_backing_buckets[1].utilization_fee_earnings, 8);
            assert_eq!(account_a.capital, 49_990);
            assert_eq!(group.c_tot, 99_990);
            assert_eq!(
                group.source_backing_buckets[1].fresh_unliened_backing_num,
                10_000 * BOUND_SCALE,
                "provider fee must not be capitalized back into fresh backing principal"
            );
            assert_eq!(
                group.source_backing_buckets[1].valid_liened_backing_num,
                10_000 * BOUND_SCALE
            );
        }

        #[test]
        fn backing_domain_fee_policy_rejects_share_without_fee() {
            assert!(state::backing_trade_fee_policy_shape_ok(1, 10_000));
            assert!(state::backing_trade_fee_policy_shape_ok(10_000, 0));
            assert!(!state::backing_trade_fee_policy_shape_ok(0, 1));
            assert!(!state::backing_trade_fee_policy_shape_ok(10_001, 0));
            assert!(!state::backing_trade_fee_policy_shape_ok(1, 10_001));
        }
    }
}

#[cfg(all(not(feature = "no-entrypoint"), not(feature = "anchor-v2")))]
pub mod entrypoint {
    use super::processor;
    #[allow(unused_imports)]
    use alloc::format;
    #[cfg(target_os = "solana")]
    use solana_program::entrypoint::{BumpAllocator, HEAP_START_ADDRESS};
    use solana_program::{
        account_info::AccountInfo,
        entrypoint::{deserialize, ProgramResult, SUCCESS},
        pubkey::Pubkey,
    };

    // The processor still materializes engine runtime structs. This remains
    // bounded at the current fixed asset cap; larger u16-indexed markets need
    // engine zero-copy/page APIs rather than larger fixed runtime arrays.
    pub const V16_HEAP_FRAME_BYTES: usize = 128 * 1024;

    #[cfg(target_os = "solana")]
    #[global_allocator]
    static A: BumpAllocator = BumpAllocator {
        start: HEAP_START_ADDRESS as usize,
        len: V16_HEAP_FRAME_BYTES,
    };

    solana_program::custom_panic_default!();

    /// # Safety
    #[no_mangle]
    pub unsafe extern "C" fn entrypoint(input: *mut u8) -> u64 {
        let (program_id, accounts, instruction_data) = unsafe { deserialize(input) };
        match process_instruction(&program_id, &accounts, &instruction_data) {
            Ok(()) => SUCCESS,
            Err(error) => error.into(),
        }
    }

    fn process_instruction<'a>(
        program_id: &Pubkey,
        accounts: &'a [AccountInfo<'a>],
        instruction_data: &[u8],
    ) -> ProgramResult {
        processor::process_instruction(program_id, accounts, instruction_data)
    }
}

#[cfg(all(not(feature = "no-entrypoint"), feature = "anchor-v2"))]
#[allow(unsafe_code)]
pub mod entrypoint {
    extern crate alloc;

    use super::processor;
    use alloc::{rc::Rc, vec::Vec};
    use anchor_lang_v2::pinocchio::{
        account::{AccountView, RuntimeAccount},
        address::Address,
        entrypoint,
        error::ProgramError as AnchorProgramError,
        ProgramResult,
    };
    use core::{cell::RefCell, mem::size_of, slice::from_raw_parts_mut};
    use solana_program::{
        account_info::AccountInfo, clock::Epoch, program_error::ProgramError as LegacyProgramError,
        pubkey::Pubkey,
    };

    entrypoint!(process_instruction);

    fn process_instruction(
        program_id: &Address,
        accounts: &mut [AccountView],
        instruction_data: &[u8],
    ) -> ProgramResult {
        let program_id = Pubkey::new_from_array(program_id.to_bytes());
        process_with_legacy_account_infos(&program_id, accounts, instruction_data)
            .map_err(map_legacy_error)
    }

    #[inline(never)]
    fn process_with_legacy_account_infos(
        program_id: &Pubkey,
        accounts: &mut [AccountView],
        instruction_data: &[u8],
    ) -> Result<(), LegacyProgramError> {
        let len = accounts.len();
        let mut lamports = Vec::with_capacity(len);
        let mut data = Vec::with_capacity(len);

        for i in 0..len {
            if let Some(first) = first_duplicate(accounts, i) {
                lamports.push(Rc::clone(&lamports[first]));
                data.push(Rc::clone(&data[first]));
                continue;
            }

            let raw = accounts[i].account_mut_ptr();
            // Anchor v2 / Pinocchio owns the runtime account view. The v16
            // processor still uses AccountInfo internally, so this adapter is
            // the only compatibility bridge; persisted state serialization is
            // handled explicitly by `state`, not by raw Rust layout casts.
            let lamports_ref = unsafe { &mut (*raw).lamports };
            let data_ref = unsafe {
                from_raw_parts_mut(
                    (raw as *mut u8).add(size_of::<RuntimeAccount>()),
                    (*raw).data_len as usize,
                )
            };
            lamports.push(Rc::new(RefCell::new(lamports_ref)));
            data.push(Rc::new(RefCell::new(data_ref)));
        }

        let mut legacy_accounts = Vec::with_capacity(len);
        for (i, account) in accounts.iter().enumerate() {
            let key = unsafe { &*(account.address() as *const Address as *const Pubkey) };
            let owner = unsafe { &*(account.owner() as *const Address as *const Pubkey) };
            legacy_accounts.push(AccountInfo {
                key,
                lamports: Rc::clone(&lamports[i]),
                data: Rc::clone(&data[i]),
                owner,
                rent_epoch: Epoch::default(),
                is_signer: account.is_signer(),
                is_writable: account.is_writable(),
                executable: account.executable(),
            });
        }

        processor::process_instruction(program_id, &legacy_accounts, instruction_data)
    }

    fn first_duplicate(accounts: &[AccountView], index: usize) -> Option<usize> {
        let ptr = accounts[index].account_ptr();
        let mut i = 0;
        while i < index {
            if accounts[i].account_ptr() == ptr {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    fn map_legacy_error(error: LegacyProgramError) -> AnchorProgramError {
        match error {
            LegacyProgramError::Custom(code) => AnchorProgramError::Custom(code),
            LegacyProgramError::InvalidArgument => AnchorProgramError::InvalidArgument,
            LegacyProgramError::InvalidInstructionData => {
                AnchorProgramError::InvalidInstructionData
            }
            LegacyProgramError::InvalidAccountData => AnchorProgramError::InvalidAccountData,
            LegacyProgramError::AccountDataTooSmall => AnchorProgramError::AccountDataTooSmall,
            LegacyProgramError::InsufficientFunds => AnchorProgramError::InsufficientFunds,
            LegacyProgramError::IncorrectProgramId => AnchorProgramError::IncorrectProgramId,
            LegacyProgramError::MissingRequiredSignature => {
                AnchorProgramError::MissingRequiredSignature
            }
            LegacyProgramError::AccountAlreadyInitialized => {
                AnchorProgramError::AccountAlreadyInitialized
            }
            LegacyProgramError::UninitializedAccount => AnchorProgramError::UninitializedAccount,
            LegacyProgramError::NotEnoughAccountKeys => AnchorProgramError::NotEnoughAccountKeys,
            LegacyProgramError::AccountBorrowFailed => AnchorProgramError::AccountBorrowFailed,
            LegacyProgramError::MaxSeedLengthExceeded => AnchorProgramError::MaxSeedLengthExceeded,
            LegacyProgramError::InvalidSeeds => AnchorProgramError::InvalidSeeds,
            LegacyProgramError::BorshIoError(_) => AnchorProgramError::BorshIoError,
            LegacyProgramError::AccountNotRentExempt => AnchorProgramError::AccountNotRentExempt,
            LegacyProgramError::UnsupportedSysvar => AnchorProgramError::UnsupportedSysvar,
            LegacyProgramError::IllegalOwner => AnchorProgramError::IllegalOwner,
            LegacyProgramError::MaxAccountsDataAllocationsExceeded => {
                AnchorProgramError::MaxAccountsDataAllocationsExceeded
            }
            LegacyProgramError::InvalidRealloc => AnchorProgramError::InvalidRealloc,
            LegacyProgramError::MaxInstructionTraceLengthExceeded => {
                AnchorProgramError::MaxInstructionTraceLengthExceeded
            }
            LegacyProgramError::BuiltinProgramsMustConsumeComputeUnits => {
                AnchorProgramError::BuiltinProgramsMustConsumeComputeUnits
            }
            LegacyProgramError::InvalidAccountOwner => AnchorProgramError::InvalidAccountOwner,
            LegacyProgramError::ArithmeticOverflow => AnchorProgramError::ArithmeticOverflow,
        }
    }
}

pub mod risk {
    pub use percolator::*;
}
