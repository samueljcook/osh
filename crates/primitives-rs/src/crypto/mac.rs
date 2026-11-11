//! Hedged message authentication with HMAC-SHA512 and KMAC256.
//!
//! Each tag contains both complete 64-byte component tags. Verification requires
//! both tags under one rotation key. Component keys come from the shared hedged
//! derivation protocol with distinct purposes. Authenticity survives failure of
//! one MAC when the other MAC and the shared derivation retain their guarantees.

use crate::crypto::{
	Key32, VerificationError,
	algorithms::{hmac_sha512, kmac256},
	key_derivation,
	keysets::Keyset,
};
use subtle::{Choice, ConstantTimeEq};

pub(crate) const COMPONENT_TAG_LEN: usize = 64;
/// The complete combined tag size in bytes.
pub const TAG_LEN: usize = 2 * COMPONENT_TAG_LEN;

/// A complete tag from the version 1 hedged MAC protocol.
#[derive(Clone, Copy, Debug)]
pub struct Tag([u8; TAG_LEN]);
fixed_bytes_traits!(Tag, TAG_LEN);
impl Tag {
	/// Constructs a tag from its complete byte representation.
	pub fn from_array(bytes: [u8; TAG_LEN]) -> Self {
		Self(bytes)
	}
	/// Returns HMAC-SHA512 followed by KMAC256.
	pub fn as_bytes(&self) -> &[u8; TAG_LEN] {
		&self.0
	}
}
impl AsRef<[u8]> for Tag {
	fn as_ref(&self) -> &[u8] {
		self.as_bytes()
	}
}
impl PartialEq for Tag {
	fn eq(&self, other: &Self) -> bool {
		self.0.ct_eq(&other.0).into()
	}
}
impl Eq for Tag {}

/// An incremental hedged MAC of a byte stream under the newest rotation key.
pub struct Authenticator(CombinedMacState);
impl Authenticator {
	/// Starts a byte stream under a purpose-bound keyset.
	pub fn new(keyset: &Keyset) -> Self {
		Self(CombinedMacState::new(
			keyset.latest_key(),
			b"primitives-rs/mac/v1/bytes",
		))
	}
	/// Appends bytes; chunk boundaries do not affect the result.
	pub fn update(&mut self, bytes: &[u8]) {
		self.0.update(bytes);
	}
	/// Returns both complete authentication tags.
	pub fn finalize(self) -> Tag {
		self.0.finalize()
	}
}
impl std::fmt::Debug for Authenticator {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str("Authenticator([redacted])")
	}
}

/// An incremental byte-stream verifier for every key in a rotation set.
pub struct Verifier(Vec<CombinedMacState>);
impl Verifier {
	/// Starts verification without retaining the message in memory.
	pub fn new(keyset: &Keyset) -> Result<Self, crate::secret_bytes::AllocationFailedError> {
		let mut states = Vec::new();
		states
			.try_reserve_exact(keyset.key_count())
			.map_err(|_| crate::secret_bytes::AllocationFailedError)?;
		for key in keyset.latest_first_keys() {
			states.push(CombinedMacState::new(key, b"primitives-rs/mac/v1/bytes"));
		}
		Ok(Self(states))
	}
	/// Appends bytes; chunk boundaries do not affect the result.
	pub fn update(&mut self, bytes: &[u8]) {
		for state in &mut self.0 {
			state.update(bytes);
		}
	}
	/// Requires both tag components to match under the same rotation key.
	pub fn verify(self, tag: &Tag) -> Result<(), VerificationError> {
		let mut matched = Choice::from(0);
		for state in self.0 {
			matched |= state.finalize().as_bytes().ct_eq(tag.as_bytes());
		}
		if matched.into() {
			Ok(())
		} else {
			Err(VerificationError)
		}
	}
}
impl std::fmt::Debug for Verifier {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str("Verifier([redacted])")
	}
}

/// Creates a tag over ordered, length-prefixed fields with the newest key.
///
/// Use a purpose-bound keyset for each application role. Field boundaries and
/// order are authenticated; `[]`, `[b""]`, and `[b"", b""]` are distinct.
pub fn authenticate_fields(keyset: &Keyset, fields: &[&[u8]]) -> Tag {
	authenticate_fields_with_key(keyset.latest_key(), fields)
}

/// Verifies both tag components under the same key, trying every rotation key.
pub fn verify_fields(
	keyset: &Keyset,
	fields: &[&[u8]],
	tag: &Tag,
) -> Result<(), VerificationError> {
	let mut matched = Choice::from(0);
	for key in keyset.latest_first_keys() {
		matched |= authenticate_fields_with_key(key, fields)
			.as_bytes()
			.ct_eq(tag.as_bytes());
	}
	if matched.into() {
		Ok(())
	} else {
		Err(VerificationError)
	}
}

pub(crate) fn authenticate_fields_with_key(key: &Key32, fields: &[&[u8]]) -> Tag {
	let mut state = CombinedMacState::new(key, b"primitives-rs/mac/v1/fields");
	crate::crypto::framing::update_fields(&mut |bytes| state.update(bytes), b"fields", fields);
	state.finalize()
}

struct CombinedMacState {
	hmac: hmac_sha512::Authenticator,
	kmac: kmac256::Authenticator,
}
impl CombinedMacState {
	fn new(key: &Key32, domain: &[u8]) -> Self {
		let derive = |purpose| {
			key_derivation::derive_key(
				key,
				key_derivation::Context {
					salt: b"",
					purpose,
					fields: &[],
				},
			)
		};
		let hmac_key = derive(b"primitives-rs/mac/v1/hmac-sha512");
		let kmac_key = derive(b"primitives-rs/mac/v1/kmac256");
		let mut state = Self {
			hmac: hmac_sha512::Authenticator::new(hmac_key.expose_secret()),
			kmac: kmac256::Authenticator::new(&kmac_key, b"primitives-rs/mac/v1"),
		};
		crate::crypto::framing::update_fields(&mut |bytes| state.update(bytes), domain, &[]);
		state
	}
	fn update(&mut self, bytes: &[u8]) {
		self.hmac.update(bytes);
		self.kmac.update(bytes);
	}
	fn finalize(self) -> Tag {
		let mut output = [0_u8; TAG_LEN];
		output[..COMPONENT_TAG_LEN].copy_from_slice(&self.hmac.finalize());
		self.kmac
			.finalize_into(&mut output[COMPONENT_TAG_LEN..])
			.expect("64 bytes fit the KMAC output limit");
		Tag(output)
	}
}
