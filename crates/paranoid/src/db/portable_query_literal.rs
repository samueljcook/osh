//! Safe PostgreSQL literal encoding for [`portable_query`](super::portable_query).
//!
//! Implementations reuse [`SimpleQuery`](super::SimpleQuery) escaping.

use super::LiteralEncodingError;
use super::simple_query::{
	encode_bool_literal, encode_bytea_literal, encode_f64_literal, encode_i16_literal,
	encode_i32_literal, encode_i64_literal, encode_text_literal,
};
use primitives_rs::serialization::json::{
	self, JsonError, JsonFormat, JsonLimits, SerializeOptions,
};

const MAX_PORTABLE_QUERY_JSON_CONTAINER_DEPTH: usize = 100;

/// A Rust value that [`portable_query`](super::portable_query) can render as a safely-escaped
/// Postgres literal in place of a `$n` bind placeholder.
///
/// Encoding fails when a value has no supported PostgreSQL literal representation or
/// exceeds its encoding limits. A failed encoding prevents query execution.
///
/// A [`serde_json::Value`] is rendered as a `jsonb` literal and has at most 100 arrays or objects
/// along any path. A value with 101 or more fails to encode, which means that acceptance does not
/// depend on PostgreSQL's configurable stack depth.
///
/// An [`Option`] renders `None` as a `NULL` with the type that the same type's `Some` rendering
/// has: `NULL::pg_catalog.int8` for `i64`, with values rendered as `(n)::pg_catalog.int8`;
/// `NULL::pg_catalog.bool` for `bool`, with values rendered as `(TRUE)` and
/// `(FALSE)`; and an untyped `NULL` for text, with values rendered as untyped string constants. A
/// `None` is therefore valid wherever a `Some` of the same type is, such as text bound to an
/// enum column.
///
/// Applications cannot implement this trait, so every rendering keeps bound values out of the
/// SQL structure:
///
/// ```compile_fail
/// struct Unchecked;
///
/// impl paranoid::db::PortableQueryArgument for Unchecked {
///     fn encode_portable_query_literal(
///         &self,
///         out: &mut String,
///     ) -> Result<(), paranoid::db::LiteralEncodingError> {
///         out.push_str("1");
///         Ok(())
///     }
///
///     fn encode_portable_query_null(out: &mut String) {
///         out.push_str("NULL");
///     }
/// }
/// ```
pub trait PortableQueryArgument: sealed::Sealed {
	/// Appends `self`, rendered as a Postgres literal, to `out`.
	fn encode_portable_query_literal(&self, out: &mut String) -> Result<(), LiteralEncodingError>;

	/// Appends the `NULL` that stands for a missing value of this type to `out`, with the type
	/// that the rendering of this type's values has.
	fn encode_portable_query_null(out: &mut String);
}

mod sealed {
	pub trait Sealed {}

	impl<T: Sealed + ?Sized> Sealed for &T {}
	impl<T: Sealed> Sealed for Option<T> {}
	impl Sealed for bool {}
	impl Sealed for i16 {}
	impl Sealed for i32 {}
	impl Sealed for i64 {}
	impl Sealed for f64 {}
	impl Sealed for time::OffsetDateTime {}
	impl Sealed for time::Date {}
	impl Sealed for serde_json::Value {}
	#[cfg(feature = "uuid")]
	impl Sealed for uuid::Uuid {}
	impl Sealed for str {}
	impl Sealed for String {}
	impl Sealed for [u8] {}
	impl<const N: usize> Sealed for [u8; N] {}
	impl Sealed for Vec<u8> {}
	impl Sealed for [String] {}
	impl Sealed for Vec<String> {}
	impl Sealed for [Vec<u8>] {}
	impl Sealed for Vec<Vec<u8>> {}
	impl Sealed for [i64] {}
	impl Sealed for Vec<i64> {}
	impl Sealed for [Option<i64>] {}
	impl Sealed for Vec<Option<i64>> {}
}

/// Appends a `NULL` of `pg_type` to `out`.
fn encode_typed_null(pg_type: &str, out: &mut String) {
	out.push_str("NULL::");
	out.push_str(pg_type);
}

impl<T> PortableQueryArgument for &T
where
	T: PortableQueryArgument + ?Sized,
{
	fn encode_portable_query_literal(&self, out: &mut String) -> Result<(), LiteralEncodingError> {
		(**self).encode_portable_query_literal(out)
	}

	fn encode_portable_query_null(out: &mut String) {
		T::encode_portable_query_null(out);
	}
}

impl PortableQueryArgument for bool {
	fn encode_portable_query_literal(&self, out: &mut String) -> Result<(), LiteralEncodingError> {
		encode_bool_literal(*self, out);
		Ok(())
	}

	fn encode_portable_query_null(out: &mut String) {
		encode_typed_null("pg_catalog.bool", out);
	}
}

impl PortableQueryArgument for i16 {
	fn encode_portable_query_literal(&self, out: &mut String) -> Result<(), LiteralEncodingError> {
		encode_i16_literal(*self, out);
		Ok(())
	}

	fn encode_portable_query_null(out: &mut String) {
		encode_typed_null("pg_catalog.int2", out);
	}
}

impl PortableQueryArgument for i32 {
	fn encode_portable_query_literal(&self, out: &mut String) -> Result<(), LiteralEncodingError> {
		encode_i32_literal(*self, out);
		Ok(())
	}

	fn encode_portable_query_null(out: &mut String) {
		encode_typed_null("pg_catalog.int4", out);
	}
}

impl PortableQueryArgument for i64 {
	fn encode_portable_query_literal(&self, out: &mut String) -> Result<(), LiteralEncodingError> {
		encode_i64_literal(*self, out);
		Ok(())
	}

	fn encode_portable_query_null(out: &mut String) {
		encode_typed_null("pg_catalog.int8", out);
	}
}

impl PortableQueryArgument for f64 {
	fn encode_portable_query_literal(&self, out: &mut String) -> Result<(), LiteralEncodingError> {
		encode_f64_literal(*self, out);
		Ok(())
	}

	fn encode_portable_query_null(out: &mut String) {
		encode_typed_null("pg_catalog.float8", out);
	}
}

impl PortableQueryArgument for time::OffsetDateTime {
	fn encode_portable_query_literal(&self, out: &mut String) -> Result<(), LiteralEncodingError> {
		let formatted = self
			.format(&time::format_description::well_known::Rfc3339)
			.map_err(|_| {
				LiteralEncodingError::new(
					"time::OffsetDateTime could not be formatted as RFC 3339 text",
				)
			})?;
		encode_text_literal(&formatted, out)?;
		out.push_str("::pg_catalog.timestamptz");
		Ok(())
	}

	fn encode_portable_query_null(out: &mut String) {
		encode_typed_null("pg_catalog.timestamptz", out);
	}
}

impl PortableQueryArgument for time::Date {
	fn encode_portable_query_literal(&self, out: &mut String) -> Result<(), LiteralEncodingError> {
		let formatted = self
			.format(&time::format_description::well_known::Iso8601::DATE)
			.map_err(|_| {
				LiteralEncodingError::new("time::Date could not be formatted as ISO 8601 text")
			})?;
		encode_text_literal(&formatted, out)?;
		out.push_str("::pg_catalog.date");
		Ok(())
	}

	fn encode_portable_query_null(out: &mut String) {
		encode_typed_null("pg_catalog.date", out);
	}
}

impl PortableQueryArgument for serde_json::Value {
	fn encode_portable_query_literal(&self, out: &mut String) -> Result<(), LiteralEncodingError> {
		// PostgreSQL applies jsonb validation after serde_json escapes control
		// characters in the text representation.
		let formatted = json::serialize_to_vec(
			self,
			SerializeOptions {
				limits: JsonLimits {
					max_size: usize::MAX,
					max_depth: MAX_PORTABLE_QUERY_JSON_CONTAINER_DEPTH,
				},
				format: JsonFormat::Compact,
			},
		)
		.map_err(|error| match error {
			JsonError::TooDeep { .. } => LiteralEncodingError::new(
				"serde_json::Value nests more than 100 arrays or objects along a path",
			),
			_ => LiteralEncodingError::new("serde_json::Value could not be serialized"),
		})?;
		let formatted = String::from_utf8(formatted).expect("serde_json writes UTF-8");
		encode_text_literal(&formatted, out)?;
		out.push_str("::pg_catalog.jsonb");
		Ok(())
	}

	fn encode_portable_query_null(out: &mut String) {
		encode_typed_null("pg_catalog.jsonb", out);
	}
}

#[cfg(feature = "uuid")]
impl PortableQueryArgument for uuid::Uuid {
	fn encode_portable_query_literal(&self, out: &mut String) -> Result<(), LiteralEncodingError> {
		encode_text_literal(&self.hyphenated().to_string(), out)?;
		out.push_str("::pg_catalog.uuid");
		Ok(())
	}

	fn encode_portable_query_null(out: &mut String) {
		encode_typed_null("pg_catalog.uuid", out);
	}
}

impl PortableQueryArgument for str {
	fn encode_portable_query_literal(&self, out: &mut String) -> Result<(), LiteralEncodingError> {
		encode_text_literal(self, out)
	}

	/// Text renders as an untyped string constant, so its `NULL` is untyped too.
	fn encode_portable_query_null(out: &mut String) {
		out.push_str("NULL");
	}
}

impl PortableQueryArgument for String {
	fn encode_portable_query_literal(&self, out: &mut String) -> Result<(), LiteralEncodingError> {
		self.as_str().encode_portable_query_literal(out)
	}

	fn encode_portable_query_null(out: &mut String) {
		str::encode_portable_query_null(out);
	}
}

impl PortableQueryArgument for [u8] {
	fn encode_portable_query_literal(&self, out: &mut String) -> Result<(), LiteralEncodingError> {
		encode_bytea_literal(self, out);
		Ok(())
	}

	fn encode_portable_query_null(out: &mut String) {
		encode_typed_null("pg_catalog.bytea", out);
	}
}

impl<const N: usize> PortableQueryArgument for [u8; N] {
	fn encode_portable_query_literal(&self, out: &mut String) -> Result<(), LiteralEncodingError> {
		self.as_slice().encode_portable_query_literal(out)
	}

	fn encode_portable_query_null(out: &mut String) {
		<[u8]>::encode_portable_query_null(out);
	}
}

impl PortableQueryArgument for Vec<u8> {
	fn encode_portable_query_literal(&self, out: &mut String) -> Result<(), LiteralEncodingError> {
		self.as_slice().encode_portable_query_literal(out)
	}

	fn encode_portable_query_null(out: &mut String) {
		<[u8]>::encode_portable_query_null(out);
	}
}

impl<T> PortableQueryArgument for Option<T>
where
	T: PortableQueryArgument,
{
	fn encode_portable_query_literal(&self, out: &mut String) -> Result<(), LiteralEncodingError> {
		match self {
			Some(value) => value.encode_portable_query_literal(out),
			None => {
				T::encode_portable_query_null(out);
				Ok(())
			}
		}
	}

	fn encode_portable_query_null(out: &mut String) {
		T::encode_portable_query_null(out);
	}
}

fn encode_array_literal<T>(
	elements: &[T],
	pg_array_type: &str,
	encode_element: impl Fn(&T, &mut String) -> Result<(), LiteralEncodingError>,
	out: &mut String,
) -> Result<(), LiteralEncodingError> {
	out.push_str("ARRAY[");
	for (index, element) in elements.iter().enumerate() {
		if index > 0 {
			out.push(',');
		}
		encode_element(element, out)?;
	}
	out.push_str("]::");
	out.push_str(pg_array_type);
	Ok(())
}

impl PortableQueryArgument for [String] {
	fn encode_portable_query_literal(&self, out: &mut String) -> Result<(), LiteralEncodingError> {
		encode_array_literal(
			self,
			"pg_catalog.text[]",
			|element, out| element.as_str().encode_portable_query_literal(out),
			out,
		)
	}

	fn encode_portable_query_null(out: &mut String) {
		encode_typed_null("pg_catalog.text[]", out);
	}
}

impl PortableQueryArgument for Vec<String> {
	fn encode_portable_query_literal(&self, out: &mut String) -> Result<(), LiteralEncodingError> {
		self.as_slice().encode_portable_query_literal(out)
	}

	fn encode_portable_query_null(out: &mut String) {
		<[String]>::encode_portable_query_null(out);
	}
}

impl PortableQueryArgument for [Vec<u8>] {
	fn encode_portable_query_literal(&self, out: &mut String) -> Result<(), LiteralEncodingError> {
		encode_array_literal(
			self,
			"pg_catalog.bytea[]",
			|element, out| element.as_slice().encode_portable_query_literal(out),
			out,
		)
	}

	fn encode_portable_query_null(out: &mut String) {
		encode_typed_null("pg_catalog.bytea[]", out);
	}
}

impl PortableQueryArgument for Vec<Vec<u8>> {
	fn encode_portable_query_literal(&self, out: &mut String) -> Result<(), LiteralEncodingError> {
		self.as_slice().encode_portable_query_literal(out)
	}

	fn encode_portable_query_null(out: &mut String) {
		<[Vec<u8>]>::encode_portable_query_null(out);
	}
}

impl PortableQueryArgument for [i64] {
	fn encode_portable_query_literal(&self, out: &mut String) -> Result<(), LiteralEncodingError> {
		encode_array_literal(
			self,
			"pg_catalog.int8[]",
			|element, out| element.encode_portable_query_literal(out),
			out,
		)
	}

	fn encode_portable_query_null(out: &mut String) {
		encode_typed_null("pg_catalog.int8[]", out);
	}
}

impl PortableQueryArgument for Vec<i64> {
	fn encode_portable_query_literal(&self, out: &mut String) -> Result<(), LiteralEncodingError> {
		self.as_slice().encode_portable_query_literal(out)
	}

	fn encode_portable_query_null(out: &mut String) {
		<[i64]>::encode_portable_query_null(out);
	}
}

impl PortableQueryArgument for [Option<i64>] {
	fn encode_portable_query_literal(&self, out: &mut String) -> Result<(), LiteralEncodingError> {
		encode_array_literal(
			self,
			"pg_catalog.int8[]",
			|element, out| element.encode_portable_query_literal(out),
			out,
		)
	}

	fn encode_portable_query_null(out: &mut String) {
		encode_typed_null("pg_catalog.int8[]", out);
	}
}

impl PortableQueryArgument for Vec<Option<i64>> {
	fn encode_portable_query_literal(&self, out: &mut String) -> Result<(), LiteralEncodingError> {
		self.as_slice().encode_portable_query_literal(out)
	}

	fn encode_portable_query_null(out: &mut String) {
		<[Option<i64>]>::encode_portable_query_null(out);
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn encode(value: impl PortableQueryArgument) -> Result<String, LiteralEncodingError> {
		let mut out = String::new();
		value.encode_portable_query_literal(&mut out)?;
		Ok(out)
	}

	#[test]
	fn scalars_encode_to_expected_literals() {
		assert_eq!(encode(true).unwrap(), "(TRUE)");
		assert_eq!(encode(false).unwrap(), "(FALSE)");
		assert_eq!(encode(1_i16).unwrap(), "(1)::pg_catalog.int2");
		assert_eq!(encode(1_i32).unwrap(), "(1)::pg_catalog.int4");
		assert_eq!(encode(1_i64).unwrap(), "(1)::pg_catalog.int8");
		assert_eq!(encode(1.5_f64).unwrap(), "(1.5::pg_catalog.float8)");
		assert_eq!(encode("theme").unwrap(), "E'theme'");
		assert_eq!(encode("theme".to_owned()).unwrap(), "E'theme'");
		assert_eq!(
			encode(b"bytes".as_slice()).unwrap(),
			"E'\\\\x6279746573'::pg_catalog.bytea"
		);
		assert_eq!(
			encode(vec![1u8, 2, 3]).unwrap(),
			"E'\\\\x010203'::pg_catalog.bytea"
		);
	}

	#[test]
	fn date_encodes_as_iso_8601_text_cast_to_date() {
		let date = time::Date::from_calendar_date(2026, time::Month::July, 8).unwrap();
		assert_eq!(encode(date).unwrap(), "E'2026-07-08'::pg_catalog.date");
		assert_eq!(
			encode(Some(date)).unwrap(),
			"E'2026-07-08'::pg_catalog.date"
		);
		assert_eq!(encode(None::<time::Date>).unwrap(), "NULL::pg_catalog.date");
	}

	#[test]
	fn json_encodes_compact_text_cast_to_jsonb_with_full_escaping() {
		let value = serde_json::json!({"key": "it's \"quoted\"", "n": 1});
		let encoded = encode(&value).unwrap();
		assert!(encoded.starts_with("E'"), "{encoded}");
		assert!(encoded.ends_with("'::pg_catalog.jsonb"), "{encoded}");
		assert!(encoded.contains("it''s"), "{encoded}");
		assert!(!encoded.contains(r"it\'s"), "{encoded}");
		let with_nul = serde_json::json!({"k": "a\u{0}b"});
		let encoded = encode(&with_nul).unwrap();
		assert!(!encoded.contains('\u{0}'));
		assert!(encoded.contains("\\u0000"), "{encoded}");
	}

	#[cfg(feature = "uuid")]
	#[test]
	fn uuid_encodes_as_hyphenated_text_cast_to_uuid() {
		let id = uuid::Uuid::parse_str("67e55044-10b1-426f-9247-bb680e5fe0c8").unwrap();
		assert_eq!(
			encode(id).unwrap(),
			"E'67e55044-10b1-426f-9247-bb680e5fe0c8'::pg_catalog.uuid"
		);
	}

	#[test]
	fn negative_scalar_extremes_do_not_overflow_before_cast() {
		assert_eq!(encode(i16::MIN).unwrap(), "(-32768)::pg_catalog.int2");
		assert_eq!(encode(i32::MIN).unwrap(), "(-2147483648)::pg_catalog.int4");
		assert_eq!(
			encode(i64::MIN).unwrap(),
			"(-9223372036854775808)::pg_catalog.int8"
		);
	}

	#[test]
	fn option_none_renders_as_null_and_some_delegates() {
		assert_eq!(
			encode(Option::<i64>::None).unwrap(),
			"NULL::pg_catalog.int8"
		);
		assert_eq!(encode(Some(1_i64)).unwrap(), "(1)::pg_catalog.int8");
	}

	#[test]
	fn none_renders_a_null_of_the_type_of_the_values() {
		fn null_of<T: PortableQueryArgument>() -> String {
			encode(None::<T>).unwrap()
		}
		assert_eq!(null_of::<bool>(), "NULL::pg_catalog.bool");
		assert_eq!(null_of::<i16>(), "NULL::pg_catalog.int2");
		assert_eq!(null_of::<i32>(), "NULL::pg_catalog.int4");
		assert_eq!(null_of::<f64>(), "NULL::pg_catalog.float8");
		assert_eq!(
			null_of::<time::OffsetDateTime>(),
			"NULL::pg_catalog.timestamptz"
		);
		assert_eq!(null_of::<serde_json::Value>(), "NULL::pg_catalog.jsonb");
		assert_eq!(null_of::<&str>(), "NULL");
		assert_eq!(null_of::<String>(), "NULL");
		assert_eq!(null_of::<&[u8]>(), "NULL::pg_catalog.bytea");
		assert_eq!(null_of::<[u8; 32]>(), "NULL::pg_catalog.bytea");
		assert_eq!(null_of::<Vec<u8>>(), "NULL::pg_catalog.bytea");
		assert_eq!(null_of::<Vec<String>>(), "NULL::pg_catalog.text[]");
		assert_eq!(null_of::<Vec<Vec<u8>>>(), "NULL::pg_catalog.bytea[]");
		assert_eq!(null_of::<Vec<i64>>(), "NULL::pg_catalog.int8[]");
		assert_eq!(null_of::<Vec<Option<i64>>>(), "NULL::pg_catalog.int8[]");
		assert_eq!(null_of::<Option<i64>>(), "NULL::pg_catalog.int8");
		assert_eq!(
			encode(vec![Some(1_i64), None]).unwrap(),
			"ARRAY[(1)::pg_catalog.int8,NULL::pg_catalog.int8]::pg_catalog.int8[]"
		);
	}

	/// The type of a value's rendering is its last `::` annotation, or none for an untyped
	/// string constant.
	fn rendered_annotation(rendering: &str) -> Option<&str> {
		rendering
			.trim_end_matches(')')
			.rsplit_once("::")
			.map(|(_, annotation)| annotation)
	}

	fn assert_none_has_the_annotation_of_some<T: PortableQueryArgument>(value: T) {
		let some = encode(Some(value)).unwrap();
		let none = encode(None::<T>).unwrap();
		match rendered_annotation(&some) {
			Some(annotation) => assert_eq!(none, format!("NULL::{annotation}"), "{some}"),
			None => assert_eq!(none, "NULL", "{some}"),
		}
	}

	#[test]
	fn none_has_the_annotation_of_the_rendering_of_some() {
		assert_none_has_the_annotation_of_some(-1_i16);
		assert_none_has_the_annotation_of_some(-1_i32);
		assert_none_has_the_annotation_of_some(-1_i64);
		assert_none_has_the_annotation_of_some(-1.5_f64);
		assert_none_has_the_annotation_of_some(f64::NAN);
		assert_none_has_the_annotation_of_some(
			time::Date::from_calendar_date(2026, time::Month::July, 8).unwrap(),
		);
		assert_none_has_the_annotation_of_some(time::OffsetDateTime::UNIX_EPOCH);
		assert_none_has_the_annotation_of_some(serde_json::json!({"key": 1}));
		assert_none_has_the_annotation_of_some("text");
		assert_none_has_the_annotation_of_some("text".to_owned());
		assert_none_has_the_annotation_of_some(vec![1_u8]);
		assert_none_has_the_annotation_of_some([1_u8; 4]);
		assert_none_has_the_annotation_of_some(vec!["a".to_owned()]);
		assert_none_has_the_annotation_of_some(vec![vec![1_u8]]);
		assert_none_has_the_annotation_of_some(vec![1_i64]);
		assert_none_has_the_annotation_of_some(vec![Some(1_i64), None]);
		// `TRUE` and `FALSE` are constants of type boolean without an annotation.
		assert_eq!(encode(Some(true)).unwrap(), "(TRUE)");
		assert_eq!(encode(None::<bool>).unwrap(), "NULL::pg_catalog.bool");
	}

	#[test]
	#[allow(
		clippy::needless_borrows_for_generic_args,
		reason = "deliberately passes a reference to prove the blanket &T impl delegates \
                  to the owned impl, not merely that the owned value itself encodes"
	)]
	fn references_delegate_to_owned_impls() {
		let value = 1_i64;
		assert_eq!(encode(&value).unwrap(), "(1)::pg_catalog.int8");
		let text = "theme".to_owned();
		assert_eq!(encode(&text).unwrap(), "E'theme'");
	}

	#[test]
	fn text_array_encodes_as_text_array() {
		let values = vec!["a".to_owned(), "b".to_owned()];
		assert_eq!(
			encode(values.clone()).unwrap(),
			"ARRAY[E'a',E'b']::pg_catalog.text[]"
		);
		assert_eq!(
			encode(values.as_slice()).unwrap(),
			"ARRAY[E'a',E'b']::pg_catalog.text[]"
		);
		assert_eq!(
			encode(Vec::<String>::new()).unwrap(),
			"ARRAY[]::pg_catalog.text[]"
		);
	}

	#[test]
	fn bytea_array_encodes_as_bytea_array() {
		let values = vec![vec![1u8], vec![2u8]];
		assert_eq!(
			encode(values).unwrap(),
			"ARRAY[E'\\\\x01'::pg_catalog.bytea,E'\\\\x02'::pg_catalog.bytea]::pg_catalog.bytea[]"
		);
	}

	#[test]
	fn bigint_array_encodes_with_per_element_cast() {
		let values: Vec<i64> = vec![1, 2];
		assert_eq!(
			encode(values).unwrap(),
			"ARRAY[(1)::pg_catalog.int8,(2)::pg_catalog.int8]::pg_catalog.int8[]"
		);
	}

	#[test]
	fn nul_byte_in_text_is_rejected() {
		assert!(encode("a\0b").is_err());
		let values = vec!["a\0b".to_owned()];
		assert!(encode(values).is_err());
	}

	#[test]
	fn offset_date_time_encodes_as_rfc3339_timestamptz() {
		let date = time::Date::from_calendar_date(2026, time::Month::July, 8).unwrap();
		let time_of_day = time::Time::from_hms_milli(12, 34, 56, 789).unwrap();
		let value = date.with_time(time_of_day).assume_utc();
		assert_eq!(
			encode(value).unwrap(),
			"E'2026-07-08T12:34:56.789Z'::pg_catalog.timestamptz"
		);
	}

	#[test]
	fn offset_date_time_preserves_non_utc_offset() {
		let date = time::Date::from_calendar_date(2026, time::Month::July, 8).unwrap();
		let time_of_day = time::Time::from_hms(12, 34, 56).unwrap();
		let offset = time::UtcOffset::from_hms(-5, 0, 0).unwrap();
		let value = date.with_time(time_of_day).assume_offset(offset);
		assert_eq!(
			encode(value).unwrap(),
			"E'2026-07-08T12:34:56-05:00'::pg_catalog.timestamptz"
		);
	}

	#[test]
	fn offset_date_time_keeps_nanos_and_the_largest_whole_minute_offset() {
		let date = time::Date::from_calendar_date(2020, time::Month::February, 29).unwrap();
		let time_of_day = time::Time::from_hms_nano(23, 59, 59, 123_456_789).unwrap();
		let offset = time::UtcOffset::from_hms(23, 59, 0).unwrap();
		let value = date.with_time(time_of_day).assume_offset(offset);
		assert_eq!(
			encode(value).unwrap(),
			"E'2020-02-29T23:59:59.123456789+23:59'::pg_catalog.timestamptz"
		);
	}

	#[test]
	fn an_offset_that_rfc_3339_cannot_write_is_rejected_instead_of_rounded() {
		let date = time::Date::from_calendar_date(2021, time::Month::January, 1).unwrap();
		let offset = time::UtcOffset::from_hms(0, 0, 30).unwrap();
		let value = date.with_time(time::Time::MIDNIGHT).assume_offset(offset);
		assert!(encode(value).is_err());
	}
}
