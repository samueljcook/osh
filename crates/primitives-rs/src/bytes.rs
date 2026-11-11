//! Operations on byte sequences and 64-bit big-endian length-prefixed framing.

#[cfg(feature = "byte-prefix-range")]
pub mod prefix_range;

#[cfg(feature = "fastcdc")]
pub mod fastcdc;

/// Appends the 8-byte big-endian length of `field` followed by `field` onto `out`.
pub fn append_length_prefixed(out: &mut Vec<u8>, field: &[u8]) {
	let len = u64::try_from(field.len()).expect("slice length fits in u64");
	out.reserve(8 + field.len());
	out.extend_from_slice(&len.to_be_bytes());
	out.extend_from_slice(field);
}

/// Encodes an ordered sequence of byte slices as concatenated 8-byte big-endian length-prefixed fields.
pub fn encode_length_prefixed(fields: impl IntoIterator<Item = impl AsRef<[u8]>>) -> Vec<u8> {
	let mut out = Vec::new();
	for field in fields {
		append_length_prefixed(&mut out, field.as_ref());
	}
	out
}

/// Feeds the 8-byte big-endian length of `field` and then `field` into `sink_fn` without allocating.
pub fn write_length_prefixed(field: &[u8], mut sink_fn: impl FnMut(&[u8])) {
	let len = u64::try_from(field.len()).expect("slice length fits in u64");
	sink_fn(&len.to_be_bytes());
	sink_fn(field);
}
