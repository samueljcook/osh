use std::fmt;
use std::str::FromStr;

use serde::de::Visitor;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::tsgen::{TsGen, TypeRef};

mod sealed {
	pub trait Sealed {}
}

/// An integer type that JavaScript's number cannot hold in full: `i64`, `u64`, `i128`, `u128`,
/// `isize`, or `usize`. No other type implements it.
pub trait WideInteger:
	sealed::Sealed + Copy + fmt::Display + FromStr + Send + Sync + 'static
{
}

macro_rules! wide_integer {
	($($ty:ty),* $(,)?) => {
		$(
			impl sealed::Sealed for $ty {}
			impl WideInteger for $ty {}
		)*
	};
}

wide_integer!(i64, i128, isize, u64, u128, usize);

/// A 64-bit or 128-bit integer that crosses the wire as its decimal text.
///
/// JavaScript reads a JSON number as a double, which cannot hold every such integer, so a field
/// of a wide integer type has no TypeScript form. A field of this type serializes as a JSON
/// string of decimal digits, with a leading `-` for a negative value, and its TypeScript type is
/// `string`. Browser code converts the text with `BigInt(text)`.
///
/// Deserialization accepts a JSON string with the text that the integer type's `FromStr`
/// accepts, and rejects a JSON number.
///
/// ```
/// use primitives_rs::tsgen::IntegerString;
///
/// let id = IntegerString(18_446_744_073_709_551_615_u64);
/// assert_eq!(serde_json::to_string(&id).unwrap(), "\"18446744073709551615\"");
/// let read: IntegerString<u64> = serde_json::from_str("\"18446744073709551615\"").unwrap();
/// assert_eq!(read.0, u64::MAX);
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IntegerString<Integer: WideInteger>(pub Integer);

impl<Integer: WideInteger> From<Integer> for IntegerString<Integer> {
	fn from(integer: Integer) -> Self {
		Self(integer)
	}
}

impl<Integer: WideInteger> fmt::Display for IntegerString<Integer> {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		self.0.fmt(formatter)
	}
}

impl<Integer: WideInteger> Serialize for IntegerString<Integer> {
	fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
		serializer.collect_str(&self.0)
	}
}

impl<'de, Integer: WideInteger> Deserialize<'de> for IntegerString<Integer> {
	fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
		struct TextVisitor<Integer>(std::marker::PhantomData<Integer>);

		impl<Integer: WideInteger> Visitor<'_> for TextVisitor<Integer> {
			type Value = IntegerString<Integer>;

			fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
				formatter.write_str("a string that holds an integer in decimal digits")
			}

			fn visit_str<E: serde::de::Error>(self, text: &str) -> Result<Self::Value, E> {
				// The message does not quote the text.
				text.parse().map(IntegerString).map_err(|_| {
					E::custom("the string is not an integer of the field's type in decimal digits")
				})
			}
		}

		deserializer.deserialize_str(TextVisitor(std::marker::PhantomData))
	}
}

impl<Integer: WideInteger> TsGen for IntegerString<Integer> {
	fn type_ref() -> TypeRef {
		TypeRef::String
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn every_wide_integer_type_round_trips_its_whole_range_as_decimal_text() {
		fn check<Integer>(values: &[Integer])
		where
			Integer: WideInteger + PartialEq + fmt::Debug,
		{
			for value in values {
				let text = serde_json::to_string(&IntegerString(*value)).expect("serialize");
				assert_eq!(text, format!("\"{value}\""));
				let read: IntegerString<Integer> = serde_json::from_str(&text).expect("read");
				assert_eq!(read.0, *value);
				assert_eq!(IntegerString(*value).to_string(), value.to_string());
			}
		}
		check(&[i64::MIN, -1, 0, 1, i64::MAX]);
		check(&[u64::MIN, 1, 9_007_199_254_740_993, u64::MAX]);
		check(&[i128::MIN, 0, i128::MAX]);
		check(&[u128::MIN, u128::MAX]);
		check(&[isize::MIN, 0, isize::MAX]);
		check(&[usize::MIN, usize::MAX]);
	}

	#[test]
	fn a_number_and_text_that_is_not_the_integers_type_are_rejected_without_the_text() {
		for json in [
			"7",
			"7.5",
			"null",
			"true",
			"\"\"",
			"\"seven\"",
			"\"7.0\"",
			"\" 7\"",
			"\"0x07\"",
			"\"-1\"",
			"\"18446744073709551616\"",
		] {
			let error = serde_json::from_str::<IntegerString<u64>>(json)
				.expect_err("not a u64 in decimal text");
			assert!(!error.to_string().contains("seven"), "{json}: {error}");
		}
		assert_eq!(
			serde_json::from_str::<IntegerString<i64>>("\"-1\"")
				.expect("a negative i64")
				.0,
			-1
		);
	}

	#[test]
	fn the_typescript_type_is_string() {
		assert_eq!(IntegerString::<u64>::type_ref(), TypeRef::String);
		assert_eq!(IntegerString::<i128>::type_ref(), TypeRef::String);
	}
}
