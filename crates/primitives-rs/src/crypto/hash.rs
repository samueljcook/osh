//! Hedged hashing with complete SHA-512 and SHA3-512 outputs.
//!
//! The 128-byte digest retains collision and second-preimage resistance when
//! either component retains the corresponding property. The complete outputs
//! are concatenated; truncating or hashing them down changes that guarantee.

use crate::crypto::algorithms::{sha3_512, sha512};
/// The complete combined digest size in bytes.
pub const DIGEST_LEN: usize = 128;

/// A complete digest from the version 1 hedged hashing protocol.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Digest([u8; DIGEST_LEN]);
fixed_bytes_traits!(Digest, DIGEST_LEN);
impl Digest {
	/// Constructs a digest from its complete byte representation.
	pub fn from_array(bytes: [u8; DIGEST_LEN]) -> Self {
		Self(bytes)
	}
	/// Returns SHA-512 followed by SHA3-512, without truncation.
	pub fn as_bytes(&self) -> &[u8; DIGEST_LEN] {
		&self.0
	}
}
impl AsRef<[u8]> for Digest {
	fn as_ref(&self) -> &[u8] {
		self.as_bytes()
	}
}

/// An incremental hedged hash of a byte stream.
#[derive(Debug)]
pub struct Hasher {
	sha: sha512::Hasher,
	keccak: sha3_512::Hasher,
}
impl Hasher {
	/// Starts the version 1 byte-stream protocol.
	pub fn new() -> Self {
		Self::with_domain(b"primitives-rs/hash/v1/bytes")
	}
	fn with_domain(domain: &[u8]) -> Self {
		let mut hasher = Self {
			sha: sha512::Hasher::new(),
			keccak: sha3_512::Hasher::new(),
		};
		crate::crypto::framing::update_fields(&mut |bytes| hasher.update(bytes), domain, &[]);
		hasher
	}
	/// Appends bytes; chunk boundaries do not affect the result.
	pub fn update(&mut self, bytes: &[u8]) {
		self.sha.update(bytes);
		self.keccak.update(bytes);
	}
	/// Returns both complete algorithm outputs.
	pub fn finalize(self) -> Digest {
		let mut output = [0; DIGEST_LEN];
		output[..64].copy_from_slice(self.sha.finalize().as_bytes());
		output[64..].copy_from_slice(self.keccak.finalize().as_bytes());
		Digest(output)
	}
}
impl Default for Hasher {
	fn default() -> Self {
		Self::new()
	}
}

/// Hashes a byte string with the version 1 hedged byte-stream protocol.
pub fn hash(bytes: &[u8]) -> Digest {
	let mut hasher = Hasher::new();
	hasher.update(bytes);
	hasher.finalize()
}

/// Framing inputs for a structured hash.
///
/// The purpose is public. Fields can be secret, so `Debug` output shows the
/// purpose and the number of fields, and omits the field bytes. The
/// [`crypto`](super) module documentation describes the limits of the digest
/// for secret inputs.
#[derive(Clone, Copy)]
pub struct Fields<'a> {
	/// An application-owned label that identifies the hash's role.
	pub purpose: &'a [u8],
	/// Ordered byte fields, which can be secret, each with its own length prefix.
	pub fields: &'a [&'a [u8]],
}

impl std::fmt::Debug for Fields<'_> {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Fields")
			.field(
				"purpose",
				&format_args!("b\"{}\"", self.purpose.escape_ascii()),
			)
			.field("field_count", &self.fields.len())
			.finish()
	}
}

/// Hashes a purpose and ordered fields without ambiguous concatenation.
pub fn hash_fields(input: Fields<'_>) -> Digest {
	let mut hasher = Hasher::with_domain(b"primitives-rs/hash/v1/fields");
	crate::crypto::framing::update_fields(
		&mut |bytes| hasher.update(bytes),
		input.purpose,
		input.fields,
	);
	hasher.finalize()
}
