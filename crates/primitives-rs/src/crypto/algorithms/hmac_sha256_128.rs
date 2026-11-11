//! HMAC-SHA256-128 with fixed 16-byte authentication tags.
//!
//! The tag is the first 16 bytes of HMAC-SHA256, as specified in
//! [RFC 4868, section 2.3](https://www.rfc-editor.org/rfc/rfc4868.html#section-2.3).
//! The `128` suffix identifies the tag length in bits. Key processing is ordinary
//! HMAC-SHA256; shortening the tag does not shorten the key or reduce hashing work.
//! Use a securely generated 32-byte key and fix the tag length in the protocol.
//! Verification accepts exactly 16 bytes and compares all 16 bytes in constant time.
//! [`super::hmac_sha256`] provides complete 32-byte tags.
//!
//! For ordinary authentication with a secret key, a guessed tag succeeds with
//! probability approximately `2^-128` per attempt. The tag does not provide
//! 128-bit collision resistance and is not a substitute for a key commitment.
//! The raw [`Authenticator`] and [`authenticate_parts`] concatenate inputs without
//! framing; callers must encode field boundaries and bind the protocol context.
//!
//! ```
//! use primitives_rs::crypto::{Key32, algorithms::hmac_sha256_128};
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let key = Key32::generate()?;
//! let tag = hmac_sha256_128::authenticate_parts(key.expose_secret(), &[b"message"]);
//! let mut verifier = hmac_sha256_128::Authenticator::new(key.expose_secret());
//! verifier.update(b"mes");
//! verifier.update(b"sage");
//! verifier.verify(&tag)?;
//! # Ok(())
//! # }
//! ```
//!
//! Shorter prefixes cannot be passed to the verifier:
//! ```compile_fail
//! use primitives_rs::crypto::algorithms::hmac_sha256_128;
//! hmac_sha256_128::Authenticator::new(&[0; 32]).verify(&[0; 15]);
//! ```
//!
//! Full 32-byte tags cannot be passed to the 16-byte verifier:
//! ```compile_fail
//! use primitives_rs::crypto::algorithms::hmac_sha256_128;
//! hmac_sha256_128::Authenticator::new(&[0; 32]).verify(&[0; 32]);
//! ```

hmac_algorithm!(::sha2::Sha256, 16);
