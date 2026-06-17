mod common;
use common::*;

use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    signature::{Keypair, Signer},
    sysvar,
    transaction::Transaction,
};

fn send_keeper_crank_with_candidates(env: &mut TestEnv, candidates: &[u16]) -> Result<(), String> {
    let caller = Keypair::new();
    env.svm.airdrop(&caller.pubkey(), 1_000_000_000).unwrap();
    let ix = Instruction {
        program_id: env.program_id,
        accounts: vec![
            AccountMeta::new(caller.pubkey(), true),
            AccountMeta::new(env.slab, false),
            AccountMeta::new_readonly(sysvar::clock::ID, false),
            AccountMeta::new_readonly(env.pyth_index, false),
        ],
        data: encode_crank_with_candidates(candidates),
    };
    let tx = Transaction::new_signed_with_payer(
        &[cu_ix(), ix],
        Some(&caller.pubkey()),
        &[&caller],
        env.svm.latest_blockhash(),
    );
    env.svm
        .send_transaction(tx)
        .map(|_| ())
        .map_err(|e| format!("{:?}", e))
}

#[test]
fn keeper_crank_complete_candidate_list_over_phase1_budget_is_accepted() {
    program_path();
    let mut env = TestEnv::new();
    env.init_market_with_cap(0, 200);
    let admin = Keypair::from_bytes(&env.payer.to_bytes()).unwrap();
    env.try_top_up_insurance(&admin, 5_000_000_000).unwrap();

    let lp = Keypair::new();
    let lp_idx = env.init_lp(&lp);
    env.deposit(&lp, lp_idx, 500_000_000_000);
    env.crank();

    let mut users = Vec::new();
    for _ in 0..25 {
        let user = Keypair::new();
        let idx = env.init_user(&user);
        env.deposit(&user, idx, 5_000_000_000);
        env.trade(&user, &lp, lp_idx, idx, 100_000);
        users.push((user, idx));
    }

    env.set_slot_and_price_raw_no_walk(180, 137_000_000);

    let mut candidates = Vec::with_capacity(26);
    candidates.push(lp_idx);
    candidates.extend(users.iter().map(|(_, idx)| *idx));

    assert!(
        send_keeper_crank_with_candidates(&mut env, &candidates).is_ok(),
        "complete candidate list above the Phase-1 budget should enter the bounded engine path"
    );
}

#[test]
fn keeper_crank_omitted_candidate_still_rejects_without_progress() {
    program_path();
    let mut env = TestEnv::new();
    env.init_market_with_cap(0, 200);
    let admin = Keypair::from_bytes(&env.payer.to_bytes()).unwrap();
    env.try_top_up_insurance(&admin, 1_000_000_000).unwrap();

    let lp = Keypair::new();
    let lp_idx = env.init_lp(&lp);
    env.deposit(&lp, lp_idx, 100_000_000_000);

    let user_a = Keypair::new();
    let user_a_idx = env.init_user(&user_a);
    env.deposit(&user_a, user_a_idx, 10_000_000_000);

    let user_b = Keypair::new();
    let user_b_idx = env.init_user(&user_b);
    env.deposit(&user_b, user_b_idx, 10_000_000_000);

    env.crank();
    env.trade(&user_a, &lp, lp_idx, user_a_idx, 1_000_000);
    env.trade(&user_b, &lp, lp_idx, user_b_idx, 1_000_000);

    env.set_slot_and_price_raw_no_walk(180, 137_000_000);
    let last_market_slot_before = env.read_last_market_slot();

    assert!(
        send_keeper_crank_with_candidates(&mut env, &[lp_idx, user_a_idx, user_a_idx]).is_err(),
        "KeeperCrank with an omitted exposed account must reject"
    );
    assert_eq!(
        env.read_last_market_slot(),
        last_market_slot_before,
        "rejected KeeperCrank must not advance market progress"
    );
}
