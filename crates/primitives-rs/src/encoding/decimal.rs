//! Strict canonical base-10 integer text parsing.
//!
//! Canonical decimal text consists of `"0"` or an optional leading `"-"` followed by a non-zero
//! ASCII digit and zero or more ASCII digits. Leading `"+"` signs, whitespace, digit separators,
//! `"-0"`, and leading zeros on multi-digit integers are rejected.

use std::error::Error as StdError;
use std::fmt;
use std::num::{
	NonZeroI8, NonZeroI16, NonZeroI32, NonZeroI64, NonZeroI128, NonZeroIsize, NonZeroU8,
	NonZeroU16, NonZeroU32, NonZeroU64, NonZeroU128, NonZeroUsize,
};

/// A failure to parse canonical base-10 integer text into a target integer type.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CanonicalDecimalError {
	/// The input text was empty.
	Empty,
	/// The input text contained non-canonical characters, leading `+`, `-0`, or leading zeros.
	NonCanonicalSyntax,
	/// The input text used canonical base-10 syntax, but the value exceeded the target type's domain.
	OutOfRange,
}

impl fmt::Display for CanonicalDecimalError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Empty => f.write_str("decimal text must not be empty"),
			Self::NonCanonicalSyntax => {
				f.write_str("decimal text is not in canonical base-10 form")
			}
			Self::OutOfRange => {
				f.write_str("decimal value is out of range for target integer type")
			}
		}
	}
}

impl StdError for CanonicalDecimalError {}

mod sealed {
	pub trait Sealed {
		fn parse_canonical(input: &str) -> Result<Self, super::CanonicalDecimalError>
		where
			Self: Sized;
	}
}

/// An integer target type that [`parse_canonical_decimal`] supports.
///
/// This trait is sealed to the fixed-width, pointer-width, and non-zero integer types in `std`.
pub trait CanonicalDecimalInteger: sealed::Sealed + Sized {}

/// Parses `input` as canonical base-10 text for `T`.
///
/// Returns [`CanonicalDecimalError::Empty`] for an empty string and
/// [`CanonicalDecimalError::NonCanonicalSyntax`] when `input` contains leading `"+"` signs,
/// whitespace, `"-0"`, or leading zeros on multi-digit values. Returns
/// [`CanonicalDecimalError::OutOfRange`] when `input` is a negative canonical integer such as
/// `"-1"` and `T` is unsigned, when `input` is `"0"` and `T` is a `NonZero*` integer type, or
/// when the integer value exceeds `T`'s range.
pub fn parse_canonical_decimal<T: CanonicalDecimalInteger>(
	input: &str,
) -> Result<T, CanonicalDecimalError> {
	<T as sealed::Sealed>::parse_canonical(input)
}

fn check_canonical_integer_syntax(input: &str) -> Result<bool, CanonicalDecimalError> {
	let bytes = input.as_bytes();
	if bytes.is_empty() {
		return Err(CanonicalDecimalError::Empty);
	}
	if bytes == b"0" {
		return Ok(false);
	}
	let (negative, digits) = if let Some(rest) = bytes.strip_prefix(b"-") {
		(true, rest)
	} else {
		(false, bytes)
	};
	let Some((&first, rest)) = digits.split_first() else {
		return Err(CanonicalDecimalError::NonCanonicalSyntax);
	};
	if !matches!(first, b'1'..=b'9') || !rest.iter().all(u8::is_ascii_digit) {
		return Err(CanonicalDecimalError::NonCanonicalSyntax);
	}
	Ok(negative)
}

macro_rules! impl_unsigned_canonical_decimal {
	($($ty:ty),* $(,)?) => {
		$(
			impl sealed::Sealed for $ty {
				fn parse_canonical(input: &str) -> Result<Self, CanonicalDecimalError> {
					let negative = check_canonical_integer_syntax(input)?;
					if negative {
						return Err(CanonicalDecimalError::OutOfRange);
					}
					input
						.parse::<$ty>()
						.map_err(|_| CanonicalDecimalError::OutOfRange)
				}
			}
			impl CanonicalDecimalInteger for $ty {}
		)*
	};
}

macro_rules! impl_signed_canonical_decimal {
	($($ty:ty),* $(,)?) => {
		$(
			impl sealed::Sealed for $ty {
				fn parse_canonical(input: &str) -> Result<Self, CanonicalDecimalError> {
					let _negative = check_canonical_integer_syntax(input)?;
					input
						.parse::<$ty>()
						.map_err(|_| CanonicalDecimalError::OutOfRange)
				}
			}
			impl CanonicalDecimalInteger for $ty {}
		)*
	};
}

macro_rules! impl_nonzero_canonical_decimal {
	($(($nz:ty, $inner:ty)),* $(,)?) => {
		$(
			impl sealed::Sealed for $nz {
				fn parse_canonical(input: &str) -> Result<Self, CanonicalDecimalError> {
					let value = <$inner as sealed::Sealed>::parse_canonical(input)?;
					<$nz>::new(value).ok_or(CanonicalDecimalError::OutOfRange)
				}
			}
			impl CanonicalDecimalInteger for $nz {}
		)*
	};
}

impl_unsigned_canonical_decimal!(u8, u16, u32, u64, u128, usize);
impl_signed_canonical_decimal!(i8, i16, i32, i64, i128, isize);
impl_nonzero_canonical_decimal!(
	(NonZeroU8, u8),
	(NonZeroU16, u16),
	(NonZeroU32, u32),
	(NonZeroU64, u64),
	(NonZeroU128, u128),
	(NonZeroUsize, usize),
	(NonZeroI8, i8),
	(NonZeroI16, i16),
	(NonZeroI32, i32),
	(NonZeroI64, i64),
	(NonZeroI128, i128),
	(NonZeroIsize, isize),
);

#[cfg(test)]
mod tests {
	use super::{CanonicalDecimalError, parse_canonical_decimal};
	use std::num::NonZeroU64;

	#[test]
	fn canonical_decimal_rejects_noncanonical_spellings_and_distinguishes_range() {
		assert_eq!(parse_canonical_decimal::<u64>("0"), Ok(0));
		assert_eq!(parse_canonical_decimal::<u64>("42"), Ok(42));
		assert_eq!(parse_canonical_decimal::<i64>("-42"), Ok(-42));
		assert_eq!(
			parse_canonical_decimal::<u64>(""),
			Err(CanonicalDecimalError::Empty)
		);
		for noncanonical in ["00", "01", "-0", "+1", " 1", "1 ", "1_000", "-"] {
			assert_eq!(
				parse_canonical_decimal::<i64>(noncanonical),
				Err(CanonicalDecimalError::NonCanonicalSyntax),
				"{noncanonical:?}"
			);
		}
		assert_eq!(
			parse_canonical_decimal::<u64>("-1"),
			Err(CanonicalDecimalError::OutOfRange)
		);
		assert_eq!(
			parse_canonical_decimal::<NonZeroU64>("0"),
			Err(CanonicalDecimalError::OutOfRange)
		);
		assert_eq!(
			parse_canonical_decimal::<u8>("256"),
			Err(CanonicalDecimalError::OutOfRange)
		);
	}
}
