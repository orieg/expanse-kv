//! Composite binary key codecs and type-prefix registry (`AGENTS.md` §2.8,
//! `docs/PRD.md` §3.2).
//!
//! All logical structures share a bounded set of `SyncExpanseOrderedBytesMap`
//! instances under composite keys with distinct leading type-prefix bytes:
//!
//! | Logical Type | Prefix | Layout |
//! | :--- | :---: | :--- |
//! | String / Binary KV | `0x01` | `[0x01][key]` |
//! | Hash field | `0x02` | `[0x02][len_varint(key)][key][field]` |
//! | Sorted Set score index | `0x03` | `[0x03][len_varint(key)][key][score_be_u64][member]` |
//! | Sorted Set member index | `0x04` | `[0x04][len_varint(key)][key][member]` |
//! | TTL / Expiration index | — | `[expiry_ms_be_u64][key]` (dedicated expiry trie) |
//!
//! Because `len_varint(key)` is a self-delimiting unsigned LEB128 varint, the
//! prefix `[tag][len_varint(key)][key]` is prefix-free across distinct logical
//! keys: no logical key `K1`'s composite prefix can ever be a byte prefix of a
//! distinct logical key `K2`'s composite key.

use crate::score::{ScoreError, decode_score_slice, encode_score};
use alloc::vec::Vec;
use core::fmt;

/// Leading type-prefix byte for String / Binary KV keys.
pub const PREFIX_STRING: u8 = 0x01;

/// Leading type-prefix byte for Hash field entries.
pub const PREFIX_HASH: u8 = 0x02;

/// Leading type-prefix byte for Sorted Set `(score, member)` index entries.
pub const PREFIX_ZSET_SCORE: u8 = 0x03;

/// Leading type-prefix byte for Sorted Set `member -> score` index entries.
pub const PREFIX_ZSET_MEMBER: u8 = 0x04;

/// Error returned when encoding or decoding a composite key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyCodecError {
    /// Input key is empty or shorter than the required header.
    Truncated,
    /// Leading type-prefix byte does not match the expected logical type.
    UnexpectedPrefix {
        /// Expected leading byte.
        expected: u8,
        /// Observed leading byte.
        found: u8,
    },
    /// Unsigned LEB128 length varint was malformed or overflowed `usize`.
    InvalidVarint,
    /// Score encoding/decoding error (e.g., `NaN` score).
    Score(ScoreError),
}

impl From<ScoreError> for KeyCodecError {
    fn from(err: ScoreError) -> Self {
        Self::Score(err)
    }
}

impl fmt::Display for KeyCodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => f.write_str("composite key is truncated"),
            Self::UnexpectedPrefix { expected, found } => {
                write!(
                    f,
                    "unexpected key prefix: expected 0x{expected:02x}, found 0x{found:02x}"
                )
            }
            Self::InvalidVarint => f.write_str("invalid or overflowing LEB128 length varint"),
            Self::Score(e) => write!(f, "score codec error: {e}"),
        }
    }
}

impl core::error::Error for KeyCodecError {}

/// Encodes `value` as an unsigned LEB128 varint into `out`.
#[inline]
pub fn encode_varint(mut value: usize, out: &mut Vec<u8>) {
    loop {
        let mut byte = (value & 0x7F) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            break;
        }
    }
}

/// Decodes an unsigned LEB128 varint from the front of `input`, returning
/// `(value, remaining_slice)`.
///
/// # Errors
///
/// Returns [`KeyCodecError::InvalidVarint`] on truncated or overflowing input.
#[inline]
pub fn decode_varint(input: &[u8]) -> Result<(usize, &[u8]), KeyCodecError> {
    let mut value: usize = 0;
    let mut shift: u32 = 0;
    for (idx, &byte) in input.iter().enumerate() {
        let low = usize::from(byte & 0x7F);
        let shifted = low.checked_shl(shift).ok_or(KeyCodecError::InvalidVarint)?;
        if (shifted >> shift) != low {
            return Err(KeyCodecError::InvalidVarint);
        }
        value |= shifted;
        if (byte & 0x80) == 0 {
            return Ok((value, &input[idx + 1..]));
        }
        shift = shift.checked_add(7).ok_or(KeyCodecError::InvalidVarint)?;
        if shift >= usize::BITS {
            return Err(KeyCodecError::InvalidVarint);
        }
    }
    Err(KeyCodecError::InvalidVarint)
}

/// Encodes `[tag][len_varint(key)][key]` into `out`.
#[inline]
fn encode_tagged_key_prefix(tag: u8, key: &[u8], out: &mut Vec<u8>) {
    out.push(tag);
    encode_varint(key.len(), out);
    out.extend_from_slice(key);
}

/// Decodes `[tag][len_varint(key)][key]` from `input`, returning `(key, rest)`.
#[inline]
fn decode_tagged_key_prefix(
    input: &[u8],
    expected_tag: u8,
) -> Result<(&[u8], &[u8]), KeyCodecError> {
    let (&first, after_tag) = input.split_first().ok_or(KeyCodecError::Truncated)?;
    if first != expected_tag {
        return Err(KeyCodecError::UnexpectedPrefix {
            expected: expected_tag,
            found: first,
        });
    }
    let (key_len, after_len) = decode_varint(after_tag)?;
    if after_len.len() < key_len {
        return Err(KeyCodecError::Truncated);
    }
    Ok(after_len.split_at(key_len))
}

// ---------------------------------------------------------------------------
// String / Binary KV (`PREFIX_STRING = 0x01`)
// ---------------------------------------------------------------------------

/// Encodes a String / Binary KV key `[0x01][key]` into `out` (clearing `out` first).
#[inline]
pub fn encode_string_key(key: &[u8], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(1 + key.len());
    out.push(PREFIX_STRING);
    out.extend_from_slice(key);
}

/// Decodes a String / Binary KV key `[0x01][key]`, returning the raw `key` slice.
///
/// # Errors
///
/// Returns [`KeyCodecError::Truncated`] or [`KeyCodecError::UnexpectedPrefix`].
#[inline]
pub fn decode_string_key(encoded: &[u8]) -> Result<&[u8], KeyCodecError> {
    let (&first, rest) = encoded.split_first().ok_or(KeyCodecError::Truncated)?;
    if first != PREFIX_STRING {
        return Err(KeyCodecError::UnexpectedPrefix {
            expected: PREFIX_STRING,
            found: first,
        });
    }
    Ok(rest)
}

// ---------------------------------------------------------------------------
// Hash (`PREFIX_HASH = 0x02`)
// ---------------------------------------------------------------------------

/// Encodes the prefix `[0x02][len_varint(key)][key]` for scanning all fields of `key`.
#[inline]
pub fn encode_hash_prefix(key: &[u8], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(1 + 10 + key.len());
    encode_tagged_key_prefix(PREFIX_HASH, key, out);
}

/// Encodes a Hash field entry `[0x02][len_varint(key)][key][field]` into `out`.
#[inline]
pub fn encode_hash_field_key(key: &[u8], field: &[u8], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(1 + 10 + key.len() + field.len());
    encode_tagged_key_prefix(PREFIX_HASH, key, out);
    out.extend_from_slice(field);
}

/// Decodes a Hash field entry, returning `(key, field)`.
///
/// # Errors
///
/// Returns [`KeyCodecError`] if the prefix, length varint, or slice is invalid.
#[inline]
pub fn decode_hash_field_key(encoded: &[u8]) -> Result<(&[u8], &[u8]), KeyCodecError> {
    decode_tagged_key_prefix(encoded, PREFIX_HASH)
}

// ---------------------------------------------------------------------------
// Sorted Set score index (`PREFIX_ZSET_SCORE = 0x03`)
// ---------------------------------------------------------------------------

/// Encodes the prefix `[0x03][len_varint(key)][key]` for scanning all score entries of `key`.
#[inline]
pub fn encode_zset_score_prefix(key: &[u8], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(1 + 10 + key.len());
    encode_tagged_key_prefix(PREFIX_ZSET_SCORE, key, out);
}

/// Encodes a Sorted Set score-range bound `[0x03][len_varint(key)][key][score_be_u64]`.
///
/// # Errors
///
/// Returns [`KeyCodecError::Score`] if `score` is `NaN`.
#[inline]
pub fn encode_zset_score_bound(
    key: &[u8],
    score: f64,
    out: &mut Vec<u8>,
) -> Result<(), KeyCodecError> {
    let score_bytes = encode_score(score)?;
    out.clear();
    out.reserve(1 + 10 + key.len() + 8);
    encode_tagged_key_prefix(PREFIX_ZSET_SCORE, key, out);
    out.extend_from_slice(&score_bytes);
    Ok(())
}

/// Encodes a Sorted Set score index entry
/// `[0x03][len_varint(key)][key][score_be_u64][member]`.
///
/// # Errors
///
/// Returns [`KeyCodecError::Score`] if `score` is `NaN`.
#[inline]
pub fn encode_zset_score_key(
    key: &[u8],
    score: f64,
    member: &[u8],
    out: &mut Vec<u8>,
) -> Result<(), KeyCodecError> {
    let score_bytes = encode_score(score)?;
    out.clear();
    out.reserve(1 + 10 + key.len() + 8 + member.len());
    encode_tagged_key_prefix(PREFIX_ZSET_SCORE, key, out);
    out.extend_from_slice(&score_bytes);
    out.extend_from_slice(member);
    Ok(())
}

/// Decodes a Sorted Set score index entry, returning `(key, score, member)`.
///
/// # Errors
///
/// Returns [`KeyCodecError`] if the prefix, varint, or 8-byte score is invalid.
#[inline]
pub fn decode_zset_score_key(encoded: &[u8]) -> Result<(&[u8], f64, &[u8]), KeyCodecError> {
    let (key, rest) = decode_tagged_key_prefix(encoded, PREFIX_ZSET_SCORE)?;
    if rest.len() < 8 {
        return Err(KeyCodecError::Truncated);
    }
    let (score_slice, member) = rest.split_at(8);
    let score = decode_score_slice(score_slice)?;
    Ok((key, score, member))
}

// ---------------------------------------------------------------------------
// Sorted Set member index (`PREFIX_ZSET_MEMBER = 0x04`)
// ---------------------------------------------------------------------------

/// Encodes the prefix `[0x04][len_varint(key)][key]` for scanning all members of `key`.
#[inline]
pub fn encode_zset_member_prefix(key: &[u8], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(1 + 10 + key.len());
    encode_tagged_key_prefix(PREFIX_ZSET_MEMBER, key, out);
}

/// Encodes a Sorted Set member index entry `[0x04][len_varint(key)][key][member]`.
#[inline]
pub fn encode_zset_member_key(key: &[u8], member: &[u8], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(1 + 10 + key.len() + member.len());
    encode_tagged_key_prefix(PREFIX_ZSET_MEMBER, key, out);
    out.extend_from_slice(member);
}

/// Decodes a Sorted Set member index entry, returning `(key, member)`.
///
/// # Errors
///
/// Returns [`KeyCodecError`] if the prefix, varint, or slice is invalid.
#[inline]
pub fn decode_zset_member_key(encoded: &[u8]) -> Result<(&[u8], &[u8]), KeyCodecError> {
    decode_tagged_key_prefix(encoded, PREFIX_ZSET_MEMBER)
}

// ---------------------------------------------------------------------------
// TTL / Expiration Index (`[expiry_ms_be_u64][key]`)
// ---------------------------------------------------------------------------

/// Encodes a TTL expiration-index entry `[expiry_ms_be_u64][key]` into `out`.
///
/// Big-endian timestamp encoding guarantees that ascending byte-lexicographical
/// iteration yields keys in earliest-expiration order (`docs/PRD.md` §3.2).
#[inline]
pub fn encode_ttl_key(expiry_ms: u64, key: &[u8], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(8 + key.len());
    out.extend_from_slice(&expiry_ms.to_be_bytes());
    out.extend_from_slice(key);
}

/// Decodes a TTL expiration-index entry `[expiry_ms_be_u64][key]`, returning
/// `(expiry_ms, key)`.
///
/// # Errors
///
/// Returns [`KeyCodecError::Truncated`] if `encoded.len() < 8`.
#[inline]
pub fn decode_ttl_key(encoded: &[u8]) -> Result<(u64, &[u8]), KeyCodecError> {
    if encoded.len() < 8 {
        return Err(KeyCodecError::Truncated);
    }
    let (ts_slice, key) = encoded.split_at(8);
    let ts_bytes: [u8; 8] = ts_slice.try_into().map_err(|_| KeyCodecError::Truncated)?;
    Ok((u64::from_be_bytes(ts_bytes), key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    struct SplitMix64(u64);
    impl SplitMix64 {
        fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        fn next_bytes(&mut self, max_len: usize) -> Vec<u8> {
            let len = (self.next_u64() as usize) % (max_len + 1);
            let mut v = Vec::with_capacity(len);
            for _ in 0..len {
                v.push((self.next_u64() & 0xFF) as u8);
            }
            v
        }
    }

    #[test]
    fn varint_round_trip_and_malformed_rejection() {
        let mut buf = Vec::new();
        for &v in &[
            0usize,
            1,
            127,
            128,
            255,
            16_383,
            16_384,
            1_000_000,
            usize::MAX,
        ] {
            buf.clear();
            encode_varint(v, &mut buf);
            let (decoded, rest) = decode_varint(&buf).expect("valid varint");
            assert_eq!(decoded, v);
            assert!(rest.is_empty());
        }
        // Truncated continuation byte.
        assert_eq!(decode_varint(&[0x80]), Err(KeyCodecError::InvalidVarint));
        // Empty slice.
        assert_eq!(decode_varint(&[]), Err(KeyCodecError::InvalidVarint));
        // Overflowing 11-byte varint.
        assert_eq!(
            decode_varint(&[0xFF; 11]),
            Err(KeyCodecError::InvalidVarint)
        );
    }

    #[test]
    fn composite_key_round_trip_and_type_isolation() {
        let mut buf = Vec::new();
        let key = b"org:user\x00:session\x01:42";
        let field = b"field\x00\x01\xFF";

        // String
        encode_string_key(key, &mut buf);
        assert_eq!(decode_string_key(&buf), Ok(key.as_slice()));
        assert_eq!(
            decode_hash_field_key(&buf),
            Err(KeyCodecError::UnexpectedPrefix {
                expected: PREFIX_HASH,
                found: PREFIX_STRING
            })
        );

        // Hash
        encode_hash_field_key(key, field, &mut buf);
        assert_eq!(
            decode_hash_field_key(&buf),
            Ok((key.as_slice(), field.as_slice()))
        );

        // ZSet score
        encode_zset_score_key(key, -42.5, field, &mut buf).expect("valid zset score");
        assert_eq!(
            decode_zset_score_key(&buf),
            Ok((key.as_slice(), -42.5, field.as_slice()))
        );

        // ZSet member
        encode_zset_member_key(key, field, &mut buf);
        assert_eq!(
            decode_zset_member_key(&buf),
            Ok((key.as_slice(), field.as_slice()))
        );

        // TTL
        encode_ttl_key(1_700_000_000_123, key, &mut buf);
        assert_eq!(
            decode_ttl_key(&buf),
            Ok((1_700_000_000_123, key.as_slice()))
        );
        assert_eq!(decode_ttl_key(&[0u8; 7]), Err(KeyCodecError::Truncated));
    }

    #[test]
    fn hash_and_zset_prefixes_never_collide_across_prefix_sharing_keys() {
        // Key "a" vs key "ab" with field "c": "a"'s prefix must NOT match "ab"'s entry.
        let mut p_a = Vec::new();
        let mut e_ab = Vec::new();
        encode_hash_prefix(b"a", &mut p_a);
        encode_hash_field_key(b"ab", b"c", &mut e_ab);
        assert!(!e_ab.starts_with(&p_a));

        // Key "a" with field "bc" MUST match "a"'s prefix.
        let mut e_a = Vec::new();
        encode_hash_field_key(b"a", b"bc", &mut e_a);
        assert!(e_a.starts_with(&p_a));
        assert_eq!(
            decode_hash_field_key(&e_a),
            Ok((b"a".as_slice(), b"bc".as_slice()))
        );
    }

    #[test]
    fn zset_score_keys_sort_by_score_then_member_and_ttl_sorts_by_expiry() {
        let key = b"leaderboard";
        let entries = [
            (f64::NEG_INFINITY, b"z".as_slice()),
            (-10.0, b"b".as_slice()),
            (-10.0, b"c".as_slice()),
            (-0.0, b"a".as_slice()),
            (0.0, b"b".as_slice()),
            (1.5, b"a".as_slice()),
            (f64::INFINITY, b"a".as_slice()),
        ];

        let mut encoded = Vec::new();
        for &(score, member) in &entries {
            let mut buf = Vec::new();
            encode_zset_score_key(key, score, member, &mut buf).expect("valid score");
            encoded.push(buf);
        }
        for pair in encoded.windows(2) {
            assert!(pair[0] < pair[1], "expected {:?} < {:?}", pair[0], pair[1]);
        }

        // TTL ordering.
        let mut t1 = Vec::new();
        let mut t2 = Vec::new();
        encode_ttl_key(100, b"zzz", &mut t1);
        encode_ttl_key(101, b"aaa", &mut t2);
        assert!(t1 < t2);
    }

    #[test]
    fn property_10k_random_composite_keys_round_trip_and_preserve_order() {
        let mut rng = SplitMix64(0x6E4C_0DEC_2026);
        let mut buf1 = Vec::new();
        let mut buf2 = Vec::new();
        let mut pref1 = Vec::new();

        for _ in 0..10_000 {
            let k1 = rng.next_bytes(48);
            let k2 = rng.next_bytes(48);
            let f1 = rng.next_bytes(32);
            let f2 = rng.next_bytes(32);

            // String order matches raw key order.
            encode_string_key(&k1, &mut buf1);
            encode_string_key(&k2, &mut buf2);
            assert_eq!(buf1.cmp(&buf2), k1.cmp(&k2));
            assert_eq!(decode_string_key(&buf1), Ok(k1.as_slice()));

            // Hash prefix non-collision and within-key field order.
            encode_hash_prefix(&k1, &mut pref1);
            encode_hash_field_key(&k1, &f1, &mut buf1);
            encode_hash_field_key(&k1, &f2, &mut buf2);
            assert!(buf1.starts_with(&pref1));
            assert_eq!(buf1.cmp(&buf2), f1.cmp(&f2));
            assert_eq!(
                decode_hash_field_key(&buf1),
                Ok((k1.as_slice(), f1.as_slice()))
            );

            if k1 != k2 {
                encode_hash_field_key(&k2, &f2, &mut buf2);
                assert!(!buf2.starts_with(&pref1));
            }

            // ZSet member round-trip.
            encode_zset_member_key(&k1, &f1, &mut buf1);
            assert_eq!(
                decode_zset_member_key(&buf1),
                Ok((k1.as_slice(), f1.as_slice()))
            );
        }

        // Verify empty key / empty field edge case.
        let mut empty_buf = vec![];
        encode_hash_field_key(&[], &[], &mut empty_buf);
        assert_eq!(decode_hash_field_key(&empty_buf), Ok((&[][..], &[][..])));
    }
}
