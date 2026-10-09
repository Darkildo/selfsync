//! SHA-256 и 32-байтовый хэш, который ходит по протоколу сырыми байтами.

use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// SHA-256, 32 байта.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default)]
pub struct Hash(pub [u8; 32]);

impl Hash {
    pub fn of(data: &[u8]) -> Hash {
        Hash(Sha256::digest(data).into())
    }

    pub fn from_slice(b: &[u8]) -> Option<Hash> {
        <[u8; 32]>::try_from(b).ok().map(Hash)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_vec(&self) -> Vec<u8> {
        self.0.to_vec()
    }

    pub fn to_hex(&self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut s = String::with_capacity(64);
        for b in self.0 {
            s.push(char::from(HEX[usize::from(b >> 4)]));
            s.push(char::from(HEX[usize::from(b & 0xf)]));
        }
        s
    }

    pub fn from_hex(s: &str) -> Option<Hash> {
        if s.len() != 64 {
            return None;
        }
        let mut out = [0u8; 32];
        let bytes = s.as_bytes();
        for (i, o) in out.iter_mut().enumerate() {
            let hi = hex_val(bytes[2 * i])?;
            let lo = hex_val(bytes[2 * i + 1])?;
            *o = (hi << 4) | lo;
        }
        Some(Hash(out))
    }
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{}", &self.to_hex()[..12])
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// Потоковый SHA-256.
#[derive(Clone, Default)]
pub struct Hasher(Sha256);

impl Hasher {
    pub fn new() -> Self {
        Self(Sha256::new())
    }

    pub fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    pub fn finish(self) -> Hash {
        Hash(self.0.finalize().into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vector() {
        assert_eq!(
            Hash::of(b"abc").to_hex(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let mut h = Hasher::new();
        h.update(b"a");
        h.update(b"bc");
        assert_eq!(h.finish(), Hash::of(b"abc"));
    }

    #[test]
    fn hex_roundtrip() {
        let h = Hash::of(b"x");
        assert_eq!(Hash::from_hex(&h.to_hex()), Some(h));
        assert_eq!(Hash::from_hex(&h.to_hex().to_uppercase()), Some(h));
        assert_eq!(Hash::from_hex("zz"), None);
        assert_eq!(Hash::from_slice(&[0; 31]), None);
    }
}
