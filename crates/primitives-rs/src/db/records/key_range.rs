use super::RecordKey;
use std::ops::Bound;

/// Explicit lower and upper bounds in unsigned lexicographic byte order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordKeyRangeBounds {
	/// The beginning of the selection.
	pub lower: Bound<RecordKey>,
	/// The end of the selection.
	pub upper: Bound<RecordKey>,
}

/// An ordered selection of byte keys. Equal endpoints with an excluded bound
/// select no keys. An unbounded lower endpoint includes the empty key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordKeyRange(RecordKeyRangeBounds);

/// Invalid byte-key selection input.
#[derive(Debug, thiserror::Error)]
pub enum RecordKeyRangeError {
	/// A prefix selection requires at least one byte.
	#[error("a record key prefix must not be empty; use an unbounded range for all keys")]
	EmptyPrefix,
	/// The lower endpoint is above the upper endpoint.
	#[error("record key range lower endpoint exceeds its upper endpoint")]
	InvertedBounds,
}

impl RecordKeyRange {
	/// Builds a range from explicit endpoints. A lower endpoint above the upper
	/// endpoint fails; equal endpoints may describe an empty selection.
	pub fn new(bounds: RecordKeyRangeBounds) -> Result<Self, RecordKeyRangeError> {
		if let (Some(lower), Some(upper)) = (endpoint(&bounds.lower), endpoint(&bounds.upper))
			&& lower > upper
		{
			return Err(RecordKeyRangeError::InvertedBounds);
		}
		Ok(Self(bounds))
	}

	/// Selects every byte key, including the empty key.
	pub fn new_all() -> Self {
		Self(RecordKeyRangeBounds {
			lower: Bound::Unbounded,
			upper: Bound::Unbounded,
		})
	}

	/// Selects exactly the keys beginning with a nonempty byte prefix. An
	/// all-`0xff` prefix has no finite upper bound.
	pub fn new_prefix(prefix: Vec<u8>) -> Result<Self, RecordKeyRangeError> {
		if prefix.is_empty() {
			return Err(RecordKeyRangeError::EmptyPrefix);
		}
		let upper = crate::bytes::prefix_range::exclusive_upper_bound(&prefix)
			.map_or(Bound::Unbounded, |bytes| {
				Bound::Excluded(RecordKey::new(bytes))
			});
		Ok(Self(RecordKeyRangeBounds {
			lower: Bound::Included(RecordKey::new(prefix)),
			upper,
		}))
	}

	/// Borrows the lower endpoint.
	pub fn lower_bound(&self) -> Bound<&RecordKey> {
		self.0.lower.as_ref()
	}

	/// Borrows the upper endpoint.
	pub fn upper_bound(&self) -> Bound<&RecordKey> {
		self.0.upper.as_ref()
	}

	/// Returns whether a key lies within both endpoints.
	pub fn contains(&self, key: &RecordKey) -> bool {
		let lower = match self.lower_bound() {
			Bound::Included(lower) => key >= lower,
			Bound::Excluded(lower) => key > lower,
			Bound::Unbounded => true,
		};
		lower
			&& match self.upper_bound() {
				Bound::Included(upper) => key <= upper,
				Bound::Excluded(upper) => key < upper,
				Bound::Unbounded => true,
			}
	}
}

fn endpoint(bound: &Bound<RecordKey>) -> Option<&RecordKey> {
	match bound {
		Bound::Included(key) | Bound::Excluded(key) => Some(key),
		Bound::Unbounded => None,
	}
}
