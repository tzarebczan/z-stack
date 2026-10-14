//! Experimental native packing profile, with snapshot/request binding supplied
//! by the versioned Enhance envelope. Query masks remain stable across publications
//! so unchanged worker units can reuse their public hint contributions.
//!
//! Responses use ipir-sp's two-mask output: the client uploads only the `K_g`
//! key and decodes under both published masks, which are rounded to
//! `MASK_BITS`. Correctness certificates for this mode are snapshot-specific.
//!
//! The helpers are the shared `pir-native` crate, which Transparent uses too.
//! This module adds Enhance's shape and its shard-bound setup.
//!
//! Servers accept a query's selection at either of two widths, told apart by
//! the upload's exact length: the 49-bit nearest-rounded query this crate's
//! client sends, and the 44-bit dithered query of `pir-native`. The width is
//! not part of the session, the parameter identity or the envelope, so a
//! client that sends 49 bits is unaffected by the other.
pub use pir_native::{
    accepted_query_bits, decode_cols, pack, params, parse_accepted, parse_with, prepare_with,
    public_len, publish, request_len, request_len_bits, response, response_len, D,
    DITHERED_QUERY_BITS, KEY_BYTES, MASK_BITS, Q, QUERY_BITS, Q_BITS, RESPONSE_BITS,
};
use reinspiring::native::*;
pub const COLS: usize = 12288;
pub const KEY_WORDS: usize = 2 * D;
pub fn packing_setup() -> NativeSetup {
    NativeSetup::new(params(), crate::types::setup_seed_bytes())
}
pub fn query_masks(shard: u64) -> Vec<Vec<u64>> {
    public_query_masks(crate::protocol::setup_seed(shard), 32768, COLS)
}
/// Domain-separated first-dimension masks for a `rows` by `cols` database.
pub fn public_query_masks(seed: [u8; 32], rows: usize, cols: usize) -> Vec<Vec<u64>> {
    pir_native::public_query_masks(seed, rows, cols).unwrap()
}
pub fn prepare(shard: u64, rows: usize, target: usize) -> Result<(NativeSecret, Vec<u8>), String> {
    if rows > 32768 {
        return Err("native query shape".into());
    }
    prepare_with(&packing_setup(), &query_masks(shard), rows, target)
}
/// Parses an upload at 49 bits or at 44 dithered bits, whichever its exact
/// length names; any other length is refused.
pub fn parse(bytes: &[u8], rows: usize) -> Result<(NativeKeys, Vec<u64>), String> {
    if rows > 32768 {
        return Err("native query framing".into());
    }
    parse_accepted(&packing_setup(), bytes, rows)
}
const _: () = assert!(crate::protocol::QUERY_ROW_QUANTUM as usize == D);
/// Parses an upload over all `rows` or over only the domain's first
/// `query_rows`, at 49 or 44 dithered bits, whichever its exact length names,
/// and zero-fills the selection back to `rows`. Wallets send the shorter
/// upload; servers accept the full one while older wallets remain.
pub fn parse_selection(
    bytes: &[u8],
    query_rows: usize,
    rows: usize,
) -> Result<(NativeKeys, Vec<u64>), String> {
    if rows > 32768 {
        return Err("native query framing".into());
    }
    pir_native::parse_selection(&packing_setup(), bytes, query_rows, rows)
}
/// Decode an Enhance row, truncated to its logical width.
pub fn decode(secret: &NativeSecret, public: &[u8], body: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = decode_cols(secret, public, body, COLS)?;
    out.truncate(crate::ROW_BYTES);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{Rng, SeedableRng};
    use rand_chacha::ChaCha20Rng;

    /// End-to-end over the library primitives the server uses: two-mask
    /// preprocessing, one-key upload, 29-bit masks and 22-bit responses.
    #[test]
    fn two_mask_rounded_roundtrip() {
        let rows = D;
        let cols = D;
        let mut rng = ChaCha20Rng::from_seed([7; 32]);
        let db: Vec<Vec<u16>> = (0..cols)
            .map(|_| (0..rows).map(|_| rng.gen()).collect())
            .collect();
        let masks = public_query_masks([3; 32], rows, cols);
        let lift = reinspiring::lift_ntt::LiftContext::new(D, Q).unwrap();
        let public = lift.prepare_public_dot(&masks, 65535).unwrap();
        let hint: Vec<_> = db
            .iter()
            .map(|col| {
                let poly = vec![col.iter().map(|&x| x as u64).collect::<Vec<_>>()];
                lift.public_dot(&public, &poly).unwrap()
            })
            .collect();
        let setup = NativeSetup::new(params(), [9; 32]);
        let blocks = vec![NativePreprocessed::build_two_mask(&setup, &hint).unwrap()];
        let published = publish(&blocks).unwrap();
        assert_eq!(published.len(), public_len(cols));
        assert_eq!(public_len(COLS), 89_088);
        assert_eq!(KEY_BYTES, 27_648);
        let target = 1234;
        let (secret, body) = prepare_with(&setup, &masks, rows, target).unwrap();
        assert_eq!(body.len(), request_len(rows));
        let (keys, query) = parse_with(&setup, &body, rows).unwrap();
        let scan: Vec<u64> = db
            .iter()
            .map(|col| {
                col.iter().zip(&query).fold(0u64, |a, (&x, &q)| {
                    a.wrapping_add((x as u64).wrapping_mul(q))
                }) & (Q - 1)
            })
            .collect();
        let response = pack(&blocks, &keys, &scan).unwrap();
        let row = decode_cols(&secret, &published, &response, cols).unwrap();
        let expected: Vec<u8> = db.iter().flat_map(|c| c[target].to_le_bytes()).collect();
        assert_eq!(row, expected);
    }

    /// The server's parse takes the 49-bit query this crate's client sends and
    /// the 44-bit dithered one, and refuses every other length.
    #[test]
    fn the_server_parse_accepts_both_query_widths() {
        let rows = 4096;
        let (_, legacy) = prepare(0, rows, 7).unwrap();
        assert_eq!(legacy.len(), request_len(rows));
        assert_eq!(legacy.len(), 27_648 + 25_088);
        let (_, dithered) =
            pir_native::prepare_dithered(&packing_setup(), &query_masks(0), rows, 7).unwrap();
        assert_eq!(dithered.len(), request_len_bits(rows, DITHERED_QUERY_BITS));
        assert_eq!(dithered.len(), 27_648 + 22_528);
        for body in [&legacy, &dithered] {
            let (keys, query) = parse(body, rows).unwrap();
            assert_eq!(query.len(), rows);
            assert_eq!(keys.kg_words(), parse(body, rows).unwrap().0.kg_words());
        }
        for len in [
            dithered.len() - 1,
            dithered.len() + 1,
            legacy.len() - 1,
            legacy.len() + 1,
            request_len_bits(rows, 47),
            request_len_bits(2 * rows, DITHERED_QUERY_BITS),
        ] {
            assert!(parse(&vec![0; len], rows).is_err(), "{len} bytes");
        }
    }
}
