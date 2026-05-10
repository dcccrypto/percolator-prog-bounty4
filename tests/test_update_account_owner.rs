// Encoder/decoder unit tests for UpdateAccountOwner (tag 34).
//
// These tests verify the wire format and decoder rejection paths for the
// owner-reassignment instruction. No BPF runtime, no TestEnv, no engine
// state — pure instruction encoding/decoding.

use solana_program::pubkey::Pubkey;

// ─────────────────────────────────────────────────────────────────────────────
// Encoder for UpdateAccountOwner (tag 34)
// Body: tag(1) | user_idx(2 le) | new_owner(32) = 35 bytes total
// ─────────────────────────────────────────────────────────────────────────────
fn encode_update_account_owner(user_idx: u16, new_owner: &Pubkey) -> Vec<u8> {
    let mut data = vec![34u8];
    data.extend_from_slice(&user_idx.to_le_bytes());
    data.extend_from_slice(new_owner.as_ref());
    data
}

/// Verify that encode_update_account_owner produces a 35-byte payload with the
/// correct tag (34), user_idx (little-endian u16), and new_owner (32 bytes).
#[test]
fn test_update_account_owner_encoder_wire_format() {
    let user_idx: u16 = 0x1234;
    let new_owner = Pubkey::new_unique();

    let payload = encode_update_account_owner(user_idx, &new_owner);

    assert_eq!(payload.len(), 35, "payload must be exactly 35 bytes");
    assert_eq!(payload[0], 34u8, "first byte must be tag 34");
    let decoded_idx = u16::from_le_bytes([payload[1], payload[2]]);
    assert_eq!(decoded_idx, user_idx, "user_idx must round-trip correctly");
    assert_eq!(&payload[3..35], new_owner.as_ref(), "new_owner bytes must match");
}

/// Verify that a truncated tag-34 payload (< 35 bytes) fails decode in the
/// instruction module's parser. This exercises the wrapper's rejection path
/// without requiring a live BPF program.
#[test]
fn test_update_account_owner_decoder_rejects_truncated() {
    use percolator_prog::ix::Instruction;

    // Only tag byte — no user_idx, no new_owner
    let short = [34u8];
    assert!(
        Instruction::decode(&short).is_err(),
        "tag-only payload must be rejected"
    );

    // Tag + 1 byte (incomplete user_idx)
    let partial_idx = [34u8, 0x01];
    assert!(
        Instruction::decode(&partial_idx).is_err(),
        "partial user_idx must be rejected"
    );

    // Tag + user_idx (2 bytes) + 31 bytes of owner (one short)
    let mut short_owner = [0u8; 34];
    short_owner[0] = 34u8;
    assert!(
        Instruction::decode(&short_owner).is_err(),
        "31-byte owner must be rejected"
    );
}

/// Verify the decoder accepts valid tag-34 payloads for boundary user_idx values.
#[test]
fn test_update_account_owner_decoder_boundary_indices() {
    use percolator_prog::ix::Instruction;

    for user_idx in [0u16, 1u16, u16::MAX] {
        let new_owner = Pubkey::new_unique();
        let payload = encode_update_account_owner(user_idx, &new_owner);
        let result = Instruction::decode(&payload);
        match result {
            Ok(Instruction::UpdateAccountOwner { user_idx: dec_idx, new_owner: dec_owner }) => {
                assert_eq!(dec_idx, user_idx);
                assert_eq!(dec_owner.to_bytes(), new_owner.to_bytes());
            }
            other => panic!("expected Ok(UpdateAccountOwner) got {:?}", other),
        }
    }
}
