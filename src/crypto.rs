use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit},
};
use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use rand::Rng;

/// The nonce (IV) length used by AES-256-GCM: 12 bytes (96 bits).
const NONCE_LEN: usize = 12;

/// Encrypts `plaintext` using AES-256-GCM with the provided 32-byte key.
///
/// A 12-byte random nonce is generated for each encryption operation.
/// The returned string is a base64-encoded concatenation of:
///
///   nonce (12 bytes) || ciphertext || GCM tag (16 bytes)
///
/// This format is self-contained so `decrypt` can recover all three parts.
pub fn encrypt(plaintext: &str, key: &[u8; 32]) -> Result<String> {
    // Create the cipher from the 32-byte key
    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|e| anyhow::anyhow!("Failed to create AES-256-GCM cipher: {e}"))?;

    // Generate a random 12-byte nonce
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from(nonce_bytes);

    // Encrypt the plaintext (GCM produces ciphertext + 16-byte tag)
    let ciphertext = cipher
        .encrypt(&nonce, plaintext.as_bytes())
        .map_err(|e| anyhow::anyhow!("Encryption failed: {e}"))?;

    // Prepend the nonce to the ciphertext so the result is self-contained
    let mut combined = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    combined.extend_from_slice(&nonce_bytes);
    combined.extend_from_slice(&ciphertext);

    // Encode the combined nonce + ciphertext as base64
    Ok(BASE64.encode(&combined))
}

/// Decrypts a base64-encoded ciphertext previously produced by `encrypt`.
///
/// Expects the same 32-byte key used during encryption. The input must be a
/// base64 string containing the 12-byte nonce followed by the ciphertext
/// (including the 16-byte GCM tag).
pub fn decrypt(encoded: &str, key: &[u8; 32]) -> Result<String> {
    // Decode from base64
    let combined = BASE64
        .decode(encoded)
        .context("Failed to decode base64 ciphertext")?;

    // Ensure we have at least the nonce length
    if combined.len() < NONCE_LEN {
        anyhow::bail!(
            "Ciphertext too short: expected at least {} bytes, got {}",
            NONCE_LEN,
            combined.len()
        );
    }

    // Split off the nonce
    let (nonce_bytes, ciphertext) = combined.split_at(NONCE_LEN);
    let nonce = Nonce::try_from(nonce_bytes).context("nonce prefix has the wrong length")?;

    // Create the cipher
    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|e| anyhow::anyhow!("Failed to create AES-256-GCM cipher: {e}"))?;

    // Decrypt (GCM verifies the tag and returns the plaintext)
    let plaintext = cipher
        .decrypt(&nonce, ciphertext)
        .map_err(|e| anyhow::anyhow!("Decryption failed: {e}"))?;

    // Convert the plaintext bytes back to a string
    let result = String::from_utf8(plaintext).context("Decrypted data is not valid UTF-8")?;

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let key = b"0123456789abcdef0123456789abcdef"; // 32 bytes
        let plaintext = "Hello, world!";

        let encrypted = encrypt(plaintext, key).unwrap();
        let decrypted = decrypt(&encrypted, key).unwrap();

        assert_eq!(plaintext, decrypted);
    }

    #[test]
    fn test_encrypt_decrypt_unicode() {
        let key = b"abcdef0123456789abcdef0123456789"; // 32 bytes
        let plaintext = "Hello 😀👍 JMAP → Matrix!";

        let encrypted = encrypt(plaintext, key).unwrap();
        let decrypted = decrypt(&encrypted, key).unwrap();

        assert_eq!(plaintext, decrypted);
    }

    #[test]
    fn test_decrypt_with_wrong_key_fails() {
        let key = b"0123456789abcdef0123456789abcdef"; // 32 bytes
        let wrong_key = b"ffffffffffffffffffffffffffffffff"; // 32 bytes
        let plaintext = "secret data";

        let encrypted = encrypt(plaintext, key).unwrap();
        let result = decrypt(&encrypted, wrong_key);

        assert!(result.is_err());
    }

    #[test]
    fn test_decrypt_invalid_base64_fails() {
        let key = b"0123456789abcdef0123456789abcdef";
        let result = decrypt("not-valid-base64!!!", key);
        assert!(result.is_err());
    }

    #[test]
    fn test_decrypt_truncated_ciphertext_fails() {
        let key = b"0123456789abcdef0123456789abcdef";
        // Valid base64 but only 3 bytes (less than 12-byte nonce)
        let result = decrypt("AAAA", key);
        assert!(result.is_err());
    }

    #[test]
    fn test_encrypt_produces_different_ciphertexts() {
        let key = b"0123456789abcdef0123456789abcdef";
        let plaintext = "same data";

        let encrypted1 = encrypt(plaintext, key).unwrap();
        let encrypted2 = encrypt(plaintext, key).unwrap();

        // With random nonces, two encryptions of the same data should differ
        assert_ne!(encrypted1, encrypted2);

        // Both should decrypt correctly
        assert_eq!(plaintext, decrypt(&encrypted1, key).unwrap());
        assert_eq!(plaintext, decrypt(&encrypted2, key).unwrap());
    }
}
