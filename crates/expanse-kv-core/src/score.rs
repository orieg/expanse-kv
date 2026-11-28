//! Order-preserving IEEE-754 `f64` score codec (`docs/PRD.md` §3.2, §7 Phase 1).
//!
//! Encodes any non-`NaN` IEEE-754 `f64` into an 8-byte big-endian array whose
//! unsigned byte-lexicographical order matches numeric `f64` order:
//!
//! - `-0.0` is canonicalized to `+0.0` so that `-0.0` and `+0.0` encode to the
//!   exact same bytes (matching IEEE-754 numeric equality and Redis/Valkey
//!   sorted-set score semantics).
//! - Positive values (including `+0.0`, positive subnormals, and `+inf`) flip
//!   the sign bit (`bits ^ (1u64 << 63)`).
//! - Negative values (including `-inf`, negative normals, and negative
//!   subnormals) flip all 64 bits (`!bits`).
//! - `NaN` is rejected with [`ScoreError::Nan`].

use core::fmt;

/// Error returned when encoding or decoding an IEEE-754 `f64` score.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScoreError {
    /// `NaN` is not a valid sorted-set score (no total numeric order).
    Nan,
    /// Input slice did not contain an 8-byte encoded score.
    InvalidLength,
}

impl fmt::Display for ScoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Nan => f.write_str("NaN is not a valid sorted-set score"),
            Self::InvalidLength => f.write_str("encoded score must be exactly 8 bytes"),
        }
    }
}

impl core::error::Error for ScoreError {}

/// Encodes a non-`NaN` `f64` into an order-preserving `u64` key.
///
/// `-0.0` is canonicalized to `+0.0`.
///
/// # Errors
///
/// Returns [`ScoreError::Nan`] if `score.is_nan()`.
#[inline]
pub fn encode_score_u64(score: f64) -> Result<u64, ScoreError> {
    if score.is_nan() {
        return Err(ScoreError::Nan);
    }
    // Canonicalize -0.0 to +0.0 so numeric equality matches byte equality.
    let canonical = if score == 0.0 { 0.0 } else { score };
    let bits = canonical.to_bits();
    let ordered = if (bits >> 63) == 0 {
        bits ^ (1u64 << 63)
    } else {
        !bits
    };
    Ok(ordered)
}

/// Encodes a non-`NaN` `f64` into an 8-byte big-endian array whose
/// byte-lexicographical order matches numeric `f64` order.
///
/// # Errors
///
/// Returns [`ScoreError::Nan`] if `score.is_nan()`.
#[inline]
pub fn encode_score(score: f64) -> Result<[u8; 8], ScoreError> {
    Ok(encode_score_u64(score)?.to_be_bytes())
}

/// Decodes an order-preserving `u64` back into its `f64` score.
#[inline]
#[must_use]
pub fn decode_score_u64(ordered: u64) -> f64 {
    let bits = if (ordered >> 63) != 0 {
        ordered ^ (1u64 << 63)
    } else {
        !ordered
    };
    f64::from_bits(bits)
}

/// Decodes an 8-byte big-endian order-preserving score back into `f64`.
#[inline]
#[must_use]
pub fn decode_score(bytes: [u8; 8]) -> f64 {
    decode_score_u64(u64::from_be_bytes(bytes))
}

/// Decodes an 8-byte slice into `f64`.
///
/// # Errors
///
/// Returns [`ScoreError::InvalidLength`] if `slice.len() != 8`.
#[inline]
pub fn decode_score_slice(slice: &[u8]) -> Result<f64, ScoreError> {
    let bytes: [u8; 8] = slice.try_into().map_err(|_| ScoreError::InvalidLength)?;
    Ok(decode_score(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;
    use core::cmp::Ordering;

    struct SplitMix64(u64);
    impl SplitMix64 {
        fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
    }

    #[test]
    fn special_values_order_and_round_trip() {
        // Strictly ascending sequence of critical IEEE-754 boundary values.
        let ordered_values = [
            f64::NEG_INFINITY,
            f64::MIN,
            -1e100,
            -1.0,
            -f64::MIN_POSITIVE,
            // Largest negative subnormal and smallest-magnitude negative subnormal.
            -f64::from_bits(0x000F_FFFF_FFFF_FFFF),
            -f64::from_bits(1),
            0.0,
            // Smallest-magnitude positive subnormal and largest positive subnormal.
            f64::from_bits(1),
            f64::from_bits(0x000F_FFFF_FFFF_FFFF),
            f64::MIN_POSITIVE,
            1.0,
            1e100,
            f64::MAX,
            f64::INFINITY,
        ];

        let encoded: Vec<[u8; 8]> = ordered_values
            .iter()
            .map(|&v| encode_score(v).expect("non-NaN encodes"))
            .collect();

        // Verify strict pairwise byte-lexicographical ordering and exact round-trip.
        for i in 0..ordered_values.len() {
            let decoded = decode_score(encoded[i]);
            assert_eq!(decoded.to_bits(), ordered_values[i].to_bits());
            assert_eq!(
                decode_score_slice(&encoded[i]).expect("8-byte slice"),
                ordered_values[i]
            );
            for j in (i + 1)..ordered_values.len() {
                assert!(
                    encoded[i] < encoded[j],
                    "expected encode({}) < encode({}), got {:02x?} vs {:02x?}",
                    ordered_values[i],
                    ordered_values[j],
                    encoded[i],
                    encoded[j]
                );
            }
        }

        // Canonicalization: -0.0 encodes identically to +0.0 and decodes to +0.0.
        let neg_zero = encode_score(-0.0).expect("-0.0 encodes");
        let pos_zero = encode_score(0.0).expect("+0.0 encodes");
        assert_eq!(neg_zero, pos_zero);
        assert_eq!(decode_score(neg_zero).to_bits(), 0.0f64.to_bits());
    }

    #[test]
    fn nan_is_rejected_and_invalid_slice_len_errors() {
        assert_eq!(encode_score(f64::NAN), Err(ScoreError::Nan));
        assert_eq!(
            encode_score(f64::from_bits(0x7FF8_0000_0000_0001)),
            Err(ScoreError::Nan)
        );
        assert_eq!(
            encode_score(f64::from_bits(0xFFF8_0000_0000_0001)),
            Err(ScoreError::Nan)
        );
        assert_eq!(
            decode_score_slice(&[0u8; 7]),
            Err(ScoreError::InvalidLength)
        );
        assert_eq!(
            decode_score_slice(&[0u8; 9]),
            Err(ScoreError::InvalidLength)
        );
    }

    #[test]
    fn property_50k_random_f64_pairs_preserve_total_order_and_round_trip() {
        let mut rng = SplitMix64(0x5C0E_2026_0001);
        let mut tested = 0usize;

        while tested < 50_000 {
            let a = f64::from_bits(rng.next_u64());
            let b = f64::from_bits(rng.next_u64());
            if a.is_nan() {
                assert_eq!(encode_score(a), Err(ScoreError::Nan));
                continue;
            }
            if b.is_nan() {
                assert_eq!(encode_score(b), Err(ScoreError::Nan));
                continue;
            }

            let ea = encode_score(a).expect("non-NaN a");
            let eb = encode_score(b).expect("non-NaN b");

            let num_ord = a.partial_cmp(&b).expect("non-NaN comparable");
            assert_eq!(
                ea.cmp(&eb),
                num_ord,
                "order mismatch for a={a} ({:#x}) vs b={b} ({:#x})",
                a.to_bits(),
                b.to_bits()
            );

            let da = decode_score(ea);
            if a == 0.0 {
                assert_eq!(da.to_bits(), 0.0f64.to_bits());
            } else {
                assert_eq!(da.to_bits(), a.to_bits());
            }
            if num_ord == Ordering::Equal {
                assert_eq!(ea, eb);
            }
            tested += 1;
        }
    }
}
