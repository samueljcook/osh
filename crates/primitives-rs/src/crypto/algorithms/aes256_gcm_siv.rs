//! Raw AES-256-GCM-SIV authenticated encryption from RFC 8452.
//!
//! Callers should use fresh 12-byte nonces. Misuse resistance reduces damage
//! from accidental nonce reuse; repeated nonces can reveal repeated plaintexts.
//! [`crate::crypto::encryption::encrypt`] owns nonce generation, key derivation, and scheme hedging.
aead_algorithm!(::aes_gcm_siv::Aes256GcmSiv, 12);
