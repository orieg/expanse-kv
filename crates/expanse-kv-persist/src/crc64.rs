//! CRC64-Jones checksum (Redis RDB compatible), zero-dependency and `#![forbid(unsafe_code)]`.
//!
//! Uses the Jones (2010) polynomial `0xad93d23594c935a9` (bit-reflected
//! `0x95ac9329ac4bc9b5`), initial value `0`, and no final XOR. Matches the
//! standard Redis CRC64 test vector:
//! `crc64(b"123456789") == 0xe9c6d914c4b8d9ca`.

/// Bit-reflected CRC64-Jones polynomial (`0xad93d23594c935a9u64.reverse_bits()`).
const POLY: u64 = 0x95ac_9329_ac4b_c9b5;

/// Build the 256-entry lookup table at compile time.
const fn build_table() -> [u64; 256] {
    let mut table = [0u64; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut crc = i as u64;
        let mut bit = 0;
        while bit < 8 {
            if (crc & 1) != 0 {
                crc = (crc >> 1) ^ POLY;
            } else {
                crc >>= 1;
            }
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

static CRC64_TABLE: [u64; 256] = build_table();

/// Streaming CRC64-Jones hasher.
#[derive(Debug, Clone, Copy, Default)]
pub struct Crc64 {
    state: u64,
}

impl Crc64 {
    /// Creates a new CRC64 hasher initialized to `0`.
    #[must_use]
    pub const fn new() -> Self {
        Self { state: 0 }
    }

    /// Feeds `bytes` into the running CRC64 state.
    pub fn update(&mut self, bytes: &[u8]) {
        let mut crc = self.state;
        for &b in bytes {
            let idx = ((crc ^ u64::from(b)) & 0xff) as usize;
            crc = CRC64_TABLE[idx] ^ (crc >> 8);
        }
        self.state = crc;
    }

    /// Returns the current CRC64 digest.
    #[must_use]
    pub const fn finalize(&self) -> u64 {
        self.state
    }
}

/// Computes the CRC64-Jones digest of `bytes` in one call.
#[must_use]
pub fn crc64(bytes: &[u8]) -> u64 {
    let mut hasher = Crc64::new();
    hasher.update(bytes);
    hasher.finalize()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_redis_standard_check_vector() {
        assert_eq!(crc64(b"123456789"), 0xe9c6_d914_c4b8_d9ca);
    }

    #[test]
    fn empty_input_is_zero() {
        assert_eq!(crc64(b""), 0);
    }

    #[test]
    fn streaming_matches_single_shot() {
        let mut h = Crc64::new();
        h.update(b"1234");
        h.update(b"56789");
        assert_eq!(h.finalize(), crc64(b"123456789"));
    }

    #[test]
    fn single_bit_flip_changes_digest() {
        let orig = b"expanse-kv-snapshot-payload-check";
        let expected = crc64(orig);
        for byte_idx in 0..orig.len() {
            for bit in 0..8 {
                let mut mutated = *orig;
                mutated[byte_idx] ^= 1u8 << bit;
                assert_ne!(
                    crc64(&mutated),
                    expected,
                    "single-bit flip at byte {byte_idx} bit {bit} collided"
                );
            }
        }
    }
}
