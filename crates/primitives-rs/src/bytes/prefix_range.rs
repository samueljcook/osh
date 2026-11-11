//! Bounds for selecting byte strings with a common prefix in lexicographic order.
//!
//! The prefix is the inclusive lower bound. [`exclusive_upper_bound`] returns
//! the exclusive upper bound, or `None` when the range is unbounded above.
//! Comparisons must use unsigned byte order, with a shorter string before a
//! longer string when every byte of the shorter string matches.
//!
//! These bounds apply to Rust byte slices, SQLite BLOB keys, and PostgreSQL
//! BYTEA keys. They do not describe locale-aware text ordering.

/// Returns the smallest byte string greater than every string with `prefix`.
///
/// An empty prefix matches all strings and has no upper bound. A prefix made
/// entirely of `0xff` bytes also has no upper bound. The returned bound need
/// not be valid UTF-8 even when the prefix is valid UTF-8.
///
/// ```
/// use primitives_rs::bytes::prefix_range::exclusive_upper_bound;
/// assert_eq!(exclusive_upper_bound(b"ab"), Some(b"ac".to_vec()));
/// assert_eq!(exclusive_upper_bound(&[0x01, 0xff]), Some(vec![0x02]));
/// assert_eq!(exclusive_upper_bound(&[0xff]), None);
/// assert_eq!(exclusive_upper_bound(b""), None);
/// ```
pub fn exclusive_upper_bound(prefix: &[u8]) -> Option<Vec<u8>> {
	let last = prefix.iter().rposition(|byte| *byte != u8::MAX)?;
	let mut upper = prefix[..=last].to_vec();
	upper[last] += 1;
	Some(upper)
}
