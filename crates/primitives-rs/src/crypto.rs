//! Hedged cryptographic protocols, purpose-bound keysets, and named algorithms.
//!
//! [`EncryptedValue`](crate::crypto::encrypted_value::EncryptedValue) adds parsed
//! ownership, payload markers, and rotation results over the same envelope. The
//! `crypto-json` feature adds explicit JSON operations with bounded, zeroizing
//! serialization. Applications
//! bind schema versions and record identities through purpose and associated data.
//! [`SecretVerifier`](crate::crypto::secret_verifier::SecretVerifier) provides
//! versioned, purpose-bound verification of secret values through the shared
//! hedged derivation protocol.
//! [`keyed_content_address`](crate::crypto::keyed_content_address) provides stable, secret-keyed content identifiers
//! with a combined KDF output and an addressing key independent of encryption
//! rotation.
//!
//! The `password-derivation` feature adds an algorithm-specific Argon2id API,
//! password-derived roots with explicit resource limits, and password-sealed keys
//! through the shared envelope. Argon2id supplies the password-stretching cost;
//! the subsequent hedged KDF does not preserve that cost if Argon2id fails.
//!
//! The `signatures` feature adds the owned, versioned `signature` protocol,
//! which requires both SLH-DSA-SHA2-256f and ML-DSA-87. It also exposes named
//! algorithm modules with FIPS encodings. The independent `ed25519` feature
//! supports RFC 8032 interoperability with strict verification.
//!
//! Encryption is a byte protocol: [`encrypt`](crate::crypto::encryption::encrypt) turns plaintext bytes and
//! associated data into the bytes of an ENVL envelope, and [`decrypt`](crate::crypto::encryption::decrypt) reverses
//! it. How a value becomes bytes is the caller's choice. A [`Keyset`](crate::crypto::keysets::Keyset) binds its
//! keys to a purpose string, which separates the domains of different uses of
//! the same keys. Randomness comes from the platform's secure random source.
//! [`crate::random::secure_bytes`] and [`crate::secret_bytes`] also provide entropy and
//! secret buffers through features that do not enable encryption.
//!
//! Browser WASM consumers must enable `getrandom` 0.3's `wasm_js` feature in
//! their application manifest. That backend uses Web Crypto; this crate does not
//! select a JavaScript backend for native consumers.
//!
//! # Example
//!
//! ```rust
//! use primitives_rs::crypto::{encryption::{DecryptInput, EncryptInput, decrypt, encrypt}, Key32, keysets::derive_keyset_from_latest_first_keys};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let current_key: Key32 = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8".parse()?;
//! let keyset = derive_keyset_from_latest_first_keys([current_key], "my-app.sessions.v1")?;
//!
//! let envelope = encrypt(
//!     &keyset,
//!     EncryptInput {
//!         plaintext: b"user-id=u123",
//!         associated_data: b"session-cookie",
//!     },
//! )?;
//! let plaintext = decrypt(
//!     &keyset,
//!     DecryptInput {
//!         envelope: &envelope,
//!         associated_data: b"session-cookie",
//!     },
//! )?;
//!
//! assert_eq!(plaintext.expose_secret(), b"user-id=u123");
//! # Ok(())
//! # }
//! ```

//! # Shared keys for separate application purposes
//!
//! A keyset can derive child keysets for encryption and message authentication.
//! Both operations use the same hedged key schedule and rotation order.
//!
//! ```rust
//! use primitives_rs::crypto::{Key32, keysets::derive_keyset_from_latest_first_keys, hash, mac};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let master_key = Key32::generate()?;
//! let application_keys = derive_keyset_from_latest_first_keys([master_key], "example.app.v1")?;
//! let message_keys = application_keys.derive_child_keyset("messages")?;
//! let tag = mac::authenticate_fields(&message_keys, &[b"account-42", b"public message"]);
//! mac::verify_fields(&message_keys, &[b"account-42", b"public message"], &tag)?;
//! let digest = hash::hash(b"public message");
//! assert_eq!(digest.as_bytes().len(), hash::DIGEST_LEN);
//! # Ok(())
//! # }
//! ```

//! # Cryptographic family protocols
//!
//! The owned protocols use one high-entropy 32-byte master secret. Applications
//! choose purpose labels and retain their rotation keys. Primitives derives the
//! working keys and separates the algorithm roles. Password stretching is a
//! separate operation; these fast derivations cannot protect a weak password.
//!
//! ## Common framing
//!
//! All integers in the common framing use unsigned 64-bit big-endian encoding.
//! For a byte string `domain` and an ordered list of byte strings `fields`, define:
//!
//! ```text
//! frame(domain, fields) =
//!     u64(len(domain)) || domain || u64(number_of_fields) ||
//!     for each field: u64(len(field)) || field
//! ```
//!
//! Empty fields, omitted fields, field order, and field boundaries are distinct.
//! Protocol labels below are literal UTF-8 bytes. Algorithm-specific APIs implement
//! the named standards without adding this framing.
//!
//! ## High-entropy key derivation
//!
//! For a 32-byte secret `K`, public `salt`, purpose bytes `purpose`, ordered
//! context fields `fields`, and requested byte length `L`, define:
//!
//! ```text
//! D = "primitives-rs/key-derivation/v1"
//! T = frame(D, [salt, purpose, u64(L)]) || frame("fields", fields)
//! A = HKDF-SHA512(ikm=K, salt=D, info=SHA512(T), length=L)
//! B = KMAC256(key=K, message=SHA3-512(T), customization=D, length=L)
//! derive(K, salt, purpose, fields, L) = A XOR B
//! ```
//!
//! The output limit is 16,320 bytes, which is HKDF-SHA512's limit. The output length
//! is authenticated context, so shorter outputs are not prefixes of longer ones.
//! The component outputs remain internal. `derive_key` selects `L=32` and returns
//! `Key32`; `derive_bytes` returns a zeroizing buffer.
//!
//! The security assumption is joint pseudorandomness: at least one branch must
//! remain pseudorandom even given the other branch's output under the same secret
//! and context. Under that assumption, XOR preserves pseudorandomness because XOR
//! with a known independent value preserves a uniform distribution. SHA-2 and
//! Keccak have different designs, which motivates their use as separate branches.
//! Ordinary standalone PRF security does not prove this shared-secret assumption.
//! In particular, the claim excludes a replacement component that deliberately
//! queries and cancels the surviving component. Domain separation prevents
//! cross-purpose reuse; it does not prove independence or cryptanalytic resilience.
//!
//! This construction provides a combined output for secret-token verification,
//! opaque keyed lookups, child keys, and algorithm working keys. A protocol must
//! bind the output's role and relevant context. Exposing one output must not expose
//! the master secret under the surviving branch's security assumption.
//!
//! ## Purpose-bound keysets
//!
//! A keyset accepts one to 16 distinct master keys, newest first. Each master key
//! produces a 32-byte purpose key:
//!
//! ```text
//! derive(master, "", "primitives-rs/keyset/v1/purpose", [purpose], 32)
//! ```
//!
//! A child keyset derives from each corresponding parent purpose key:
//!
//! ```text
//! derive(parent_key, "", "primitives-rs/keyset/v1/child",
//!        [parent_purpose, child_purpose], 32)
//! ```
//!
//! Purpose segments contain one to 255 visible ASCII bytes other than `/`.
//! The displayed child purpose joins segments with `/` and must fit 255 bytes.
//! The keyset retains purpose keys, not the input master keys. Encryption and MAC
//! creation use the newest key. Decryption and MAC verification accept retained
//! older keys. Removing an old key makes its encrypted values inaccessible and its
//! authentication tags unverifiable.
//!
//! ## Hashing
//!
//! The byte-stream hash computes:
//!
//! ```text
//! T = frame("primitives-rs/hash/v1/bytes", []) || message
//! SHA512(T) || SHA3-512(T)
//! ```
//!
//! The structured hash computes the same concatenation over:
//!
//! ```text
//! frame("primitives-rs/hash/v1/fields", []) || frame(purpose, fields)
//! ```
//!
//! The 128-byte result preserves collision and second-preimage resistance if
//! either component retains that property: a collision for the complete result
//! must also collide in the surviving component. Neither digest is truncated.
//! This is not a general preimage-resistance hedge for secret inputs because a
//! failed component could reveal its input. Secret-token verifiers should use
//! hedged derivation, which exposes only the combined branch output.
//!
//! ## Message authentication
//!
//! For a purpose key `K`, derive two 32-byte working keys:
//!
//! ```text
//! KH = derive(K, "", "primitives-rs/mac/v1/hmac-sha512", [], 32)
//! KK = derive(K, "", "primitives-rs/mac/v1/kmac256", [], 32)
//! ```
//!
//! Structured authentication uses the transcript:
//!
//! ```text
//! T = frame("primitives-rs/mac/v1/fields", []) || frame("fields", fields)
//! ```
//!
//! Incremental byte-stream authentication uses:
//!
//! ```text
//! T = frame("primitives-rs/mac/v1/bytes", []) || message
//! ```
//!
//! Both forms return the complete 128-byte tag:
//!
//! ```text
//! HMAC-SHA512(KH, T) ||
//! KMAC256(KK, T, customization="primitives-rs/mac/v1", length=64)
//! ```
//!
//! Verification requires both components to match under the same retained rotation
//! key. It compares all tag bytes in constant time and evaluates every rotation
//! key. A tag assembled from components under different rotation keys fails.
//! Streaming verification retains keyed algorithm states rather than message
//! bytes. The structured and byte-stream protocols have different domains.
//!
//! Authenticity requires that at least one MAC remain unforgeable and that the
//! shared derivation preserve the surviving MAC's working-key secrecy. The tag
//! size does not imply 1,024-bit security. Tags authenticate messages; they do not
//! promise message confidentiality. Secret code verifiers and opaque lookups use
//! the combined derivation output when exposure of a failed component's output
//! would permit guessing the secret input.
//!
//! ## Algorithm implementations and secret memory
//!
//! RustCrypto implements SHA-2, SHA-3, HMAC, cSHAKE, and both AEAD algorithms.
//! Primitives implements RFC 5869 HKDF extraction and expansion over RustCrypto's
//! HMAC, with zeroizing extracted keys and expansion blocks. Primitives also
//! implements the SP 800-185 KMAC framing over RustCrypto's cSHAKE256;
//! it does not implement Keccak's permutation. For a 32-byte key, KMAC's initial
//! `bytepad(encode_string(K), 136)` is the five bytes `01 88 02 01 00`, followed
//! by the key and 99 zero bytes. KMAC appends `right_encode(8*L)` before squeezing.
//! The BLAKE3 dependency implements BLAKE3's algorithms.
//!
//! Key storage, secret byte buffers, Primitives-owned derivation buffers, keyed BLAKE3 states,
//! and enabled RustCrypto keyed states clear their owned storage on drop. This
//! does not erase every compiler-generated copy or dependency temporary. For
//! example, RustCrypto HMAC does not explicitly clear every temporary key-pad
//! buffer or inner digest. Recovering such remnants requires access to process
//! or WASM linear memory; zeroization is not protection against that access.
//! Applications must protect any secret copies that they explicitly make.
//! Algorithm APIs and owned protocols use the same implementation on native and
//! WASM targets. Applications select the platform entropy backend.
//!
//! The optional `serde` feature serializes digests and MAC tags as fixed-length
//! byte tuples and rejects other lengths. Keys require explicit secret exposure;
//! they do not implement ordinary serialization.
//!
//! ## References
//!
//! - [RFC 5869](https://www.rfc-editor.org/rfc/rfc5869.html) specifies HKDF.
//! - [NIST SP 800-185](https://doi.org/10.6028/NIST.SP.800-185) specifies KMAC.
//! - [Hash-combiner analysis](https://eprint.iacr.org/2012/354) distinguishes hash
//!   security properties and output-length requirements.
//! - [Robust-combiner analysis](https://eprint.iacr.org/2002/135) distinguishes
//!   cascade confidentiality from chosen-ciphertext security.
//! # Encrypted envelope version 2
//!
//! An envelope encrypts at most 1,048,576 plaintext bytes and authenticates at most
//! 1,048,576 caller-associated-data bytes. It does not store associated data or the
//! keyset purpose. Decryption requires both values. The size-planning functions in
//! [`encryption`](crate::crypto::encryption) calculate complete envelope sizes and plaintext capacity, including
//! nested framing.
//!
//! ## Header and working keys
//!
//! The first 138 bytes form the public header:
//!
//! | Offset | Size | Field |
//! | --- | ---: | --- |
//! | 0 | 4 | The magic bytes `ENVL`. |
//! | 4 | 1 | The version byte `2`. |
//! | 5 | 1 | The suite byte `2`. |
//! | 6 | 32 | A fresh random derivation salt. |
//! | 38 | 24 | A fresh XChaCha20-Poly1305 nonce. |
//! | 62 | 12 | A fresh AES-256-GCM-SIV nonce. |
//! | 74 | 32 | The HMAC component of the key commitment. |
//! | 106 | 32 | The KMAC component of the key commitment. |
//!
//! The first 74 bytes are the base header. The shared hedged KDF derives separate
//! 32-byte working keys from the purpose key, using the header salt, no context
//! fields, and these labels:
//!
//! - `primitives-rs/envelope/v2/xchacha20-poly1305`
//! - `primitives-rs/envelope/v2/aes256-gcm-siv`
//! - `primitives-rs/envelope/v2/key-commitment`
//!
//! The commitment key is the input key of the shared structured MAC over the single
//! field `[base_header]`. That MAC derives its own separate component keys. The
//! commitment contains bytes `0..32` and `64..96` of its 128-byte result, in that
//! order. Both components must match. KMAC computes its standard 64-byte output
//! before this truncation; requesting 32 output bytes would produce a different
//! value. The fresh salt prevents a stable public commitment across envelopes.
//!
//! ## Encryption and size rounding
//!
//! The plaintext becomes:
//!
//! ```text
//! little_endian_u64(plaintext_length) || plaintext || random_padding
//! ```
//!
//! Both AEADs authenticate `complete_header || caller_associated_data`.
//! XChaCha20-Poly1305 encrypts the padded payload. AES-256-GCM-SIV encrypts the
//! complete inner ciphertext, including its tag. Each AEAD adds its complete
//! 16-byte tag. The encoded envelope is `complete_header || outer_ciphertext`.
//!
//! The fixed overhead relative to plaintext is 178 bytes: 138 header bytes,
//! 32 AEAD tag bytes, and eight encrypted length bytes. Padmé rounds the complete
//! unpadded envelope to a size-dependent multiple:
//!
//! ```text
//! L = plaintext_length + 178
//! E = floor(log2(L))
//! S = floor(log2(E)) + 1
//! multiple = 2^(E - S)
//! envelope_length = ceil(L / multiple) * multiple
//! padding_length = envelope_length - L
//! ```
//!
//! The minimum envelope is 192 bytes, a 32-byte plaintext produces 224 bytes, and
//! the maximum envelope is 1,081,344 bytes. Encryption draws random bytes for the
//! padded payload, overwrites its length and plaintext, and then draws the salt,
//! XChaCha nonce, and AES nonce in that order.
//!
//! Padding is less than 8% of the unrounded complete envelope, less than 6.25%
//! when that size is at least 256 bytes, and less than 3.125% when it is at least
//! 64 KiB. These percentages exclude the fixed overhead as a cost relative to
//! plaintext. Padding obscures exact length within a bucket; known candidate
//! contents in different buckets remain distinguishable. It does not conceal
//! chunk counts or access patterns. The rule follows section 4 of
//! [the Padmé paper](https://www.petsymposium.org/2019/files/papers/issue4/popets-2019-0056.pdf).
//!
//! ## Decryption
//!
//! Decryption checks input bounds, magic, version, suite, and allowed rounded size
//! before deriving keys. For each retained rotation key, it checks both commitment
//! components before deriving the AEAD keys. It then verifies and decrypts the AES
//! layer and the XChaCha layer. The decrypted true length must imply exactly the
//! observed envelope size. No plaintext is returned before all checks succeed.
//!
//! A wrong key, purpose, associated data, commitment, AEAD tag, or authenticated
//! true length produces the same decryption failure. Public structural errors and
//! allocation failures remain distinct. Every retained rotation key is tried;
//! the first successfully decrypted plaintext is retained. Intermediate plaintext
//! buffers clear their contents on drop.
//!
//! The commitment authenticates neither ciphertext bytes nor caller-associated
//! data. A valid copied header with altered ciphertext still requires an AEAD
//! attempt. AES-GCM-SIV decrypts internally before verifying its tag; no caller
//! receives that unauthenticated intermediate value.
//!
//! ## Security assumptions
//!
//! The cascade preserves the surviving AEAD's confidentiality and authenticity
//! under separately derived secret keys, the specified nonce discipline, and
//! correct fixed implementations. AES-GCM-SIV's decryption must accept only the
//! ciphertext that its encryption operation would produce under the same key,
//! nonce, associated data, and recovered inner ciphertext. Its complete tag check
//! establishes that unique encoding. Both layers bind the complete header,
//! including both nonces, so changing the outer nonce also changes the inner
//! associated data. These conditions prevent re-encoding an accepted inner
//! ciphertext to bypass its authentication. Section 3 of
//! [Combiners for AEAD](https://d-nb.info/1220864994/34) explains this condition;
//! substituting an algorithm or implementation requires reassessing it.
//!
//! The two AEAD tags do not establish 256-bit authentication security after one
//! scheme fails. The surviving scheme's bounds apply. Authentic replays remain
//! authentic; consuming protocols assign replay semantics.
//!
//! Key commitment is a separate property: an attacker must not construct one
//! envelope that accepts different purpose keys. Its target is 128-bit classical
//! collision resistance from either surviving 256-bit commitment component. The
//! complete map from a purpose key through derivation and that component must
//! remain collision resistant. MAC unforgeability and KDF pseudorandomness alone
//! do not establish this additional assumption. Matching both components requires
//! a collision in the surviving complete map. The shared 32-byte key hierarchy
//! also limits the binding claim; longer encoded tags do not remove that limit.
//! [Key-commitment constructions](https://www.usenix.org/system/files/sec22-albertini.pdf)
//! explain why ordinary AEAD authenticity alone does not establish key binding.
//!
//! The shared KDF must retain the stated secrecy and separation of working keys.
//! A failed MAC's exposed working key must not reveal a purpose or encryption key.
//! The protocol does not claim protection against compromised implementations,
//! side channels, disclosed master keys, or simultaneous failure of all components
//! that protect a property.

#[macro_use]
mod algorithm_macros;
#[macro_use]
mod aead;
pub mod algorithms;
pub mod encrypted_value;
mod error;
mod framing;
pub mod hash;
pub mod key_derivation;
pub mod keyed_content_address;
pub mod mac;
#[cfg(feature = "password-derivation")]
pub mod password_derivation;
#[cfg(feature = "password-derivation")]
pub mod password_encrypted_key;
#[cfg(feature = "password-derivation")]
pub mod password_encryption;
#[cfg(feature = "password-derivation")]
mod password_record;
pub mod proof_of_work;
pub mod secret_verifier;
#[cfg(any(feature = "signatures", feature = "ed25519"))]
mod signature_support;
#[cfg(feature = "totp")]
pub mod totp;
#[cfg(any(feature = "signatures", feature = "ed25519"))]
pub use signature_support::{SignatureError, SignatureMsg};
pub mod encryption;
pub mod keysets;
#[cfg(feature = "signatures")]
mod ml_dsa_impl;
#[cfg(feature = "signatures")]
pub mod signature;

use std::fmt;
use std::str::FromStr;

use crate::encoding::binary_to_text::ByteEncoding;
use secrecy::{ExposeSecret, ExposeSecretMut, SecretBox, SecretString};
use subtle::ConstantTimeEq;
use zeroize::Zeroize;

use crate::random::secure_bytes::fill_random_bytes;

#[cfg(feature = "__internal")]
#[doc(hidden)]
pub use error::AllocationFailedError;
pub use error::{AeadError, Key32Error, OutputLengthError, RandomError, VerificationError};
/// Size, in bytes, of a `Key32`.
pub const KEY32_LEN: usize = 32;

/// Length, in bytes, of a `Key32`'s text form.
pub const KEY32_TEXT_LEN: usize = 43;

/// A 32-byte secret key.
///
/// Its one text form is the canonical unpadded URL-safe Base64 encoding of its
/// 32 bytes (43 characters), which [`FromStr`] parses and
/// [`Key32::expose_secret_as_base64url_text`] prints. Parsing rejects text of
/// another length and text that is not the canonical encoding.
///
/// The bytes and the text that [`Key32::expose_secret`] and
/// [`Key32::expose_secret_as_base64url_text`] return are secrets that the application owns
/// once it has them. The application must store and transmit them as secrets. The `Debug`
/// output of a key is redacted, and a key clears its own memory when it is dropped, but a key
/// cannot clear a copy that the application takes from an exposed value.
pub struct Key32(SecretBox<[u8; KEY32_LEN]>);

impl Key32 {
	/// Wraps exactly 32 bytes as a key.
	///
	/// The key clears its own storage on drop. Because arrays are copied by value,
	/// the caller must clear any retained copy of the input.
	pub fn from_array(mut bytes: [u8; KEY32_LEN]) -> Self {
		let mut key = Self::zeroed();
		key.as_mut_bytes().copy_from_slice(&bytes);
		bytes.zeroize();
		key
	}

	/// Generates a new key from the platform's secure random source.
	pub fn generate() -> Result<Self, RandomError> {
		let mut key = Self::zeroed();
		fill_random_bytes(key.as_mut_bytes())?;
		Ok(key)
	}

	/// Explicitly exposes this key as secret bytes.
	pub fn expose_secret(&self) -> &[u8; KEY32_LEN] {
		self.0.expose_secret()
	}

	/// Explicitly exposes this key as its canonical unpadded URL-safe Base64
	/// text.
	pub fn expose_secret_as_base64url_text(&self) -> SecretString {
		ByteEncoding::Base64UrlUnpadded
			.encode(self.as_bytes())
			.into()
	}

	/// Returns this key as raw bytes.
	pub(crate) fn as_bytes(&self) -> &[u8; KEY32_LEN] {
		self.0.expose_secret()
	}

	pub(crate) fn zeroed() -> Self {
		Self(SecretBox::default())
	}

	pub(crate) fn as_mut_bytes(&mut self) -> &mut [u8; KEY32_LEN] {
		self.0.expose_secret_mut()
	}
}

impl FromStr for Key32 {
	type Err = Key32Error;

	fn from_str(text: &str) -> Result<Self, Self::Err> {
		if text.len() != KEY32_TEXT_LEN {
			return Err(Key32Error::InvalidLength { actual: text.len() });
		}
		let mut key = Self::zeroed();
		match ByteEncoding::Base64UrlUnpadded.decode_into(text, key.as_mut_bytes()) {
			Ok(KEY32_LEN) => Ok(key),
			Ok(_) | Err(_) => Err(Key32Error::InvalidEncoding),
		}
	}
}

impl PartialEq for Key32 {
	fn eq(&self, other: &Self) -> bool {
		self.as_bytes().ct_eq(other.as_bytes()).into()
	}
}

impl Eq for Key32 {}

impl fmt::Debug for Key32 {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("Key32([redacted; 32])")
	}
}
