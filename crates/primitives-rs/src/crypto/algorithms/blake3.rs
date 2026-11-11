//! BLAKE3 hashing, keyed hashing, and key derivation with zeroizing keyed state.
//!
//! BLAKE3's native incremental and extended-output interfaces remain available.
//! These operations use BLAKE3 alone; the owned hedged protocols are
//! [`crate::crypto::hash`], [`crate::crypto::mac`], and [`crate::crypto::key_derivation`].

use crate::crypto::Key32;
pub use ::blake3::{Hash, hash};
use zeroize::Zeroizing;

/// An incremental BLAKE3 state that clears its memory on drop.
#[derive(Clone)]
pub struct Hasher(Zeroizing<::blake3::Hasher>);
impl Hasher {
	/// Starts an unkeyed byte stream.
	pub fn new() -> Self {
		Self(Zeroizing::new(::blake3::Hasher::new()))
	}
	/// Starts a keyed byte stream.
	pub fn new_keyed(key: &Key32) -> Self {
		Self(Zeroizing::new(::blake3::Hasher::new_keyed(
			key.expose_secret(),
		)))
	}
	/// Starts BLAKE3 derive-key mode with a protocol-owned context.
	pub fn new_derive_key(context: &'static str) -> Self {
		Self(Zeroizing::new(::blake3::Hasher::new_derive_key(context)))
	}
	/// Appends bytes without framing and returns the stream.
	pub fn update(&mut self, bytes: &[u8]) -> &mut Self {
		self.0.update(bytes);
		self
	}
	/// Returns the complete digest without consuming the stream.
	pub fn finalize(&self) -> Hash {
		self.0.finalize()
	}
	/// Returns an extended-output reader that clears its memory on drop.
	pub fn finalize_xof(&self) -> OutputReader {
		OutputReader(Zeroizing::new(self.0.finalize_xof()))
	}
}
impl Default for Hasher {
	fn default() -> Self {
		Self::new()
	}
}
impl std::fmt::Debug for Hasher {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str("Hasher([redacted])")
	}
}

/// A zeroizing BLAKE3 extended-output reader.
pub struct OutputReader(Zeroizing<::blake3::OutputReader>);
impl OutputReader {
	/// Reads the next output bytes and advances the reader's position.
	pub fn fill(&mut self, output: &mut [u8]) {
		self.0.fill(output);
	}
}
impl std::fmt::Debug for OutputReader {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str("OutputReader([redacted])")
	}
}

/// Hashes concatenated slices without inserting separators or lengths.
pub fn hash_parts(parts: &[&[u8]]) -> Hash {
	let mut hasher = Hasher::new();
	for part in parts {
		hasher.update(part);
	}
	hasher.finalize()
}

/// Computes a keyed BLAKE3 hash of a byte string.
pub fn keyed_hash(key: &Key32, bytes: &[u8]) -> Hash {
	let mut hasher = Hasher::new_keyed(key);
	hasher.update(bytes);
	hasher.finalize()
}

/// Fills an output buffer from one keyed BLAKE3 extended-output stream.
pub fn keyed_hash_into(key: &Key32, bytes: &[u8], output: &mut [u8]) {
	let mut hasher = Hasher::new_keyed(key);
	hasher.update(bytes);
	hasher.finalize_xof().fill(output);
}

/// Derives a key with BLAKE3's derive-key mode.
///
/// The context must be a protocol-owned, globally unique string. This operation
/// does not stretch passwords.
pub fn derive_key(key_material: &[u8], context: &'static str) -> Key32 {
	let mut hasher = Hasher::new_derive_key(context);
	hasher.update(key_material);
	let mut output = Key32::zeroed();
	hasher.finalize_xof().fill(output.as_mut_bytes());
	output
}
