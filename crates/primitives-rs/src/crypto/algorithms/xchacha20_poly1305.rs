//! Raw XChaCha20-Poly1305 authenticated encryption.
//!
//! Callers must use a fresh 24-byte nonce for every encryption under a key.
//! [`crate::crypto::encryption::encrypt`] owns nonce generation, key derivation, and scheme hedging.
aead_algorithm!(::chacha20poly1305::XChaCha20Poly1305, 24);
