//! Versioned signatures that require SLH-DSA-SHA2-256f and ML-DSA-87.
//!
//! # Protocol
//!
//! For one high-entropy 32-byte root, derive 96 SLH-DSA seed bytes and a 32-byte
//! ML-DSA seed through [`crate::crypto::key_derivation`], using empty salt and fields,
//! with purposes `primitives-rs/signature/v1/slh-dsa-sha2-256f` and
//! `primitives-rs/signature/v1/ml-dsa-87`, respectively. The SLH-DSA seed bytes
//! are SK.seed, SK.prf, and PK.seed in that order. The same purpose strings are
//! the respective FIPS signing contexts.
//!
//! A public key is `SPKY || 0x01 || slh_public_key[64] || ml_public_key[2592]`.
//! A signature is `SIGN || 0x01 || slh_signature[49856] || ml_signature[4627]`.
//! Both component algorithms sign the complete 128-byte result of
//! `hash::hash_fields` with purpose `primitives-rs/signature/v1` and ordered
//! fields `[encoded_public_key, application_context, message]`. Parsing accepts
//! only these exact lengths and version. Verification requires both components.
//! These are this crate's protocol encodings, not FIPS encodings or an IETF
//! composite-signature format. The algorithm modules provide the FIPS formats.
//!
//! # Security properties
//!
//! A forgery for a new message or context under a fixed, authenticated public key
//! requires either a collision in the complete framed digest or a forgery against
//! both component signatures. The digest concatenates SHA-512 and SHA3-512, so
//! collision resistance survives a failure of either hash. Component seed
//! separation relies on the shared KDF's stated joint pseudorandomness assumption.
//! Under these assumptions, either signature scheme's unforgeability can protect
//! against failure of the other scheme. This is not a claim that the schemes
//! have independent implementations or that arbitrary software compromise is
//! contained. The root must remain secret; a password-derived root additionally
//! depends on its password-stretching protocol.
//!
//! Both component public keys are bound into the digest, preventing replacement
//! of one component key without new signatures. Component keys are private to
//! this protocol. Applications must authenticate public keys and bind the
//! appropriate application identity through the explicit context. Randomized
//! signatures are not unique message identifiers and do not themselves prevent
//! replay. The protocol claims existential unforgeability, not uniqueness or
//! strong unforgeability of the combined signature bytes.
//!
//! # Example
//!
//! ```
//! use primitives_rs::crypto::{Key32, SignatureMsg, signature};
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let root = Key32::generate()?;
//! let signing = signature::SigningKey::from_root(&root);
//! let public_key = signing.verifying_key();
//! let message = SignatureMsg { context: b"example.approval.v1", msg: b"approve item 42" };
//! let signed = signing.sign(message)?;
//! let parsed = signature::Signature::parse(signed.as_bytes())?;
//! public_key.verify(message, &parsed)?;
//! # Ok(())
//! # }
//! ```
use crate::crypto::{
	Key32, SignatureError, SignatureMsg,
	algorithms::{ml_dsa_87, slh_dsa_sha2_256f},
	hash, key_derivation,
};
use zeroize::Zeroizing;

const DOMAIN: &[u8] = b"primitives-rs/signature/v1";
const SLH_DOMAIN: &[u8] = b"primitives-rs/signature/v1/slh-dsa-sha2-256f";
const ML_DOMAIN: &[u8] = b"primitives-rs/signature/v1/ml-dsa-87";
const KEY_HEADER: &[u8; 5] = b"SPKY\x01";
const SIGNATURE_HEADER: &[u8; 5] = b"SIGN\x01";
const ML_KEY_OFFSET: usize = 5 + slh_dsa_sha2_256f::PUBLIC_KEY_LEN;
const ML_SIGNATURE_OFFSET: usize = 5 + slh_dsa_sha2_256f::SIGNATURE_LEN;
/// The encoded combined public-key length in bytes.
pub const PUBLIC_KEY_LEN: usize = ML_KEY_OFFSET + ml_dsa_87::PUBLIC_KEY_LEN;
/// The encoded combined signature length in bytes.
pub const SIGNATURE_LEN: usize = ML_SIGNATURE_OFFSET + ml_dsa_87::SIGNATURE_LEN;

/// A pair of signing keys derived from one root; stored secrets clear on drop.
#[derive(Debug)]
pub struct SigningKey {
	slh: slh_dsa_sha2_256f::SigningKey,
	ml: ml_dsa_87::SigningKey,
	public_key: VerifyingKey,
}
impl SigningKey {
	/// Derives this protocol's component keys from a high-entropy root.
	///
	/// Applications that share a master across purposes should first derive a
	/// root for their signing role through the shared key derivation API.
	pub fn from_root(root: &Key32) -> Self {
		let context = |purpose| key_derivation::Context {
			salt: b"",
			purpose,
			fields: &[],
		};
		let mut slh_seed = Zeroizing::new([0; slh_dsa_sha2_256f::SEED_LEN]);
		key_derivation::derive_into(root, context(SLH_DOMAIN), slh_seed.as_mut())
			.expect("96 bytes fit the KDF limit");
		let slh = slh_dsa_sha2_256f::SigningKey::from_seed(&slh_seed);
		let ml =
			ml_dsa_87::SigningKey::from_seed(&key_derivation::derive_key(root, context(ML_DOMAIN)));
		let mut public_key = [0; PUBLIC_KEY_LEN];
		public_key[..5].copy_from_slice(KEY_HEADER);
		public_key[5..ML_KEY_OFFSET].copy_from_slice(&slh.verifying_key().to_bytes());
		public_key[ML_KEY_OFFSET..].copy_from_slice(&ml.verifying_key().to_bytes());
		Self {
			slh,
			ml,
			public_key: VerifyingKey(public_key),
		}
	}
	/// Returns the complete public-key pair and its protocol version.
	pub fn verifying_key(&self) -> VerifyingKey {
		self.public_key.clone()
	}
	/// Signs the framed public key, application context, and message with both schemes.
	pub fn sign(&self, input: SignatureMsg<'_>) -> Result<Signature, SignatureError> {
		let digest = hash_transcript(&self.public_key, input);
		let slh = self.slh.sign(SignatureMsg {
			context: SLH_DOMAIN,
			msg: digest.as_bytes(),
		})?;
		let ml = self.ml.sign(SignatureMsg {
			context: ML_DOMAIN,
			msg: digest.as_bytes(),
		})?;
		let mut bytes = Vec::with_capacity(SIGNATURE_LEN);
		bytes.extend_from_slice(SIGNATURE_HEADER);
		bytes.extend_from_slice(&slh.to_bytes());
		bytes.extend_from_slice(&ml.to_bytes());
		Ok(Signature(bytes))
	}
}
/// An exact-version combined public key; parsing does not establish its owner.
#[derive(Clone, PartialEq, Eq)]
pub struct VerifyingKey([u8; PUBLIC_KEY_LEN]);
impl VerifyingKey {
	/// Parses the exact protocol version, length, and component key encodings.
	pub fn parse(bytes: &[u8]) -> Result<Self, SignatureError> {
		if bytes.len() != PUBLIC_KEY_LEN || &bytes[..5] != KEY_HEADER {
			return Err(SignatureError::InvalidInput);
		}
		slh_dsa_sha2_256f::VerifyingKey::parse(&bytes[5..ML_KEY_OFFSET])?;
		ml_dsa_87::VerifyingKey::parse(&bytes[ML_KEY_OFFSET..])?;
		Ok(Self(bytes.try_into().expect("checked public-key length")))
	}
	/// Returns the complete versioned public-key encoding.
	pub fn as_bytes(&self) -> &[u8; PUBLIC_KEY_LEN] {
		&self.0
	}
	/// Requires both component signatures over the same complete transcript.
	pub fn verify(
		&self,
		input: SignatureMsg<'_>,
		signature: &Signature,
	) -> Result<(), SignatureError> {
		let digest = hash_transcript(self, input);
		let slh_key = slh_dsa_sha2_256f::VerifyingKey::parse(&self.0[5..ML_KEY_OFFSET])?;
		let ml_key = ml_dsa_87::VerifyingKey::parse(&self.0[ML_KEY_OFFSET..])?;
		let slh_signature =
			slh_dsa_sha2_256f::Signature::parse(&signature.0[5..ML_SIGNATURE_OFFSET])?;
		let ml_signature = ml_dsa_87::Signature::parse(&signature.0[ML_SIGNATURE_OFFSET..])?;
		// Both components must succeed; there is no algorithm negotiation.
		slh_key.verify(
			SignatureMsg {
				context: SLH_DOMAIN,
				msg: digest.as_bytes(),
			},
			&slh_signature,
		)?;
		ml_key.verify(
			SignatureMsg {
				context: ML_DOMAIN,
				msg: digest.as_bytes(),
			},
			&ml_signature,
		)
	}
}
impl std::fmt::Debug for VerifyingKey {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("VerifyingKey")
			.field("len", &PUBLIC_KEY_LEN)
			.finish()
	}
}
/// A structurally parsed signature containing both required components.
#[derive(Clone, PartialEq, Eq)]
pub struct Signature(Vec<u8>);
impl Signature {
	/// Parses the exact version and length and both component encodings.
	pub fn parse(bytes: &[u8]) -> Result<Self, SignatureError> {
		if bytes.len() != SIGNATURE_LEN || &bytes[..5] != SIGNATURE_HEADER {
			return Err(SignatureError::InvalidInput);
		}
		slh_dsa_sha2_256f::Signature::parse(&bytes[5..ML_SIGNATURE_OFFSET])?;
		ml_dsa_87::Signature::parse(&bytes[ML_SIGNATURE_OFFSET..])?;
		Ok(Self(bytes.to_vec()))
	}
	/// Returns the complete versioned signature encoding.
	pub fn as_bytes(&self) -> &[u8] {
		&self.0
	}
	/// Transfers the encoded signature without copying its buffer.
	pub fn into_bytes(self) -> Vec<u8> {
		self.0
	}
}
impl std::fmt::Debug for Signature {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Signature")
			.field("len", &SIGNATURE_LEN)
			.finish()
	}
}
fn hash_transcript(key: &VerifyingKey, input: SignatureMsg<'_>) -> hash::Digest {
	hash::hash_fields(hash::Fields {
		purpose: DOMAIN,
		fields: &[key.as_bytes(), input.context, input.msg],
	})
}
