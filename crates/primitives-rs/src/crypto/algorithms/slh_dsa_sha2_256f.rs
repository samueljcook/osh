//! SLH-DSA-SHA2-256f with exact FIPS 205 seed material and context binding.
//!
//! The seed consists of SK.seed, SK.prf, and PK.seed, each 32 bytes, in that
//! order. This module performs no seed expansion. Use [`crate::crypto::signature`] for
//! the owned protocol that derives component seeds from one root and requires
//! both SLH-DSA-SHA2-256f and ML-DSA-87 signatures.
use crate::crypto::{
	SignatureError, SignatureMsg,
	signature_support::{FixedRandomness, check_context},
};
use slh_dsa::Sha2_256f;
use zeroize::Zeroizing;

/// The FIPS 205 seed-material length in bytes.
pub const SEED_LEN: usize = 96;
/// The FIPS 205 public-key length in bytes.
pub const PUBLIC_KEY_LEN: usize = 64;
/// The FIPS 205 signature length in bytes.
pub const SIGNATURE_LEN: usize = 49_856;

/// A signing key with zeroizing stored secrets and redacted diagnostics.
pub struct SigningKey(slh_dsa::SigningKey<Sha2_256f>);
impl SigningKey {
	/// Consumes exactly the supplied FIPS 205 seed material in field order.
	///
	/// The caller must supply uniformly random or securely derived bytes and
	/// protect and clear the original buffer.
	pub fn from_seed(seed: &[u8; SEED_LEN]) -> Self {
		let mut rng = FixedRandomness(seed);
		let key = Self(slh_dsa::SigningKey::new(&mut rng));
		assert!(rng.0.is_empty(), "SLH-DSA seed consumption");
		key
	}
	/// Returns this signing key's public verification key.
	pub fn verifying_key(&self) -> VerifyingKey {
		VerifyingKey(self.0.as_ref().clone())
	}
	/// Signs with fresh platform randomness and an explicit FIPS context.
	pub fn sign(&self, input: SignatureMsg<'_>) -> Result<Signature, SignatureError> {
		check_context(input)?;
		let mut randomizer = Zeroizing::new([0; 32]);
		crate::random::secure_bytes::fill_random_bytes(randomizer.as_mut())
			.map_err(SignatureError::Random)?;
		self.0
			.try_sign_with_context(input.msg, input.context, Some(randomizer.as_ref()))
			.map(Signature)
			.map_err(|_| SignatureError::InvalidInput)
	}
}
impl std::fmt::Debug for SigningKey {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str("SigningKey([REDACTED])")
	}
}
/// A parsed FIPS 205 public verification key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifyingKey(slh_dsa::VerifyingKey<Sha2_256f>);
impl VerifyingKey {
	/// Parses an exact-length FIPS 205 public key.
	pub fn parse(bytes: &[u8]) -> Result<Self, SignatureError> {
		slh_dsa::VerifyingKey::try_from(bytes)
			.map(Self)
			.map_err(|_| SignatureError::InvalidInput)
	}
	/// Encodes this public key using FIPS 205.
	pub fn to_bytes(&self) -> [u8; PUBLIC_KEY_LEN] {
		self.0
			.to_bytes()
			.as_slice()
			.try_into()
			.expect("SLH-DSA public-key size")
	}
	/// Verifies the message, context, and signature together.
	pub fn verify(
		&self,
		input: SignatureMsg<'_>,
		signature: &Signature,
	) -> Result<(), SignatureError> {
		check_context(input)?;
		self.0
			.try_verify_with_context(input.msg, input.context, &signature.0)
			.map_err(|_| SignatureError::InvalidSignature)
	}
}
/// A structurally parsed FIPS 205 detached signature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature(slh_dsa::Signature<Sha2_256f>);
impl Signature {
	/// Parses an exact-length FIPS 205 signature without verifying it.
	pub fn parse(bytes: &[u8]) -> Result<Self, SignatureError> {
		slh_dsa::Signature::try_from(bytes)
			.map(Self)
			.map_err(|_| SignatureError::InvalidInput)
	}
	/// Encodes this signature using FIPS 205.
	pub fn to_bytes(&self) -> Vec<u8> {
		self.0.to_vec()
	}
}
