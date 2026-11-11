use serde::{Deserialize, Serialize};

/// An integer outside JavaScript's exactly representable integer range.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JsSafeIntegerError;

impl std::fmt::Display for JsSafeIntegerError {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter.write_str("integer is outside JavaScript's safe integer range")
	}
}

impl std::error::Error for JsSafeIntegerError {}

macro_rules! safe_integer {
	($name:ident, $integer:ty, $serde:literal, $min:expr, $max:expr, $doc:literal) => {
		#[doc = $doc]
		///
		/// Serde uses a JSON number. Construction and deserialization reject values
		/// outside the safe integer range, so JavaScript can decode and encode the
		/// value without rounding. Arithmetic requires extracting the integer and
		/// parsing the result again before it crosses the JSON boundary.
		#[derive(
			Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize,
		)]
		#[serde(try_from = $serde, into = $serde)]
		pub struct $name($integer);
		impl $name {
			/// The smallest supported integer.
			pub const MIN: $integer = $min;
			/// The largest supported integer.
			pub const MAX: $integer = $max;
			/// Parses an integer without rounding or clamping it.
			pub const fn new(value: $integer) -> Result<Self, JsSafeIntegerError> {
				if (value as i128) < (Self::MIN as i128) || (value as i128) > (Self::MAX as i128) {
					Err(JsSafeIntegerError)
				} else {
					Ok(Self(value))
				}
			}
			/// Returns the exact integer.
			pub const fn get(self) -> $integer {
				self.0
			}
		}
		impl TryFrom<$integer> for $name {
			type Error = JsSafeIntegerError;
			fn try_from(value: $integer) -> Result<Self, Self::Error> {
				Self::new(value)
			}
		}
		impl From<$name> for $integer {
			fn from(value: $name) -> Self {
				value.0
			}
		}
		#[cfg(feature = "tsgen")]
		impl crate::tsgen::TsGen for $name {
			fn type_ref() -> crate::tsgen::TypeRef {
				crate::tsgen::TypeRef::Integer
			}
		}
	};
}

safe_integer!(
	JsSafeU64,
	u64,
	"u64",
	0,
	9_007_199_254_740_991,
	"An unsigned integer that a JavaScript number represents exactly."
);
safe_integer!(
	JsSafeI64,
	i64,
	"i64",
	-9_007_199_254_740_991,
	9_007_199_254_740_991,
	"A signed integer that a JavaScript number represents exactly."
);
