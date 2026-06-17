mod common;
use common::*;

fn encode_set_wallet_cap(cap_e6: u64) -> Vec<u8> {
    let mut data = vec![70u8];
    data.extend_from_slice(&cap_e6.to_le_bytes());
    data
}

fn encode_set_oi_imbalance_hard_block(threshold_bps: u16) -> Vec<u8> {
    let mut data = vec![71u8];
    data.extend_from_slice(&threshold_bps.to_le_bytes());
    data
}

fn send_admin_ix(
    env: &mut TestEnv,
    admin: &Keypair,
    data: Vec<u8>,
) -> Result<(), String> {
    let ix = Instruction {
        program_id: env.program_id,
        accounts: vec![
            AccountMeta::new(admin.pubkey(), true),
            AccountMeta::new(env.slab, false),
        ],
        data,
    };
    let tx = Transaction::new_signed_with_payer(
        &[cu_ix(), ix],
        Some(&admin.pubkey()),
        &[admin],
        env.svm.latest_blockhash(),
    );
    env.svm
        .send_transaction(tx)
        .map(|_| ())
        .map_err(|e| format!("{:?}", e))
}

#[test]
fn admin_risk_controls_reject_nonzero_unsupported_values() {
    program_path();
    let mut env = TestEnv::new();
    env.init_market_with_invert(0);
    let admin = Keypair::from_bytes(&env.payer.to_bytes()).unwrap();

    let before = env.svm.get_account(&env.slab).unwrap().data;

    assert!(
        send_admin_ix(&mut env, &admin, encode_set_wallet_cap(1_000)).is_err(),
        "SetWalletCap must reject nonzero values until storage and enforcement exist"
    );
    let after_wallet_cap = env.svm.get_account(&env.slab).unwrap().data;
    assert_eq!(
        before, after_wallet_cap,
        "rejected SetWalletCap must not mutate slab bytes"
    );

    assert!(
        send_admin_ix(&mut env, &admin, encode_set_oi_imbalance_hard_block(1)).is_err(),
        "SetOiImbalanceHardBlock must reject nonzero values until storage and enforcement exist"
    );
    let after_oi_block = env.svm.get_account(&env.slab).unwrap().data;
    assert_eq!(
        before, after_oi_block,
        "rejected SetOiImbalanceHardBlock must not mutate slab bytes"
    );

    let cfg = percolator_prog::state::read_config(&after_oi_block);
    assert_eq!(percolator_prog::state::get_max_wallet_pos_e6(&cfg), 0);
    assert_eq!(
        percolator_prog::state::get_oi_imbalance_hard_block_bps(&cfg),
        0
    );

    send_admin_ix(&mut env, &admin, encode_set_wallet_cap(0)).unwrap();
    send_admin_ix(&mut env, &admin, encode_set_oi_imbalance_hard_block(0)).unwrap();
}
