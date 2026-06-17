#![allow(dead_code, unused_imports, unused_mut, clippy::field_reassign_with_default)]
//! Regression test for the fix to the non-Hyperp `TradeCpi` oracle-replay
//! liveness spoof (the `None` -> `Some(&mut data)` change at the non-Hyperp
//! `read_price_and_stamp` call in `handle_trade_cpi`).
//!
//! Fix: the non-Hyperp `TradeCpi` oracle read now passes `Some(slab)`, so it gets
//! the same strict publish-time-advance gate as every other caller. A replayed
//! (not-advanced) oracle observation no longer refreshes `last_good_oracle_slot`.
//!
//!  * `replay_trades_no_longer_keep_market_live` — the exact replay attack now
//!    FAILS to block: the market matures and `ResolvePermissionless` succeeds.
//!  * `genuine_oracle_advance_still_refreshes_liveness` — a trade after a real
//!    publish-time advance DOES refresh the clock (no over-fix; honest liveness
//!    is preserved, matching every other caller of read_price_and_stamp).

mod common;
#[allow(unused_imports)]
use common::*;

use solana_sdk::{
    clock::Clock,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
};

const WINDOW: u64 = 200; // non-Hyperp default permissionless_resolve_stale_slots

fn set_clock(env: &mut TradeCpiTestEnv, slot: u64) {
    env.svm.set_sysvar(&Clock {
        slot,
        unix_timestamp: slot as i64,
        ..Clock::default()
    });
}
fn now(env: &TradeCpiTestEnv) -> u64 {
    env.svm.get_sysvar::<Clock>().slot
}

// FIX: replayed self-trades against a never-advanced oracle no longer refresh
// last_good_oracle_slot, so the market matures (unlike pre-fix).
#[test]
fn replay_trades_no_longer_keep_market_live() {
    let mut env = TradeCpiTestEnv::new();
    let lp = Keypair::new();
    env.init_market();
    let matcher_prog = env.matcher_program_id;
    let (lp_idx, matcher_ctx) = env.init_lp_with_matcher(&lp, &matcher_prog);
    env.deposit(&lp, lp_idx, 100_000_000_000);
    let user = Keypair::new();
    let user_idx = env.init_user(&user);
    env.deposit(&user, user_idx, 10_000_000_000);

    let t0 = now(&env);

    // Bootstrap trade (first observation legitimately stamps last_good_oracle_slot once).
    env.svm.expire_blockhash();
    assert!(env
        .try_trade_cpi(&user, &lp.pubkey(), lp_idx, user_idx, 1_000_000, &matcher_prog, &matcher_ctx)
        .is_ok());

    // Advance within the window (cranks).
    env.warp_with_cranks(WINDOW - 60);

    // A REPLAY self-trade against the SAME (never-advanced) oracle account.
    // Pre-fix this refreshed last_good_oracle_slot; post-fix (Some / strict
    // publish-advance) it does NOT.
    env.svm.expire_blockhash();
    assert!(
        env.try_trade_cpi(&user, &lp.pubkey(), lp_idx, user_idx, -1_000_000, &matcher_prog, &matcher_ctx)
            .is_ok(),
        "the trade still executes post-fix"
    );

    // Advance a full window past the bootstrap: with no genuine refresh the market
    // matures and the terminal exit becomes available.
    set_clock(&mut env, t0 + WINDOW + 50);
    assert!(
        env.try_resolve_permissionless().is_ok(),
        "FIX: replay TradeCpi no longer refreshes last_good_oracle_slot — market matures"
    );
}

// NO OVER-FIX: a trade after a GENUINE publish-time advance still refreshes the
// liveness clock (honest liveness preserved — same Some-path behavior every other
// caller already has).
#[test]
fn genuine_oracle_advance_still_refreshes_liveness() {
    let mut env = TradeCpiTestEnv::new();
    let lp = Keypair::new();
    env.init_market();
    let matcher_prog = env.matcher_program_id;
    let (lp_idx, matcher_ctx) = env.init_lp_with_matcher(&lp, &matcher_prog);
    env.deposit(&lp, lp_idx, 100_000_000_000);
    let user = Keypair::new();
    let user_idx = env.init_user(&user);
    env.deposit(&user, user_idx, 10_000_000_000);

    // Advance most of the way through the window with cranks.
    env.warp_with_cranks(WINDOW - 40); // ~slot 260

    // A GENUINE oracle update (new publish_time), then a trade: the Some-path
    // strict-advance gate fires and refreshes last_good_oracle_slot to ~now.
    env.set_oracle_price_e6(138_000_000);
    let s = now(&env);
    env.svm.expire_blockhash();
    assert!(
        env.try_trade_cpi(&user, &lp.pubkey(), lp_idx, user_idx, 1_000_000, &matcher_prog, &matcher_ctx)
            .is_ok(),
        "trade on a fresh publish executes"
    );

    // Only ~half a window past the refresh: the market is still LIVE because the
    // genuine update legitimately advanced the liveness clock. (Had the fix
    // wrongly suppressed honest advances, last_good would be stale here.)
    set_clock(&mut env, s + WINDOW - 40);
    assert!(
        env.try_resolve_permissionless().is_err(),
        "NO OVER-FIX: a genuine publish advance keeps the market live (resolve blocked)"
    );
}
