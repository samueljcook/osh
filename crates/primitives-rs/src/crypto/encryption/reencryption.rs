use super::{
	DecryptError, DecryptInput, EncryptError, EncryptInput, decrypt_with_key_match, encrypt,
};
use crate::crypto::keysets::Keyset;

/// The authenticated envelope's rotation result.
#[derive(Debug)]
pub enum ReencryptResult {
	/// The envelope authenticated under the newest key, so no replacement is necessary.
	Unchanged,
	/// A replacement envelope preserves the plaintext and associated data under the newest key.
	Reencrypted(Vec<u8>),
}

/// Failure to authenticate an existing envelope or encrypt its replacement.
#[derive(Debug)]
pub enum ReencryptError {
	/// The existing envelope could not be authenticated and decrypted.
	Decryption(DecryptError),
	/// The replacement envelope could not be encrypted.
	Encryption(EncryptError),
}

impl std::fmt::Display for ReencryptError {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::Decryption(error) => write!(formatter, "envelope decryption failed: {error}"),
			Self::Encryption(error) => write!(formatter, "envelope encryption failed: {error}"),
		}
	}
}

impl std::error::Error for ReencryptError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		Some(match self {
			Self::Decryption(error) => error,
			Self::Encryption(error) => error,
		})
	}
}

/// Authenticates an envelope and replaces it when it uses an older key in the keyset.
///
/// Both outcomes require successful authentication. A replacement uses fresh randomness and
/// preserves the exact plaintext and associated data. Callers must persist replacements atomically
/// with a check that the stored value has not changed; this function performs no storage operations.
pub fn reencrypt(
	keyset: &Keyset,
	input: DecryptInput<'_>,
) -> Result<ReencryptResult, ReencryptError> {
	let (plaintext, matched_key) =
		decrypt_with_key_match(keyset, input).map_err(ReencryptError::Decryption)?;
	if matched_key.is_latest() {
		return Ok(ReencryptResult::Unchanged);
	}
	let envelope = encrypt(
		keyset,
		EncryptInput {
			plaintext: plaintext.expose_secret(),
			associated_data: input.associated_data,
		},
	)
	.map_err(ReencryptError::Encryption)?;
	Ok(ReencryptResult::Reencrypted(envelope))
}
