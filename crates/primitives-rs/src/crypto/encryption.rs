//! Hedged encryption with authenticated, padded envelopes.
//!
//! Encryption derives independent working keys from one purpose-bound keyset.
//! Decryption checks a key commitment, then verifies both AEAD layers before
//! returning plaintext. Both layers authenticate the complete header and caller data.
//! The exact format and security assumptions are specified in [`super`].
//!
//! # Size planning
//!
//! [`envelope_len_for_plaintext_len`] includes all padding and envelope overhead.
//! [`max_plaintext_len_for_envelope_limit`] finds the largest plaintext that fits
//! within a complete output-size limit. Both functions count bytes and allocate
//! nothing. A valid envelope can exceed [`MAX_PLAINTEXT_LEN`], so it is not
//! necessarily a valid plaintext for another encryption operation.
//!
//! For two nested envelopes, the inner envelope must fit in the outer layer's
//! plaintext capacity. This example reserves space for application framing
//! around the inner envelope, then finds the maximum inner plaintext:
//!
//! ```
//! use primitives_rs::crypto::encryption::{
//!     MAX_PLAINTEXT_LEN, envelope_len_for_plaintext_len,
//!     max_plaintext_len_for_envelope_limit,
//! };
//!
//! let outer_framing_len = 32;
//! let inner_envelope_limit = MAX_PLAINTEXT_LEN.checked_sub(outer_framing_len).unwrap();
//! let inner_plaintext_limit = max_plaintext_len_for_envelope_limit(inner_envelope_limit).unwrap();
//! let inner_envelope_len = envelope_len_for_plaintext_len(inner_plaintext_limit).unwrap();
//! let outer_plaintext_len = outer_framing_len + inner_envelope_len;
//! let outer_envelope_len = envelope_len_for_plaintext_len(outer_plaintext_len).unwrap();
//! assert_eq!(inner_plaintext_limit, 1_032_014);
//! assert_eq!(inner_envelope_len, 1_032_192);
//! assert_eq!(outer_envelope_len, 1_048_576);
//! ```
//!
//! Application framing inside the inner plaintext consumes part of
//! `inner_plaintext_limit`. Text encoding between layers also consumes the outer
//! plaintext capacity; these functions describe binary envelopes only. Associated
//! data is authenticated separately and does not consume either plaintext capacity.
//! An additional outer output-size limit can first be converted to a plaintext
//! capacity with [`max_plaintext_len_for_envelope_limit`].
internal_modules!(crate; envelope);
mod reencryption;
pub use crate::crypto::error::{DecryptError, EncryptError};
#[cfg(feature = "__internal")]
#[doc(hidden)]
pub use envelope::encrypt_with_random_fill;
pub use envelope::{
	DecryptInput, EncryptInput, MAX_ASSOCIATED_DATA_LEN, MAX_ENVELOPE_LEN, MAX_PLAINTEXT_LEN,
	MIN_ENVELOPE_LEN, decrypt, encrypt, envelope_len_for_plaintext_len,
	max_plaintext_len_for_envelope_limit,
};
pub(crate) use envelope::{decrypt_with_key_match, validate_envelope_structure};
pub use reencryption::{ReencryptError, ReencryptResult, reencrypt};
