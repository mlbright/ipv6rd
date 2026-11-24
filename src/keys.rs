//! The fwknop keys and the one SPA scheme ipv6rd sends (ADR-0006).
//!
//! This module is the only place an encryption mode or HMAC digest is chosen.
//! fkospa's API also offers the legacy IV mode and weaker digests; nothing in
//! ipv6rd's config can reach them, and the tests in `knock` hold that line.

use fkospa::{DigestType, EncryptionKey, EncryptionMode, HmacKey, HmacType, KeySet};
use zeroize::Zeroizing;

/// Rijndael (AES) in CBC mode — fwknop's default.
pub const ENCRYPTION_MODE: EncryptionMode = EncryptionMode::Cbc;
/// HMAC over the ciphertext (encrypt-then-MAC).
pub const HMAC_TYPE: HmacType = HmacType::Sha256;
/// The message digest inside the encrypted payload.
pub const DIGEST_TYPE: DigestType = DigestType::Sha256;

/// fwknop derives the AES key with one MD5 round and no stretching, which is
/// only safe for random keys: exactly what `fwknop --key-gen` produces.
pub const ENCRYPTION_KEY_LEN: usize = 32;
pub const HMAC_KEY_MIN: usize = 32;
pub const HMAC_KEY_MAX: usize = 128;

/// Checks the decoded keys against the key rules and builds the key set every
/// packet is encrypted with. Consumes the buffers; the key bytes move into
/// fkospa's key types, which zero them on drop.
pub fn build_keyset(
    mut encryption: Zeroizing<Vec<u8>>,
    mut hmac: Zeroizing<Vec<u8>>,
) -> Result<KeySet, String> {
    if encryption.len() != ENCRYPTION_KEY_LEN {
        return Err(format!(
            "the encryption key (fwknop.key_ref) is {} bytes after base64 decoding; it must be \
             exactly {ENCRYPTION_KEY_LEN} random bytes (generate one with `fwknop --key-gen`)",
            encryption.len()
        ));
    }
    if !(HMAC_KEY_MIN..=HMAC_KEY_MAX).contains(&hmac.len()) {
        return Err(format!(
            "the HMAC key (fwknop.hmac_ref) is {} bytes after base64 decoding; it must be \
             {HMAC_KEY_MIN} to {HMAC_KEY_MAX} random bytes (generate one with `fwknop --key-gen`)",
            hmac.len()
        ));
    }
    if encryption[..] == hmac[..] {
        return Err(
            "the encryption key and the HMAC key are identical; they must be different keys".into(),
        );
    }
    Ok(KeySet::new(
        EncryptionKey::new(std::mem::take(&mut *encryption)),
        ENCRYPTION_MODE,
        Some(HmacKey::new(std::mem::take(&mut *hmac), HMAC_TYPE)),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(len: usize, fill: u8) -> Zeroizing<Vec<u8>> {
        Zeroizing::new(vec![fill; len])
    }

    #[test]
    fn accepts_key_gen_sized_keys() {
        for hmac_len in [32, 64, 128] {
            let ks = build_keyset(key(32, 1), key(hmac_len, 2)).unwrap();
            assert_eq!(ks.encryption_mode, EncryptionMode::Cbc);
            let h = ks.hmac.as_ref().unwrap();
            assert_eq!(h.hmac_type(), HmacType::Sha256);
            assert_eq!(h.len(), hmac_len);
            assert_eq!(ks.encryption_key.len(), 32);
        }
    }

    #[test]
    fn rejects_wrong_encryption_key_lengths() {
        for len in [0, 16, 31, 33, 64] {
            let e = build_keyset(key(len, 1), key(64, 2)).unwrap_err();
            assert!(e.contains("must be exactly 32"), "{len}: {e}");
        }
    }

    #[test]
    fn rejects_wrong_hmac_key_lengths() {
        for len in [0, 16, 31, 129, 256] {
            let e = build_keyset(key(32, 1), key(len, 2)).unwrap_err();
            assert!(e.contains("32 to 128"), "{len}: {e}");
        }
    }

    #[test]
    fn rejects_identical_keys() {
        let e = build_keyset(key(32, 7), key(32, 7)).unwrap_err();
        assert!(e.contains("identical"), "{e}");
        // Same prefix but longer HMAC key is a different key.
        assert!(build_keyset(key(32, 7), key(64, 7)).is_ok());
    }

    #[test]
    fn errors_never_contain_key_bytes() {
        let e = build_keyset(key(31, b'Z'), key(64, b'Q')).unwrap_err();
        assert!(!e.contains("ZZZZ") && !e.contains("QQQQ"));
    }
}
