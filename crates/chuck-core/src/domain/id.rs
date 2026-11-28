//! Identifiers of cards, files and rules: 26 characters of Crockford base32 (`0-9A-Z` without `I L O U`), in the shape of a
//! ULID. The first ten characters are the creation time in milliseconds, the other sixteen are random, so ids are unique
//! without asking a database, sort in creation order, and say nothing else. Clients treat them as opaque text.
//!
//! This module only encodes and checks; making a new id needs a clock and a source of randomness, which are in
//! `chuck_data::ids`.

pub const LEN: usize = 26;
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Encodes a millisecond timestamp (48 bits used) and 80 random bits.
pub fn encode(ms: u64, random: u128) -> String {
    let value: u128 = (((ms as u128) & 0xFFFF_FFFF_FFFF) << 80) | (random & ((1u128 << 80) - 1));
    let mut out = [b'0'; LEN];
    for (i, slot) in out.iter_mut().enumerate() {
        let shift = 5 * (LEN - 1 - i) as u32;
        *slot = ALPHABET[((value >> shift) & 31) as usize];
    }
    String::from_utf8(out.to_vec()).expect("alphabet is ASCII")
}

/// Whether the text has the shape of an id. Anything else cannot be one of ours, so a request for it is simply not found.
pub fn is_valid(s: &str) -> bool {
    s.len() == LEN && s.bytes().all(|c| ALPHABET.contains(&c)) && s.as_bytes()[0] <= b'7'
}

/// The millisecond timestamp inside an id, if it is one.
pub fn timestamp_ms(s: &str) -> Option<u64> {
    if !is_valid(s) {
        return None;
    }
    let mut v: u128 = 0;
    for c in s.bytes() {
        v = (v << 5) | ALPHABET.iter().position(|a| *a == c).unwrap() as u128;
    }
    Some((v >> 80) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shape_round_trip_and_order() {
        let a = encode(1_700_000_000_000, 5);
        assert_eq!(a.len(), LEN);
        assert!(is_valid(&a), "{a}");
        assert_eq!(timestamp_ms(&a), Some(1_700_000_000_000));
        // later time sorts later, whatever the random part; same time sorts by the random part
        assert!(encode(1_700_000_000_001, 0) > encode(1_700_000_000_000, u128::MAX));
        assert!(encode(7, 1) < encode(7, 2));
        assert_eq!(encode(0, 0), "0".repeat(26));
        assert!(is_valid(&encode(u64::MAX, u128::MAX)), "extra bits are cut off, never producing a bad id");
    }

    #[test]
    fn only_ids_are_valid() {
        let (a26, z27) = ("A".repeat(26), "0".repeat(27));
        for bad in ["", "1", "9999999999999999999999999I", a26.as_str(), "01ARZ3NDEKTSV4RRFFQ69G5FAv", z27.as_str(), "../../etc/passwd", "42"] {
            assert!(!is_valid(bad), "{bad}");
        }
        assert!(is_valid("01ARZ3NDEKTSV4RRFFQ69G5FAV"));
        assert_eq!(timestamp_ms("nope"), None);
    }
}
