use super::{
	encryption::{self, MAX_ASSOCIATED_DATA_LEN, MIN_ENVELOPE_LEN},
	hash,
	keysets::derive_keyset_from_latest_first_keys,
	password_derivation::{self, Parameters},
	password_encryption::{Error, PasswordDecryptInput, PasswordEncryptInput},
};
use crate::secret_bytes::{SecretBytes, allocate_byte_buffer};

pub(super) const HEADER_LEN: usize = 6 + password_derivation::PARAMETERS_LEN;

#[derive(Clone, Copy)]
pub(super) struct RecordFormat {
	pub prefix: &'static [u8; 5],
	pub key_purpose: &'static str,
	pub context_purpose: &'static [u8],
}

pub(super) fn validate(bytes: &[u8], format: RecordFormat) -> Result<(), Error> {
	if !(HEADER_LEN + MIN_ENVELOPE_LEN..=HEADER_LEN + encryption::MAX_ENVELOPE_LEN)
		.contains(&bytes.len())
		|| &bytes[..5] != format.prefix
		|| bytes[5] > 1
	{
		return Err(Error::InvalidRecord);
	}
	Parameters::parse(&bytes[6..HEADER_LEN]).map_err(|_| Error::InvalidRecord)?;
	encryption::validate_envelope_structure(&bytes[HEADER_LEN..]).map_err(|_| Error::InvalidRecord)
}

pub(super) fn parameters(bytes: &[u8]) -> Parameters {
	Parameters::parse(&bytes[6..HEADER_LEN]).expect("parsed password record parameters")
}

pub(super) fn encrypt(
	plaintext: &[u8],
	input: PasswordEncryptInput<'_>,
	format: RecordFormat,
) -> Result<Vec<u8>, Error> {
	check_context(input.context)?;
	if plaintext.len() > encryption::MAX_PLAINTEXT_LEN {
		return Err(Error::PlaintextTooLong);
	}
	input
		.limits
		.check(&input.profile, input.password.password.len())
		.map_err(Error::Derivation)?;
	let parameters = Parameters::generate(input.profile).map_err(Error::Random)?;
	let root = parameters
		.derive_key(input.password, input.limits)
		.map_err(Error::Derivation)?;
	let keyset = derive_keyset_from_latest_first_keys([root], format.key_purpose)
		.expect("one root and the fixed protocol purpose satisfy keyset bounds");
	let mut header = [0; HEADER_LEN];
	header[..5].copy_from_slice(format.prefix);
	header[5] = u8::from(input.password.additional_secret.is_some());
	header[6..].copy_from_slice(&parameters.to_bytes());
	let associated_data = hash_context(
		ContextHashInput {
			header: &header,
			context: input.context,
		},
		format,
	);
	let envelope = encryption::encrypt(
		&keyset,
		encryption::EncryptInput {
			plaintext,
			associated_data: associated_data.as_ref(),
		},
	)
	.map_err(Error::Encryption)?;
	let mut record =
		allocate_byte_buffer(HEADER_LEN + envelope.len()).map_err(|_| Error::AllocationFailed)?;
	record.extend_from_slice(&header);
	record.extend_from_slice(&envelope);
	Ok(record)
}

pub(super) fn decrypt(
	bytes: &[u8],
	input: PasswordDecryptInput<'_>,
	format: RecordFormat,
) -> Result<SecretBytes, Error> {
	check_context(input.context)?;
	if (bytes[5] == 1) != input.password.additional_secret.is_some() {
		return Err(Error::AdditionalSecretMismatch);
	}
	let root = parameters(bytes)
		.derive_key(input.password, input.limits)
		.map_err(Error::Derivation)?;
	let keyset = derive_keyset_from_latest_first_keys([root], format.key_purpose)
		.expect("one root and the fixed protocol purpose satisfy keyset bounds");
	let associated_data = hash_context(
		ContextHashInput {
			header: &bytes[..HEADER_LEN],
			context: input.context,
		},
		format,
	);
	encryption::decrypt(
		&keyset,
		encryption::DecryptInput {
			envelope: &bytes[HEADER_LEN..],
			associated_data: associated_data.as_ref(),
		},
	)
	.map_err(|error| match error {
		encryption::DecryptError::AllocationFailed => Error::AllocationFailed,
		_ => Error::AuthenticationFailed,
	})
}

fn check_context(context: &[u8]) -> Result<(), Error> {
	if context.len() > MAX_ASSOCIATED_DATA_LEN {
		Err(Error::ContextTooLong)
	} else {
		Ok(())
	}
}

struct ContextHashInput<'a> {
	header: &'a [u8],
	context: &'a [u8],
}

fn hash_context(input: ContextHashInput<'_>, format: RecordFormat) -> hash::Digest {
	hash::hash_fields(hash::Fields {
		purpose: format.context_purpose,
		fields: &[input.header, input.context],
	})
}
