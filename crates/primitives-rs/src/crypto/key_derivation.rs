//! Hedged high-entropy key derivation using HKDF-SHA512 XOR KMAC256.
//!
//! One high-entropy master secret produces each output through both branches.
//! Security requires that one branch remain pseudorandom given the other
//! branch's output. Domain separation prevents cross-purpose reuse; it does not
//! establish that assumption. SHA-2 and Keccak provide distinct hash families.
//! This protocol does not stretch passwords or add entropy to weak input keys.

use crate::{
	crypto::{
		Key32, OutputLengthError,
		algorithms::{hkdf_sha512, kmac256},
	},
	secret_bytes::SecretBytes,
};
use cshake::digest::XofReader;
use zeroize::Zeroizing;

const DOMAIN: &[u8] = b"primitives-rs/key-derivation/v1";
/// The maximum derived output size in bytes.
pub const MAX_OUTPUT_LEN: usize = hkdf_sha512::MAX_OUTPUT_LEN;

/// Inputs that distinguish one derivation from another.
///
/// The salt and purpose are public. Fields can be secret, such as a code that a
/// secret verifier checks, so `Debug` output shows the salt length, the purpose,
/// and the number of fields, and omits the salt and field bytes.
#[derive(Clone, Copy)]
pub struct Context<'a> {
	/// Public salt specified by the consuming protocol; it may be empty.
	pub salt: &'a [u8],
	/// A protocol-owned label that identifies the derived key's role.
	pub purpose: &'a [u8],
	/// Ordered application fields, which can be secret; the protocol frames each
	/// field separately.
	pub fields: &'a [&'a [u8]],
}

impl std::fmt::Debug for Context<'_> {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Context")
			.field("salt_len", &self.salt.len())
			.field(
				"purpose",
				&format_args!("b\"{}\"", self.purpose.escape_ascii()),
			)
			.field("field_count", &self.fields.len())
			.finish()
	}
}

/// Derives a 32-byte secret key through both algorithm branches.
pub fn derive_key(master_key: &Key32, context: Context<'_>) -> Key32 {
	let mut output = Key32::zeroed();
	derive_into(master_key, context, output.as_mut_bytes())
		.expect("32 bytes fit the derivation limit");
	output
}

/// Derives an explicitly sized, zeroizing secret buffer.
pub fn derive_bytes(
	master_key: &Key32,
	context: Context<'_>,
	output_len: usize,
) -> Result<SecretBytes, DeriveBytesError> {
	check_output_len(output_len)?;
	let mut output = SecretBytes::zeroed(output_len).map_err(DeriveBytesError::AllocationFailed)?;
	derive_into(master_key, context, output.expose_secret_mut())?;
	Ok(output)
}

/// Derives into a caller-owned buffer, which the caller must protect and clear.
///
/// The protocol binds the output length, so different lengths produce distinct
/// outputs. An invalid output length leaves the output unchanged.
pub fn derive_into(
	master_key: &Key32,
	context: Context<'_>,
	output: &mut [u8],
) -> Result<(), OutputLengthError> {
	check_output_len(output.len())?;
	// A fixed-size digest encodes arbitrary field counts without an allocation.
	// Each branch hashes the framing with its own hash family.
	let mut sha = crate::crypto::algorithms::sha512::Hasher::new();
	let mut keccak = crate::crypto::algorithms::sha3_512::Hasher::new();
	crate::crypto::framing::update_fields(
		&mut |bytes| {
			sha.update(bytes);
			keccak.update(bytes);
		},
		DOMAIN,
		&[
			context.salt,
			context.purpose,
			&(output.len() as u64).to_be_bytes(),
		],
	);
	crate::crypto::framing::update_fields(
		&mut |bytes| {
			sha.update(bytes);
			keccak.update(bytes);
		},
		b"fields",
		context.fields,
	);
	// The transcript digests depend on the fields, which can be secret.
	let mut sha_context = Zeroizing::new([0_u8; 64]);
	sha.finalize_into(&mut sha_context);
	let mut keccak_context = Zeroizing::new([0_u8; 64]);
	keccak.finalize_into(&mut keccak_context);
	hkdf_sha512::derive_into(
		master_key.expose_secret(),
		hkdf_sha512::Context {
			salt: DOMAIN,
			info: &[sha_context.as_slice()],
		},
		output,
	)?;
	let mut kmac = kmac256::Authenticator::new(master_key, DOMAIN);
	kmac.update(keccak_context.as_slice());
	let mut reader = kmac
		.finalize_reader(output.len())
		.expect("HKDF's limit fits KMAC's limit");
	let mut block = Zeroizing::new([0_u8; 64]);
	for chunk in output.chunks_mut(block.len()) {
		reader.read(&mut block[..chunk.len()]);
		for (left, right) in chunk.iter_mut().zip(block.iter()) {
			*left ^= right;
		}
	}
	Ok(())
}

fn check_output_len(actual: usize) -> Result<(), OutputLengthError> {
	if actual > MAX_OUTPUT_LEN {
		Err(OutputLengthError {
			actual,
			max: MAX_OUTPUT_LEN,
		})
	} else {
		Ok(())
	}
}

/// A derived buffer could not be sized or allocated.
#[derive(Debug)]
pub enum DeriveBytesError {
	/// The requested output exceeded the protocol's limit.
	OutputLength(OutputLengthError),
	/// The secret buffer could not be allocated.
	AllocationFailed(crate::secret_bytes::AllocationFailedError),
}
impl From<OutputLengthError> for DeriveBytesError {
	fn from(error: OutputLengthError) -> Self {
		Self::OutputLength(error)
	}
}
impl std::fmt::Display for DeriveBytesError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::OutputLength(error) => error.fmt(f),
			Self::AllocationFailed(error) => error.fmt(f),
		}
	}
}
impl std::error::Error for DeriveBytesError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self {
			Self::OutputLength(error) => Some(error),
			Self::AllocationFailed(error) => Some(error),
		}
	}
}
