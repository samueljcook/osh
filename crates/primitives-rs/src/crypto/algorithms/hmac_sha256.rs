//! HMAC-SHA256 with complete 32-byte tags.
//!
//! [`super::hmac_sha256_128`] provides the explicit 16-byte authentication-tag variant.
hmac_algorithm!(::sha2::Sha256, 32);
