//! String keys as `u128`.
//!
//! A string of at most 15 bytes is packed big-endian and left-aligned with its length in
//! the low byte, so integer order is byte-wise string order and the packing is exact.
//! Longer strings (never produced by the generator, but legal) are interned in a global
//! table and get the key `(id << 8) | 0xFF`; `key_cmp` compares those by their bytes.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::Mutex;

pub const P_BITS: u32 = 8;
/// Number of hash partitions.
pub const P: usize = 1 << P_BITS;

struct LongTable {
    ids: HashMap<Box<[u8]>, u32>,
    strings: Vec<Box<[u8]>>,
}

static LONG: Mutex<Option<LongTable>> = Mutex::new(None);

#[inline]
pub fn pack(s: &[u8]) -> u128 {
    if s.len() <= 15 {
        let mut buf = [0u8; 16];
        buf[..s.len()].copy_from_slice(s);
        buf[15] = s.len() as u8;
        u128::from_be_bytes(buf)
    } else {
        pack_long(s)
    }
}

#[cold]
fn pack_long(s: &[u8]) -> u128 {
    let mut guard = LONG.lock().unwrap();
    let table = guard.get_or_insert_with(|| LongTable { ids: HashMap::new(), strings: Vec::new() });
    let id = match table.ids.get(s) {
        Some(&id) => id,
        None => {
            let id = table.strings.len() as u32;
            table.strings.push(s.into());
            table.ids.insert(s.into(), id);
            id
        }
    };
    ((id as u128) << 8) | 0xFF
}

#[inline]
pub fn is_long(k: u128) -> bool {
    (k as u8) > 15
}

/// The string a key stands for.
pub fn to_vec(k: u128) -> Vec<u8> {
    if !is_long(k) {
        let buf = k.to_be_bytes();
        buf[..buf[15] as usize].to_vec()
    } else {
        let guard = LONG.lock().unwrap();
        guard.as_ref().expect("long key without table").strings[(k >> 8) as usize].to_vec()
    }
}

/// Byte-wise order of the strings behind two keys.
#[inline]
pub fn key_cmp(a: u128, b: u128) -> Ordering {
    if !is_long(a) && !is_long(b) { a.cmp(&b) } else { cmp_slow(a, b) }
}

#[cold]
fn cmp_slow(a: u128, b: u128) -> Ordering {
    to_vec(a).cmp(&to_vec(b))
}

/// A strong 64-bit mix of a key.
#[inline]
pub fn mix(k: u128) -> u64 {
    let mut h = (k as u64) ^ ((k >> 64) as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(32);
    h ^= h >> 31;
    h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    h ^= h >> 29;
    h = h.wrapping_mul(0x94D0_49BB_1331_11EB);
    h ^ (h >> 32)
}

/// Partition of a key. Uses hash bits 32..40, which hashbrown does not use for bucket
/// index or tag at our table sizes, so a per-partition map stays well spread.
#[inline]
pub fn part_of(k: u128) -> usize {
    ((mix(k) >> 32) as usize) & (P - 1)
}

#[derive(Default, Clone, Copy)]
pub struct KeyHasher(u64);

impl Hasher for KeyHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ b as u64).wrapping_mul(0x0000_0100_0000_01B3);
        }
    }
    #[inline]
    fn write_u128(&mut self, k: u128) {
        self.0 = mix(k);
    }
}

pub type KeyMap<V> = HashMap<u128, V, BuildHasherDefault<KeyHasher>>;

pub fn key_map<V>(capacity: usize) -> KeyMap<V> {
    KeyMap::with_capacity_and_hasher(capacity, Default::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order() {
        let words: [&[u8]; 9] = [b"U1", b"u1", b"u1 ", b" u1", b"u1\0", b"u10", b"u2", b"a", b"user_id_a\tq"];
        for a in words {
            for b in words {
                assert_eq!(key_cmp(pack(a), pack(b)), a.cmp(b), "{a:?} {b:?}");
            }
        }
        let long = b"this-is-a-very-long-identifier";
        let k = pack(long);
        assert!(is_long(k));
        assert_eq!(pack(long), k);
        assert_eq!(to_vec(k), long.to_vec());
        assert_eq!(key_cmp(k, pack(b"this")), Ordering::Greater);
        assert_eq!(key_cmp(k, pack(b"u")), Ordering::Less);
        assert_eq!(to_vec(pack(b"u1 ")), b"u1 ".to_vec());
    }
}
