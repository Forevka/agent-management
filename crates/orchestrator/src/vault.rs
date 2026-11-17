//! Encryption at rest for secrets, bot tokens and Claude credentials.
//!
//! XChaCha20-Poly1305 with a random 24-byte nonce stored in front of each ciphertext. The
//! master key comes from the environment (`FLEET_MASTER_KEY`), never from the database, so a
//! database dump alone reveals no secret.

use anyhow::{Context, Result, bail};
use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit},
};
use rand::RngCore;

#[derive(Clone)]
pub struct Vault {
    cipher: XChaCha20Poly1305,
}

impl Vault {
    /// `hex_key` is 64 hex characters (`openssl rand -hex 32`).
    pub fn from_hex(hex_key: &str) -> Result<Self> {
        let bytes = data_encoding::HEXLOWER_PERMISSIVE
            .decode(hex_key.trim().as_bytes())
            .context("FLEET_MASTER_KEY is not hex")?;
        if bytes.len() != 32 {
            bail!(
                "FLEET_MASTER_KEY must be 32 bytes (64 hex characters), got {}",
                bytes.len()
            );
        }
        Ok(Self {
            cipher: XChaCha20Poly1305::new_from_slice(&bytes).expect("length checked"),
        })
    }

    pub fn encrypt(&self, plaintext: &str) -> Vec<u8> {
        let mut nonce = [0u8; 24];
        rand::rng().fill_bytes(&mut nonce);
        let mut out = nonce.to_vec();
        out.extend(
            self.cipher
                .encrypt(XNonce::from_slice(&nonce), plaintext.as_bytes())
                .expect("encryption cannot fail"),
        );
        out
    }

    pub fn decrypt(&self, data: &[u8]) -> Result<String> {
        if data.len() < 24 {
            bail!("ciphertext too short");
        }
        let (nonce, ct) = data.split_at(24);
        let plain = self
            .cipher
            .decrypt(XNonce::from_slice(nonce), ct)
            .map_err(|_| anyhow::anyhow!("decryption failed (wrong FLEET_MASTER_KEY?)"))?;
        Ok(String::from_utf8(plain)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_tamper_detection() {
        let v = Vault::from_hex(&"ab".repeat(32)).unwrap();
        let ct = v.encrypt("s3cret");
        assert_eq!(v.decrypt(&ct).unwrap(), "s3cret");
        assert_ne!(v.encrypt("s3cret"), ct, "random nonce per encryption");
        let mut bad = ct.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(v.decrypt(&bad).is_err());
        let other = Vault::from_hex(&"cd".repeat(32)).unwrap();
        assert!(other.decrypt(&ct).is_err());
        assert!(Vault::from_hex("abcd").is_err());
    }
}
