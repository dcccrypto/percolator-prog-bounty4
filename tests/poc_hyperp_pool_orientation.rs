mod common;
use common::*;

fn encode_set_dex_pool(pool: &Pubkey) -> Vec<u8> {
    let mut data = vec![74u8];
    data.extend_from_slice(pool.as_ref());
    data
}

fn encode_update_hyperp_mark() -> Vec<u8> {
    vec![34u8]
}

fn raydium_pool_data(
    mint0: &Pubkey,
    mint1: &Pubkey,
    liquidity: u128,
    sqrt_price_x64: u128,
) -> Vec<u8> {
    let mut data = vec![0u8; 272];
    data[73..105].copy_from_slice(mint0.as_ref());
    data[105..137].copy_from_slice(mint1.as_ref());
    data[233] = 6;
    data[234] = 6;
    data[237..253].copy_from_slice(&liquidity.to_le_bytes());
    data[253..269].copy_from_slice(&sqrt_price_x64.to_le_bytes());
    data
}

fn send_signed(
    env: &mut TestEnv,
    payer: &Keypair,
    ixs: &[Instruction],
    signers: &[&Keypair],
) -> Result<(), String> {
    let tx = Transaction::new_signed_with_payer(
        ixs,
        Some(&payer.pubkey()),
        signers,
        env.svm.latest_blockhash(),
    );
    env.svm
        .send_transaction(tx)
        .map(|_| ())
        .map_err(|e| format!("{:?}", e))
}

fn set_legacy_pinned_pool(env: &mut TestEnv, pool: &Pubkey) {
    let mut slab_account = env.svm.get_account(&env.slab).unwrap();
    let mut config = percolator_prog::state::read_config(&slab_account.data);
    config.dex_pool = pool.to_bytes();
    percolator_prog::state::write_config(&mut slab_account.data, &config);
    env.svm.set_account(env.slab, slab_account).unwrap();
}

#[test]
fn raydium_hyperp_rejects_collateral_on_quote_side() {
    program_path();
    let mut env = TestEnv::new();
    let init = encode_init_market_full_v2(
        &env.payer.pubkey(),
        &env.mint,
        &[0u8; 32],
        0,
        1_000_000,
        0,
    );
    env.try_init_market_raw(init).unwrap();
    let admin = Keypair::from_bytes(&env.payer.to_bytes()).unwrap();

    let quote_mint = Pubkey::new_unique();
    let reversed_pool = Pubkey::new_unique();
    let valid_pool = Pubkey::new_unique();
    let liquidity = percolator_prog::constants::MIN_DEX_QUOTE_LIQUIDITY as u128 + 1;
    let sqrt_price_x64 = 1u128 << 64;

    env.svm
        .set_account(
            reversed_pool,
            Account {
                lamports: 1_000_000,
                data: raydium_pool_data(&quote_mint, &env.mint, liquidity, sqrt_price_x64),
                owner: percolator_prog::oracle::RAYDIUM_CLMM_PROGRAM_ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
    env.svm
        .set_account(
            valid_pool,
            Account {
                lamports: 1_000_000,
                data: raydium_pool_data(&env.mint, &quote_mint, liquidity, sqrt_price_x64),
                owner: percolator_prog::oracle::RAYDIUM_CLMM_PROGRAM_ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

    let set_reversed_ix = Instruction {
        program_id: env.program_id,
        accounts: vec![
            AccountMeta::new(admin.pubkey(), true),
            AccountMeta::new(env.slab, false),
            AccountMeta::new_readonly(reversed_pool, false),
        ],
        data: encode_set_dex_pool(&reversed_pool),
    };
    assert!(
        send_signed(&mut env, &admin, &[set_reversed_ix], &[&admin]).is_err(),
        "SetDexPool must reject Raydium pools where mint1 is collateral_mint"
    );

    set_legacy_pinned_pool(&mut env, &reversed_pool);
    env.svm.set_sysvar(&Clock {
        slot: 200,
        unix_timestamp: 200,
        ..Clock::default()
    });
    let update_reversed_ix = Instruction {
        program_id: env.program_id,
        accounts: vec![
            AccountMeta::new(env.slab, false),
            AccountMeta::new_readonly(reversed_pool, false),
            AccountMeta::new_readonly(sysvar::clock::ID, false),
        ],
        data: encode_update_hyperp_mark(),
    };
    assert!(
        send_signed(&mut env, &admin, &[update_reversed_ix], &[&admin]).is_err(),
        "UpdateHyperpMark must reject legacy-pinned Raydium pools where mint1 is collateral_mint"
    );

    let set_valid_ix = Instruction {
        program_id: env.program_id,
        accounts: vec![
            AccountMeta::new(admin.pubkey(), true),
            AccountMeta::new(env.slab, false),
            AccountMeta::new_readonly(valid_pool, false),
        ],
        data: encode_set_dex_pool(&valid_pool),
    };
    send_signed(&mut env, &admin, &[set_valid_ix], &[&admin]).unwrap();

    let before =
        percolator_prog::state::read_config(&env.svm.get_account(&env.slab).unwrap().data);
    let update_valid_ix = Instruction {
        program_id: env.program_id,
        accounts: vec![
            AccountMeta::new(env.slab, false),
            AccountMeta::new_readonly(valid_pool, false),
            AccountMeta::new_readonly(sysvar::clock::ID, false),
        ],
        data: encode_update_hyperp_mark(),
    };
    send_signed(&mut env, &admin, &[update_valid_ix], &[&admin]).unwrap();
    let after =
        percolator_prog::state::read_config(&env.svm.get_account(&env.slab).unwrap().data);
    assert!(
        after.last_mark_push_slot > before.last_mark_push_slot,
        "correctly oriented Raydium pool should update the Hyperp mark"
    );
}
