use super::*;
#[test]
fn protocol_and_bounds_are_exact_upstream_v7() {
    assert_eq!(enhance_pir::PROTOCOL_REVISION, PROTOCOL);
    assert_eq!(enhance_pir::RECORD_BYTES, RECORD_BYTES);
    assert_eq!(enhance_pir::ROW_BYTES, 21549);
    let (_, params) = ipir_sp::params_for_simplepir_profile(
        32768,
        enhance_pir::ITEM_SIZE_BITS,
        ipir_sp::SimplePirProfile::P16Q48,
    )
    .unwrap();
    assert_eq!(params.query_bits, 48);
    assert_eq!(params.db_cols, 12288);
}
