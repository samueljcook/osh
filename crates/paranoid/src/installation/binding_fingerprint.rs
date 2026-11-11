//! Canonical framing for code-derived PostgreSQL installation binding fingerprints and record
//! keys.

use primitives_rs::bytes::{append_length_prefixed, write_length_prefixed};
use primitives_rs::crypto::algorithms::blake3;

/// Frames one complete byte value so adjoining values cannot produce an ambiguous encoding.
pub(super) fn frame_fingerprint_input(hasher: &mut blake3::Hasher, bytes: &[u8]) {
	write_length_prefixed(bytes, |chunk| {
		hasher.update(chunk);
	});
}

/// Appends one complete byte value to `output` in the framing of [`frame_fingerprint_input`].
pub(super) fn append_framed(output: &mut Vec<u8>, bytes: &[u8]) {
	append_length_prefixed(output, bytes);
}
