//! The version 2 hedged encrypted envelope protocol.
//!
//! The payload is padded, encrypted with XChaCha20-Poly1305, and encrypted with
//! AES-256-GCM-SIV. A hedged key commitment binds the header to the purpose key.
//! Both AEADs authenticate the complete header and caller-associated data.
//! Every operation key comes from the shared hedged derivation protocol.

pub use crate::crypto::error::{DecryptError, EncryptError};
use crate::crypto::{
	Key32,
	algorithms::{aes256_gcm_siv, xchacha20_poly1305},
	error::AllocationFailedError,
};
use crate::secret_bytes::{SecretBytes, allocate_byte_buffer};
use crate::{
	crypto::{
		error::RandomError,
		key_derivation,
		keysets::{KeyMatch, Keyset},
		mac,
	},
	random::secure_bytes::{fill_random_bytes, fill_random_bytes_with_source},
};
use subtle::ConstantTimeEq;
use zeroize::Zeroize;

/// Maximum plaintext size, in bytes, that encryption accepts.
pub const MAX_PLAINTEXT_LEN: usize = 1 << 20;

/// Maximum associated-data size, in bytes, that encryption and decryption accept.
pub const MAX_ASSOCIATED_DATA_LEN: usize = 1 << 20;

/// Four bytes opening every envelope, identifying the format.
pub const MAGIC: &[u8; 4] = b"ENVL";
/// Format version byte following [`MAGIC`].
pub const VERSION: u8 = 2;
/// Suite identifier for XChaCha20-Poly1305, AES-256-GCM-SIV, and a hedged key commitment.
pub const SUITE_V2: u8 = 2;
/// Size, in bytes, of the per-envelope derivation salt.
pub const SALT_LEN: usize = 32;
/// Size, in bytes, of the true-length prefix inside a padded payload.
pub const TRUE_LENGTH_FIELD_LEN: usize = 8;
/// Combined authentication-tag bytes the two cascade layers add.
pub const CASCADE_TAG_OVERHEAD_LEN: usize = xchacha20_poly1305::TAG_LEN + aes256_gcm_siv::TAG_LEN;
/// Header offset, in bytes, at which the derivation salt begins.
pub const SALT_OFFSET: usize = MAGIC.len() + 2;
/// Header offset, in bytes, at which the XChaCha20-Poly1305 nonce begins.
pub const XCHACHA_NONCE_OFFSET: usize = SALT_OFFSET + SALT_LEN;
/// Header offset, in bytes, at which the AES-256-GCM-SIV nonce begins.
pub const AES_GCM_SIV_NONCE_OFFSET: usize = XCHACHA_NONCE_OFFSET + xchacha20_poly1305::NONCE_LEN;
/// Header size, in bytes, before the key commitment.
pub const BASE_HEADER_LEN: usize = AES_GCM_SIV_NONCE_OFFSET + aes256_gcm_siv::NONCE_LEN;
/// Bytes retained from each independent MAC component for key commitment.
pub const KEY_COMMITMENT_COMPONENT_LEN: usize = 32;
/// Total bytes in the two-component key commitment.
pub const KEY_COMMITMENT_LEN: usize = 2 * KEY_COMMITMENT_COMPONENT_LEN;
/// Total header size, in bytes, preceding the cascade ciphertext.
pub const HEADER_LEN: usize = BASE_HEADER_LEN + KEY_COMMITMENT_LEN;
const ENVELOPE_OVERHEAD_LEN: usize = HEADER_LEN + CASCADE_TAG_OVERHEAD_LEN;
const FIXED_OVERHEAD_LEN: usize = ENVELOPE_OVERHEAD_LEN + TRUE_LENGTH_FIELD_LEN;
/// Smallest byte length any well-formed envelope can have.
pub const MIN_ENVELOPE_LEN: usize = round_up_to_padme_len(FIXED_OVERHEAD_LEN);

/// Maximum envelope size accepted by decryption.
pub const MAX_ENVELOPE_LEN: usize = round_up_to_padme_len(FIXED_OVERHEAD_LEN + MAX_PLAINTEXT_LEN);
/// Smallest padded payload, including its true-length field.
pub const MIN_PADDED_PAYLOAD_LEN: usize = MIN_ENVELOPE_LEN - ENVELOPE_OVERHEAD_LEN;
/// Largest padded payload, including its true-length field.
pub const MAX_PADDED_PAYLOAD_LEN: usize = MAX_ENVELOPE_LEN - ENVELOPE_OVERHEAD_LEN;

/// The inputs of [`encrypt`].
#[derive(Clone, Copy)]
pub struct EncryptInput<'a> {
	/// The bytes to encrypt, at most [`MAX_PLAINTEXT_LEN`] bytes. Compute the complete
	/// output length with [`envelope_len_for_plaintext_len`], or the capacity within an
	/// output-size limit with [`max_plaintext_len_for_envelope_limit`].
	pub plaintext: &'a [u8],
	/// Bytes that the envelope authenticates but does not store, at most
	/// [`MAX_ASSOCIATED_DATA_LEN`] bytes. Decryption needs the same bytes.
	pub associated_data: &'a [u8],
}

/// The inputs of [`decrypt`].
#[derive(Clone, Copy, Debug)]
pub struct DecryptInput<'a> {
	/// The envelope that [`encrypt`] returned.
	pub envelope: &'a [u8],
	/// The associated data that encryption used.
	pub associated_data: &'a [u8],
}

/// Encrypts bytes under the latest key in `keyset` and returns the envelope.
pub fn encrypt(keyset: &Keyset, input: EncryptInput<'_>) -> Result<Vec<u8>, EncryptError> {
	encrypt_with_key_and_fill(input, keyset.latest_key(), fill_random_bytes)
}

/// Decrypts an envelope with the first matching key in `keyset`, trying each
/// key from the latest to the oldest.
///
/// Decryption succeeds only with the same associated data, and with a keyset that has the same
/// purpose and holds the key that encrypted the envelope; see [`Keyset`].
pub fn decrypt(keyset: &Keyset, input: DecryptInput<'_>) -> Result<SecretBytes, DecryptError> {
	decrypt_with_key_match(keyset, input).map(|(plaintext, _)| plaintext)
}

pub(crate) fn decrypt_with_key_match(
	keyset: &Keyset,
	input: DecryptInput<'_>,
) -> Result<(SecretBytes, KeyMatch), DecryptError> {
	let envelope = input.envelope;
	validate_envelope_size(envelope)?;
	let parsed = ParsedEnvelope::parse(envelope)?;
	validate_decrypt_associated_data_len(input.associated_data.len())?;
	let associated_data_for_layers = build_layer_associated_data(LayerAssociatedDataInput {
		header: parsed.header,
		associated_data: input.associated_data,
	})?;

	let mut decrypted = None;
	for (index, key) in keyset.latest_first_keys().iter().enumerate() {
		match decrypt_parsed_with_key(parsed, &associated_data_for_layers, key) {
			Ok(plaintext) if decrypted.is_none() => {
				decrypted = Some((plaintext, KeyMatch { index }));
			}
			Ok(_) | Err(DecryptError::DecryptionFailed) => {}
			Err(error) => return Err(error),
		}
	}

	decrypted.ok_or(DecryptError::DecryptionFailed)
}

/// Encrypts like [`encrypt`], drawing every random value (padding fill, salt,
/// and both nonces) from `random_fill_fn`.
///
/// Every successful callback invocation must overwrite the entire slice with
/// fresh CSPRNG output. Repeating deterministic output can reuse encryption
/// nonces and keys, so only test vectors use this. If the callback reports an
/// error after a partial write, this function clears that destination before
/// returning the error.
#[cfg(feature = "__internal")]
#[doc(hidden)]
pub fn encrypt_with_random_fill(
	keyset: &Keyset,
	input: EncryptInput<'_>,
	random_fill_fn: impl FnMut(&mut [u8]) -> Result<(), RandomError>,
) -> Result<Vec<u8>, EncryptError> {
	encrypt_with_key_and_fill(input, keyset.latest_key(), random_fill_fn)
}

#[cfg(feature = "__internal")]
impl Keyset {
	/// Seals raw bytes under this keyset's newest key.
	#[doc(hidden)]
	pub fn encrypt_bytes(&self, plaintext: &[u8]) -> Result<Vec<u8>, EncryptError> {
		self.encrypt_bytes_with_associated_data(plaintext, &[])
	}

	/// Seals raw bytes under this keyset's newest key, authenticating
	/// `associated_data` alongside them without sealing it.
	#[doc(hidden)]
	pub fn encrypt_bytes_with_associated_data(
		&self,
		plaintext: &[u8],
		associated_data: &[u8],
	) -> Result<Vec<u8>, EncryptError> {
		encrypt(
			self,
			EncryptInput {
				plaintext,
				associated_data,
			},
		)
	}

	/// Opens an envelope of raw bytes, trying each rotation key newest first.
	#[doc(hidden)]
	pub fn decrypt_bytes(&self, envelope: &[u8]) -> Result<SecretBytes, DecryptError> {
		self.decrypt_bytes_with_associated_data(envelope, &[])
	}

	/// Opens an envelope of raw bytes that authenticated `associated_data`.
	#[doc(hidden)]
	pub fn decrypt_bytes_with_associated_data(
		&self,
		envelope: &[u8],
		associated_data: &[u8],
	) -> Result<SecretBytes, DecryptError> {
		decrypt(
			self,
			DecryptInput {
				envelope,
				associated_data,
			},
		)
	}
}

fn encrypt_with_key_and_fill(
	input: EncryptInput<'_>,
	key: &Key32,
	mut random_fill_fn: impl FnMut(&mut [u8]) -> Result<(), RandomError>,
) -> Result<Vec<u8>, EncryptError> {
	let EncryptInput {
		plaintext,
		associated_data,
	} = input;
	let padded_payload_len = padded_payload_len_for_plaintext_len(plaintext.len()).ok_or(
		EncryptError::PlaintextTooLarge {
			actual: plaintext.len(),
			max: MAX_PLAINTEXT_LEN,
		},
	)?;

	if associated_data.len() > MAX_ASSOCIATED_DATA_LEN {
		return Err(EncryptError::AssociatedDataTooLarge {
			actual: associated_data.len(),
			max: MAX_ASSOCIATED_DATA_LEN,
		});
	}

	let mut padded_payload = SecretBytes::zeroed(padded_payload_len)?;
	fill_random_bytes_with_source(padded_payload.expose_secret_mut(), &mut random_fill_fn)?;
	padded_payload.expose_secret_mut()[..TRUE_LENGTH_FIELD_LEN]
		.copy_from_slice(&(plaintext.len() as u64).to_le_bytes());
	padded_payload.expose_secret_mut()
		[TRUE_LENGTH_FIELD_LEN..TRUE_LENGTH_FIELD_LEN + plaintext.len()]
		.copy_from_slice(plaintext);

	let mut salt = [0_u8; SALT_LEN];
	fill_random_bytes_with_source(&mut salt, &mut random_fill_fn)?;
	let mut xchacha_nonce = [0_u8; xchacha20_poly1305::NONCE_LEN];
	fill_random_bytes_with_source(&mut xchacha_nonce, &mut random_fill_fn)?;
	let mut aes_gcm_siv_nonce = [0_u8; aes256_gcm_siv::NONCE_LEN];
	fill_random_bytes_with_source(&mut aes_gcm_siv_nonce, &mut random_fill_fn)?;
	let base_header = build_base_header(&salt, &xchacha_nonce, &aes_gcm_siv_nonce);
	let commitment = key_commitment_for_base_header(key, &base_header);
	let header = build_header(&base_header, &commitment);
	let associated_data_for_layers = build_layer_associated_data(LayerAssociatedDataInput {
		header: &header,
		associated_data,
	})?;
	let keys = derive_envelope_keys(key, &salt);
	let inner_ciphertext = SecretBytes::from_vec(
		xchacha20_poly1305::encrypt(
			&keys.xchacha,
			xchacha20_poly1305::EncryptInput {
				nonce: &xchacha_nonce,
				associated_data: &associated_data_for_layers,
				plaintext: padded_payload.expose_secret(),
			},
		)
		.map_err(encrypt_error)?,
	);
	let ciphertext = aes256_gcm_siv::encrypt(
		&keys.aes_gcm_siv,
		aes256_gcm_siv::EncryptInput {
			nonce: &aes_gcm_siv_nonce,
			associated_data: &associated_data_for_layers,
			plaintext: inner_ciphertext.expose_secret(),
		},
	)
	.map_err(encrypt_error)?;
	let mut envelope = allocate_byte_buffer(header.len() + ciphertext.len())?;
	envelope.extend_from_slice(&header);
	envelope.extend_from_slice(&ciphertext);
	debug_assert!(envelope.len() <= MAX_ENVELOPE_LEN);

	Ok(envelope)
}

/// Opens an envelope with one working key directly, bypassing keyset rotation.
///
/// The caller supplies the exact purpose key that the envelope was sealed
/// under, so no rotation candidate is tried and a mismatched key fails
/// rather than falling through to an older key.
#[cfg(feature = "__internal")]
#[doc(hidden)]
pub fn decrypt_with_key_and_associated_data(
	envelope: &[u8],
	associated_data: &[u8],
	key: &Key32,
) -> Result<SecretBytes, DecryptError> {
	validate_envelope_size(envelope)?;
	let parsed = ParsedEnvelope::parse(envelope)?;
	validate_decrypt_associated_data_len(associated_data.len())?;
	let associated_data_for_layers = build_layer_associated_data(LayerAssociatedDataInput {
		header: parsed.header,
		associated_data,
	})?;

	decrypt_parsed_with_key(parsed, &associated_data_for_layers, key)
}

fn decrypt_parsed_with_key(
	parsed: ParsedEnvelope<'_>,
	associated_data_for_layers: &[u8],
	key: &Key32,
) -> Result<SecretBytes, DecryptError> {
	let expected_commitment = key_commitment_for_base_header(key, parsed.base_header);
	if !bool::from(expected_commitment.ct_eq(parsed.commitment)) {
		return Err(DecryptError::DecryptionFailed);
	}
	let keys = derive_envelope_keys(key, parsed.salt);
	let inner_ciphertext = aes256_gcm_siv::decrypt(
		&keys.aes_gcm_siv,
		aes256_gcm_siv::DecryptInput {
			nonce: parsed.aes_gcm_siv_nonce,
			associated_data: associated_data_for_layers,
			ciphertext: parsed.ciphertext,
		},
	)
	.map_err(decrypt_error)?;
	let padded_payload = xchacha20_poly1305::decrypt(
		&keys.xchacha,
		xchacha20_poly1305::DecryptInput {
			nonce: parsed.xchacha_nonce,
			associated_data: associated_data_for_layers,
			ciphertext: inner_ciphertext.expose_secret(),
		},
	)
	.map_err(decrypt_error)?;
	plaintext_from_padded_payload(&padded_payload)
}

fn encrypt_error(error: crate::crypto::AeadError) -> EncryptError {
	match error {
		crate::crypto::AeadError::AllocationFailed => EncryptError::AllocationFailed,
		crate::crypto::AeadError::MsgTooLarge
		| crate::crypto::AeadError::AuthenticationFailed => {
			unreachable!("envelope input bounds fit both encryption algorithms")
		}
	}
}

fn decrypt_error(error: crate::crypto::AeadError) -> DecryptError {
	match error {
		crate::crypto::AeadError::AllocationFailed => DecryptError::AllocationFailed,
		crate::crypto::AeadError::MsgTooLarge
		| crate::crypto::AeadError::AuthenticationFailed => DecryptError::DecryptionFailed,
	}
}

struct EnvelopeKeys {
	xchacha: Key32,
	aes_gcm_siv: Key32,
}

fn derive_envelope_keys(key: &Key32, salt: &[u8; SALT_LEN]) -> EnvelopeKeys {
	let derive = |purpose| {
		key_derivation::derive_key(
			key,
			key_derivation::Context {
				salt,
				purpose,
				fields: &[],
			},
		)
	};
	EnvelopeKeys {
		xchacha: derive(b"primitives-rs/envelope/v2/xchacha20-poly1305"),
		aes_gcm_siv: derive(b"primitives-rs/envelope/v2/aes256-gcm-siv"),
	}
}

fn key_commitment_for_base_header(
	key: &Key32,
	base_header: &[u8; BASE_HEADER_LEN],
) -> [u8; KEY_COMMITMENT_LEN] {
	let commitment_key = key_derivation::derive_key(
		key,
		key_derivation::Context {
			salt: &base_header[SALT_OFFSET..XCHACHA_NONCE_OFFSET],
			purpose: b"primitives-rs/envelope/v2/key-commitment",
			fields: &[],
		},
	);
	let tag = mac::authenticate_fields_with_key(&commitment_key, &[base_header]);
	let mut commitment = [0; KEY_COMMITMENT_LEN];
	commitment[..KEY_COMMITMENT_COMPONENT_LEN]
		.copy_from_slice(&tag.as_bytes()[..KEY_COMMITMENT_COMPONENT_LEN]);
	commitment[KEY_COMMITMENT_COMPONENT_LEN..].copy_from_slice(
		&tag.as_bytes()[mac::COMPONENT_TAG_LEN..mac::COMPONENT_TAG_LEN + KEY_COMMITMENT_COMPONENT_LEN],
	);
	commitment
}

/// Assembles the fixed header fields that the key commitment binds.
pub fn build_base_header(
	salt: &[u8; SALT_LEN],
	xchacha_nonce: &[u8; xchacha20_poly1305::NONCE_LEN],
	aes_gcm_siv_nonce: &[u8; aes256_gcm_siv::NONCE_LEN],
) -> [u8; BASE_HEADER_LEN] {
	let mut header = [0_u8; BASE_HEADER_LEN];
	header[..MAGIC.len()].copy_from_slice(MAGIC);
	header[MAGIC.len()] = VERSION;
	header[MAGIC.len() + 1] = SUITE_V2;
	header[SALT_OFFSET..XCHACHA_NONCE_OFFSET].copy_from_slice(salt);
	header[XCHACHA_NONCE_OFFSET..AES_GCM_SIV_NONCE_OFFSET].copy_from_slice(xchacha_nonce);
	header[AES_GCM_SIV_NONCE_OFFSET..BASE_HEADER_LEN].copy_from_slice(aes_gcm_siv_nonce);
	header
}

/// Appends the commitment to the fixed header fields.
pub fn build_header(
	base_header: &[u8; BASE_HEADER_LEN],
	commitment: &[u8; KEY_COMMITMENT_LEN],
) -> [u8; HEADER_LEN] {
	let mut header = [0; HEADER_LEN];
	header[..BASE_HEADER_LEN].copy_from_slice(base_header);
	header[BASE_HEADER_LEN..].copy_from_slice(commitment);
	header
}

/// The inputs of [`build_layer_associated_data`].
#[derive(Clone, Copy)]
pub struct LayerAssociatedDataInput<'a> {
	/// The complete envelope header, including the key commitment.
	pub header: &'a [u8],
	/// The caller associated data.
	pub associated_data: &'a [u8],
}

/// Concatenates the header and caller associated data into the byte string
/// both cascade layers authenticate.
///
/// The caller checks `associated_data` against [`MAX_ASSOCIATED_DATA_LEN`]
/// first.
pub fn build_layer_associated_data(
	input: LayerAssociatedDataInput<'_>,
) -> Result<Vec<u8>, AllocationFailedError> {
	let mut combined =
		allocate_byte_buffer(input.header.len() + input.associated_data.len())?;
	combined.extend_from_slice(input.header);
	combined.extend_from_slice(input.associated_data);
	Ok(combined)
}

/// Returns the exact envelope length in bytes for a plaintext length, or `None`
/// when the plaintext length exceeds [`MAX_PLAINTEXT_LEN`].
///
/// The length includes the encrypted length field, padding, public header, and
/// authentication tags. Subtract `plaintext_len` from the result to obtain the
/// complete size overhead. Associated data and the keyset purpose are not stored
/// in the envelope and do not affect its length. This calculation allocates
/// nothing and does not encrypt or validate associated data.
pub const fn envelope_len_for_plaintext_len(plaintext_len: usize) -> Option<usize> {
	if plaintext_len > MAX_PLAINTEXT_LEN {
		return None;
	}
	Some(round_up_to_padme_len(FIXED_OVERHEAD_LEN + plaintext_len))
}

/// Returns the largest plaintext length in bytes that produces a complete envelope
/// within `envelope_len_limit`, or `None` when even an empty plaintext cannot fit.
///
/// The result accounts for padding buckets and never exceeds [`MAX_PLAINTEXT_LEN`],
/// even when the limit exceeds [`MAX_ENVELOPE_LEN`]. Every plaintext length from
/// zero through the result fits; a larger permitted plaintext does not fit.
/// This calculation allocates nothing.
///
/// For nested encryption, pass the outer layer's available plaintext capacity as
/// `envelope_len_limit`. Reserve any framing or encoding around the inner envelope
/// before this calculation, and subtract any framing inside the inner plaintext
/// from the result. See the [module example](crate::crypto::encryption#size-planning).
pub fn max_plaintext_len_for_envelope_limit(envelope_len_limit: usize) -> Option<usize> {
	if envelope_len_limit < MIN_ENVELOPE_LEN {
		return None;
	}
	let bounded_limit = envelope_len_limit.min(MAX_ENVELOPE_LEN);
	let multiple = padme_padding_multiple(bounded_limit);
	let envelope_len = bounded_limit & !(multiple - 1);
	Some((envelope_len - FIXED_OVERHEAD_LEN).min(MAX_PLAINTEXT_LEN))
}

// Inputs are bounded complete-envelope lengths, all greater than one byte.
const fn padme_padding_multiple(envelope_len: usize) -> usize {
	let exponent = envelope_len.ilog2();
	let precision = exponent.ilog2() + 1;
	1 << (exponent - precision)
}

const fn round_up_to_padme_len(unpadded_envelope_len: usize) -> usize {
	let multiple = padme_padding_multiple(unpadded_envelope_len);
	(unpadded_envelope_len + multiple - 1) & !(multiple - 1)
}

/// Returns the padding-bucket size a plaintext of the given length occupies,
/// or `None` when `plaintext_len` exceeds [`MAX_PLAINTEXT_LEN`].
pub fn padded_payload_len_for_plaintext_len(plaintext_len: usize) -> Option<usize> {
	envelope_len_for_plaintext_len(plaintext_len)
		.map(|envelope_len| envelope_len - ENVELOPE_OVERHEAD_LEN)
}

pub(crate) fn is_valid_padded_payload_len(len: usize) -> bool {
	if !(MIN_PADDED_PAYLOAD_LEN..=MAX_PADDED_PAYLOAD_LEN).contains(&len) {
		return false;
	}
	let envelope_len = len + ENVELOPE_OVERHEAD_LEN;
	round_up_to_padme_len(envelope_len) == envelope_len
}

fn plaintext_from_padded_payload(
	padded_payload: &SecretBytes,
) -> Result<SecretBytes, DecryptError> {
	if !is_valid_padded_payload_len(padded_payload.len()) {
		return Err(DecryptError::DecryptionFailed);
	}
	let mut length_bytes = [0_u8; TRUE_LENGTH_FIELD_LEN];
	length_bytes.copy_from_slice(&padded_payload.expose_secret()[..TRUE_LENGTH_FIELD_LEN]);
	let plaintext_len_u64 = u64::from_le_bytes(length_bytes);
	length_bytes.zeroize();
	let plaintext_len =
		usize::try_from(plaintext_len_u64).map_err(|_| DecryptError::DecryptionFailed)?;
	if plaintext_len > MAX_PLAINTEXT_LEN {
		return Err(DecryptError::DecryptionFailed);
	}
	let canonical_padded_len = padded_payload_len_for_plaintext_len(plaintext_len)
		.ok_or(DecryptError::DecryptionFailed)?;
	if canonical_padded_len != padded_payload.len() {
		return Err(DecryptError::DecryptionFailed);
	}

	let plaintext_start = TRUE_LENGTH_FIELD_LEN;
	let plaintext_end = plaintext_start + plaintext_len;
	Ok(SecretBytes::from_slice(
		&padded_payload.expose_secret()[plaintext_start..plaintext_end],
	)?)
}

fn validate_decrypt_associated_data_len(len: usize) -> Result<(), DecryptError> {
	if len > MAX_ASSOCIATED_DATA_LEN {
		return Err(DecryptError::AssociatedDataTooLarge {
			actual: len,
			max: MAX_ASSOCIATED_DATA_LEN,
		});
	}
	Ok(())
}

fn validate_envelope_size(envelope: &[u8]) -> Result<(), DecryptError> {
	if envelope.is_empty() {
		return Err(DecryptError::EmptyEnvelope);
	}
	if envelope.len() > MAX_ENVELOPE_LEN {
		return Err(DecryptError::EnvelopeTooLarge {
			actual: envelope.len(),
			max: MAX_ENVELOPE_LEN,
		});
	}
	Ok(())
}

#[derive(Clone, Copy)]
struct ParsedEnvelope<'a> {
	header: &'a [u8],
	base_header: &'a [u8; BASE_HEADER_LEN],
	salt: &'a [u8; SALT_LEN],
	xchacha_nonce: &'a [u8; xchacha20_poly1305::NONCE_LEN],
	aes_gcm_siv_nonce: &'a [u8; aes256_gcm_siv::NONCE_LEN],
	ciphertext: &'a [u8],
	commitment: &'a [u8; KEY_COMMITMENT_LEN],
}

impl<'a> ParsedEnvelope<'a> {
	fn parse(envelope: &'a [u8]) -> Result<Self, DecryptError> {
		if envelope.len() < MIN_ENVELOPE_LEN {
			return Err(DecryptError::EnvelopeTooShort {
				actual: envelope.len(),
				min: MIN_ENVELOPE_LEN,
			});
		}
		if &envelope[..MAGIC.len()] != MAGIC {
			return Err(DecryptError::InvalidMagic);
		}
		let version = envelope[MAGIC.len()];
		if version != VERSION {
			return Err(DecryptError::UnsupportedVersion { version });
		}
		let suite = envelope[MAGIC.len() + 1];
		if suite != SUITE_V2 {
			return Err(DecryptError::UnsupportedSuite { suite });
		}

		let ciphertext_len = envelope.len() - HEADER_LEN;
		let padded_payload_len = ciphertext_len - CASCADE_TAG_OVERHEAD_LEN;
		if !is_valid_padded_payload_len(padded_payload_len) {
			return Err(DecryptError::InvalidEnvelopeLength {
				actual: envelope.len(),
			});
		}

		Ok(Self {
			header: &envelope[..HEADER_LEN],
			base_header: envelope[..BASE_HEADER_LEN]
				.try_into()
				.expect("the parsed base header has a fixed size"),
			salt: envelope[SALT_OFFSET..XCHACHA_NONCE_OFFSET]
				.try_into()
				.expect("the parsed salt has a fixed size"),
			xchacha_nonce: envelope[XCHACHA_NONCE_OFFSET..AES_GCM_SIV_NONCE_OFFSET]
				.try_into()
				.map_err(|_| DecryptError::InvalidEnvelopeLength {
					actual: envelope.len(),
				})?,
			aes_gcm_siv_nonce: envelope[AES_GCM_SIV_NONCE_OFFSET..BASE_HEADER_LEN]
				.try_into()
				.map_err(|_| DecryptError::InvalidEnvelopeLength {
					actual: envelope.len(),
				})?,
			ciphertext: &envelope[HEADER_LEN..],
			commitment: envelope[BASE_HEADER_LEN..HEADER_LEN]
				.try_into()
				.expect("the parsed commitment has a fixed size"),
		})
	}
}

pub(crate) fn validate_envelope_structure(envelope: &[u8]) -> Result<(), DecryptError> {
	validate_envelope_size(envelope)?;
	ParsedEnvelope::parse(envelope).map(|_| ())
}
