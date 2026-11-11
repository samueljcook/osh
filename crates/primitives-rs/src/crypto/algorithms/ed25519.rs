//! Ed25519 for protocols that require RFC 8032 keys and signatures.
//!
//! This classical algorithm is not post-quantum. The `ed25519` feature is
//! independent of the post-quantum signature protocol. Signing is deterministic;
//! verification uses Dalek's strict checks, including rejection of small-order
//! public keys and signature points. This is pure Ed25519, with no implicit
//! context or prehashing.
use crate::crypto::{Key32, SignatureError};
use ed25519_dalek::Signer;
/// The RFC 8032 public-key length in bytes.
pub const PUBLIC_KEY_LEN: usize = 32;
/// The RFC 8032 signature length in bytes.
pub const SIGNATURE_LEN: usize = 64;
/// A signing key with zeroizing secret storage and redacted diagnostics.
pub struct SigningKey(ed25519_dalek::SigningKey);
impl SigningKey {
	/// Derives a key from the exact RFC 8032 seed.
	pub fn from_seed(seed: &Key32) -> Self {
		Self(ed25519_dalek::SigningKey::from_bytes(seed.expose_secret()))
	}
	/// Returns this signing key's public verification key.
	pub fn verifying_key(&self) -> VerifyingKey {
		VerifyingKey(self.0.verifying_key())
	}
	/// Signs the exact message bytes with pure Ed25519.
	pub fn sign(&self, message: &[u8]) -> Signature {
		Signature(self.0.sign(message))
	}
}
impl std::fmt::Debug for SigningKey {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str("SigningKey([REDACTED])")
	}
}
/// A parsed, non-small-order Ed25519 public key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifyingKey(ed25519_dalek::VerifyingKey);
impl VerifyingKey {
	/// Parses a compressed Edwards point and rejects small-order keys.
	pub fn parse(bytes: &[u8]) -> Result<Self, SignatureError> {
		let encoded = bytes.try_into().map_err(|_| SignatureError::InvalidInput)?;
		let key = ed25519_dalek::VerifyingKey::from_bytes(encoded)
			.map_err(|_| SignatureError::InvalidInput)?;
		if key.is_weak() {
			return Err(SignatureError::InvalidInput);
		}
		Ok(Self(key))
	}
	/// Encodes this public key using RFC 8032.
	pub fn to_bytes(&self) -> [u8; PUBLIC_KEY_LEN] {
		self.0.to_bytes()
	}
	/// Verifies with Dalek's strict scalar and point checks.
	pub fn verify(&self, message: &[u8], signature: &Signature) -> Result<(), SignatureError> {
		self.0
			.verify_strict(message, &signature.0)
			.map_err(|_| SignatureError::InvalidSignature)
	}
}
/// An exact-length RFC 8032 signature; verification checks its scalar and points.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature(ed25519_dalek::Signature);
impl Signature {
	/// Parses the signature length without authenticating its contents.
	pub fn parse(bytes: &[u8]) -> Result<Self, SignatureError> {
		ed25519_dalek::Signature::from_slice(bytes)
			.map(Self)
			.map_err(|_| SignatureError::InvalidInput)
	}
	/// Encodes this signature using RFC 8032.
	pub fn to_bytes(&self) -> [u8; SIGNATURE_LEN] {
		self.0.to_bytes()
	}
}
