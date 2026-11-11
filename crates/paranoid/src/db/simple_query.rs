//! Literal-bound Postgres queries that are safe under **every** connection-pooler mode,
//! including statement-mode pooling, by rendering bound values as escaped SQL literals and
//! sending a single simple-protocol `Query` message.
//!
//! [`portable_query`](super::portable_query) provides the same transport guarantee while
//! preserving familiar `$1`, `$2`, ... template placeholders and fluent `.bind()` calls.
//! This builder is the lower-level composition surface for callers that instead want to
//! interleave explicitly audited SQL fragments with typed literals and do not need
//! placeholder parsing. Both APIs render a parameter-free [`RawSql`] and send one
//! simple-protocol `Query`, so neither has a `Parse`/`Bind` split for a statement-mode
//! pooler to separate across backends.
//!
//! # Injection safety
//!
//! Values are rendered as literals, so encoding correctness *is* injection safety. Every
//! textual value is emitted with Postgres escape-string syntax (`E'...'`), writing `\` as `\\`
//! and `'` as `''`, which is deterministic regardless of the session's
//! `standard_conforming_strings` and `backslash_quote` settings. Byte strings are emitted as `E'\\x<hex>'::pg_catalog.bytea` (hex is `[0-9a-f]` only).
//! Numerics are Rust-typed and rendered without quotes, except that `NaN`/`±∞` use Postgres's
//! `'NaN'` / `'Infinity'` float literals. Every cast names its type as `pg_catalog.<type>`, so a
//! session's `search_path` cannot change which type a literal has. Arrays render with the
//! `ARRAY[...]` constructor so each
//! element flows back through this same encoder, and any type without a dedicated variant
//! (`numeric`, `uuid`, `timestamptz`, `jsonb`, enums, domains, …) goes through
//! [`PgLiteral::TextCast`], where the value text is escaped identically. A cast target
//! type name is *structure*, so — like every other SQL fragment that this builder accepts — it is
//! an [`AuditedSql`](super::AuditedSql), never interpolated from a runtime string. The *only*
//! thing that this encoder rejects is a NUL byte in text, which Postgres text cannot store.
//! There is deliberately no `$n` placeholder parsing.
//!
//! # Joins
//!
//! [`SimpleQuery`] writes one space between each piece that it appends, a fragment or a
//! literal, and the piece before it. Every fragment ends in SQL code because
//! [`audited_sql!`](crate::audited_sql) rejects a template that ends inside a string constant,
//! a quoted identifier, a dollar-quoted string, or a comment. A space ends every other token,
//! so the last token of one piece never continues into the first token of the next. The one
//! join that a space does not prevent is PostgreSQL's continuation of a string constant: a text
//! literal and a following fragment that starts with white space that contains a line break,
//! and then a string constant, form one string constant. The literal's quotes are doubled, and
//! the fragment's constant contains no backslash, so that constant ends at the fragment's
//! closing quote for every value.
//!
//! The part of a literal that varies with the value stays inside one closed group: the
//! parentheses of a number, of a `bool` (`(TRUE)` or `(FALSE)`), and of [`PgLiteral::Null`]
//! (`(NULL)`), the string constant of a text, a byte string, or a cast value, and the brackets
//! of an array. The text that follows the group is the same for every value, so the value
//! changes no token outside the group. Every literal is an expression: where PostgreSQL's
//! grammar takes a column label or a keyword instead, such as after `.` or `AS`, the literal is
//! a syntax error for every value. Inside a literal, the type fragment of [`PgLiteral::TextCast`] or
//! [`PgLiteral::Array`] follows `::` and precedes `[]`, `,`, `]`, or the end of the literal.
//! No neighboring token continues into these tokens, and no fragment ends inside a constant, an
//! identifier, or a comment, so the type fragment joins no other token either.

use std::fmt::Write as _;

use sqlx::RawSql;

use super::AuditedSql;
use super::LiteralEncodingError;

/// A typed value to be rendered as a safe Postgres literal by [`SimpleQuery`].
#[derive(Clone, Debug, PartialEq)]
pub enum PgLiteral<'a> {
	/// SQL `NULL`, rendered as `(NULL)`.
	Null,
	/// `boolean`, rendered as `(TRUE)` or `(FALSE)`.
	Bool(bool),
	/// `smallint`.
	I16(i16),
	/// `integer`.
	I32(i32),
	/// `bigint`.
	I64(i64),
	/// `double precision`. `NaN` and `±∞` are rendered as Postgres's special float literals
	/// (`'NaN'`, `'Infinity'`, `'-Infinity'`), which it stores natively. Every value renders as
	/// one parenthesized expression.
	F64(f64),
	/// `text`.
	Text(&'a str),
	/// `bytea`.
	Bytea(&'a [u8]),
	/// A textual value cast to a named Postgres type, e.g. `uuid`, `timestamptz`,
	/// `jsonb`, `numeric`, an enum, or a domain. `value` is escaped exactly like
	/// [`PgLiteral::Text`]; Postgres validates it at cast time. `pg_type` is an
	/// [`AuditedSql`] type name, so any valid form works — multi-word
	/// (`pg_catalog.timestamptz`), array (`pg_catalog.int4[]`), schema-qualified, or quoted.
	/// Qualify built-in types with `pg_catalog` so that the session's `search_path` cannot
	/// change the type. Use for any type without a dedicated variant above.
	TextCast {
		/// The textual value; escaped exactly like [`PgLiteral::Text`].
		value: &'a str,
		/// The Postgres type to cast to, as an [`AuditedSql`] fragment.
		pg_type: AuditedSql,
	},
	/// A Postgres array, rendered with the `ARRAY[...]` constructor so each element is
	/// encoded by this same safe encoder — there are no array-literal quoting rules for a
	/// caller to get wrong. The value is cast to `element_pg_type` + `[]`, which also makes
	/// empty and all-`NULL` arrays unambiguous. Elements may themselves be
	/// [`PgLiteral::Array`] to construct a multidimensional Postgres array
	/// (Postgres itself accepts and stores this correctly), but Postgres requires every
	/// nested sub-array at a given nesting depth to have the same length; a mismatched
	/// nested length is rejected by Postgres at execution time with SQLSTATE `2202E`
	/// (`array_subscript_error`), not silently truncated or reshaped. Note for callers who
	/// intend to read such a value back through sqlx: sqlx has no `PgHasArrayType`
	/// implementation for `Vec<T>`, so a multidimensional array built this way cannot be
	/// decoded as `Vec<Vec<T>>` through sqlx's typed row access; reading it back (for
	/// example via a `::text` cast, or `unnest`) requires a different query shape than a
	/// same-typed insert.
	Array {
		/// The array elements, each encoded as a nested literal.
		elements: &'a [PgLiteral<'a>],
		/// The element type name, as an [`AuditedSql`] fragment (e.g. `pg_catalog.int4`,
		/// `pg_catalog.text`, `pg_catalog.timestamptz`).
		element_pg_type: AuditedSql,
	},
}

impl PgLiteral<'_> {
	fn encode_into(&self, out: &mut String) -> Result<(), LiteralEncodingError> {
		match self {
			Self::Null => out.push_str("(NULL)"),
			Self::Bool(value) => encode_bool_literal(*value, out),
			Self::I16(value) => encode_i16_literal(*value, out),
			Self::I32(value) => encode_i32_literal(*value, out),
			Self::I64(value) => encode_i64_literal(*value, out),
			Self::F64(value) => encode_f64_literal(*value, out),
			Self::Text(value) => encode_text_literal(value, out)?,
			Self::Bytea(value) => encode_bytea_literal(value, out),
			Self::TextCast { value, pg_type } => {
				encode_text_literal(value, out)?;
				out.push_str("::");
				out.push_str(pg_type.as_str());
			}
			Self::Array {
				elements,
				element_pg_type,
			} => {
				out.push_str("ARRAY[");
				for (index, element) in elements.iter().enumerate() {
					if index > 0 {
						out.push(',');
					}
					element.encode_into(out)?;
				}
				out.push_str("]::");
				out.push_str(element_pg_type.as_str());
				out.push_str("[]");
			}
		}
		Ok(())
	}
}

/// Emits `value` as `(TRUE)` or `(FALSE)`. The parentheses make the literal an expression for
/// PostgreSQL's grammar so that it can never be a column label: the keyword `TRUE` alone is the
/// label "true" after `.`, after `AS`, and after an output column's expression. Shared with
/// [`portable_query`](super::portable_query)'s literal encoder and the `bool` arguments of
/// [`audited_sql!`](crate::audited_sql) so that every surface renders `boolean` values
/// identically.
pub(crate) fn encode_bool_literal(value: bool, out: &mut String) {
	out.push_str(if value { "(TRUE)" } else { "(FALSE)" });
}

// Parenthesized: Postgres's `::` cast binds tighter than unary minus, so an unparenthesized
// negative literal like `-32768::smallint` parses as `-(32768::smallint)` and overflows
// before negation is ever applied. Shared with `portable_query`'s literal encoder so both
// surfaces render integers identically.

/// Emits `value` as `(value)::pg_catalog.int2`.
pub(crate) fn encode_i16_literal(value: i16, out: &mut String) {
	write!(out, "({value})::pg_catalog.int2").expect("write to String is infallible");
}

/// Emits `value` as `(value)::pg_catalog.int4`.
pub(crate) fn encode_i32_literal(value: i32, out: &mut String) {
	write!(out, "({value})::pg_catalog.int4").expect("write to String is infallible");
}

/// Emits `value` as `(value)::pg_catalog.int8`.
pub(crate) fn encode_i64_literal(value: i64, out: &mut String) {
	write!(out, "({value})::pg_catalog.int8").expect("write to String is infallible");
}

/// Emits a finite `value` as `(value::pg_catalog.float8)`, and `NaN`, `+∞`, and `-∞` as
/// Postgres's special float literals in the same form: `('NaN'::pg_catalog.float8)`,
/// `('Infinity'::pg_catalog.float8)`, and `('-Infinity'::pg_catalog.float8)`, which Postgres
/// stores natively. Every value therefore renders as one parenthesized expression, so the token
/// that follows the literal never joins the type name, and the minus sign of a negative value
/// never joins a `-` that precedes the literal into `--`, which starts a SQL comment. The cast is
/// inside the parentheses because PostgreSQL applies a cast before a unary minus:
/// `-0.0::pg_catalog.float8` negates the `float8` zero and keeps negative zero, while
/// `(-0.0)::pg_catalog.float8` negates a `numeric` zero, which has no sign.
/// Shared with [`portable_query`](super::portable_query)'s literal encoder so both surfaces
/// render `double precision` values identically.
pub(crate) fn encode_f64_literal(value: f64, out: &mut String) {
	// `{:?}` gives a round-trippable decimal for finite values (e.g. `1.0`, not `1`).
	if value.is_nan() {
		out.push_str("('NaN'::pg_catalog.float8)");
	} else if value == f64::INFINITY {
		out.push_str("('Infinity'::pg_catalog.float8)");
	} else if value == f64::NEG_INFINITY {
		out.push_str("('-Infinity'::pg_catalog.float8)");
	} else {
		write!(out, "({value:?}::pg_catalog.float8)").expect("write to String is infallible");
	}
}

/// Emits `value` as a Postgres escape-string literal (`E'...'`), writing `\` as `\\` and `'` as
/// `''`.
///
/// Escape-string syntax has fixed escaping rules independent of `standard_conforming_strings`.
/// A doubled quote is accepted under every `backslash_quote` setting, whereas that setting
/// rejects `\'` in string literals. The result is therefore safe regardless of those session
/// settings. NUL is rejected because Postgres
/// text cannot contain it. Shared with
/// [`portable_query`](super::portable_query)'s literal encoder so both surfaces render
/// `text` values identically.
pub(crate) fn encode_text_literal(
	value: &str,
	out: &mut String,
) -> Result<(), LiteralEncodingError> {
	if value.as_bytes().contains(&0) {
		return Err(LiteralEncodingError::new(
			"text value contains a NUL byte, which Postgres text cannot represent",
		));
	}
	out.push_str("E'");
	for character in value.chars() {
		match character {
			'\\' => out.push_str("\\\\"),
			'\'' => out.push_str("''"),
			other => out.push(other),
		}
	}
	out.push('\'');
	Ok(())
}

/// Emits `bytes` as `E'\x<hex>'::pg_catalog.bytea`. The hex payload is `[0-9a-f]` only, so no
/// escaping of the value is required; the `E'\\x` prefix is escape-string-safe under any
/// `standard_conforming_strings` setting. Shared with [`portable_query`](super::portable_query)'s
/// literal encoder so both surfaces render `bytea` values identically.
pub(crate) fn encode_bytea_literal(bytes: &[u8], out: &mut String) {
	out.push_str("E'\\\\x");
	for byte in bytes {
		write!(out, "{byte:02x}").expect("write to String is infallible");
	}
	out.push_str("'::pg_catalog.bytea");
}

/// Builds a pooler-safe simple-protocol query by interleaving audited SQL fragments with
/// safely-encoded literal values.
///
/// SQL structure comes only from [`AuditedSql`] fragments. The part of each literal that varies
/// with the value is one closed group, and PostgreSQL reads every literal as an expression, so
/// the value of a literal never changes the statement's structure. The builder writes one space between each appended piece and the
/// piece before it so that no token of one piece continues into the next; the module
/// documentation states the rules of these joins. The result is a single [`RawSql`] with no
/// bind parameters.
///
/// ```
/// # use paranoid::db::{SimpleQuery, PgLiteral};
/// let query = SimpleQuery::new()
///     .push_sql(paranoid::audited_sql!("SELECT value FROM app_settings WHERE key ="))
///     .push_literal(PgLiteral::Text("theme"))
///     .and_then(|q| q.push_sql(paranoid::audited_sql!("AND tenant =")).push_literal(PgLiteral::I64(7)))
///     .expect("valid literals");
/// assert_eq!(
///     query.as_sql(),
///     "SELECT value FROM app_settings WHERE key = E'theme' AND tenant = (7)::pg_catalog.int8"
/// );
/// let _raw = query.into_raw_sql();
/// ```
#[derive(Clone, Debug, Default)]
pub struct SimpleQuery {
	sql: String,
}

impl SimpleQuery {
	/// Starts an empty query.
	pub fn new() -> Self {
		Self::default()
	}

	/// Appends an audited SQL fragment (structure, identifiers, operators), after one space when
	/// the query already has text.
	pub fn push_sql(mut self, sql: AuditedSql) -> Self {
		self.separate();
		self.sql.push_str(sql.as_str());
		self
	}

	/// Appends a value rendered as a safe Postgres literal, after one space when the query
	/// already has text.
	///
	/// Returns an error only when the value cannot be represented at all — a NUL byte in text,
	/// which Postgres text cannot store — rather than emitting anything unsafe.
	pub fn push_literal(mut self, literal: PgLiteral<'_>) -> Result<Self, LiteralEncodingError> {
		self.separate();
		literal.encode_into(&mut self.sql)?;
		Ok(self)
	}

	/// Writes the space that separates the next piece from the text before it.
	fn separate(&mut self) {
		if !self.sql.is_empty() {
			self.sql.push(' ');
		}
	}

	/// Returns the assembled SQL text (primarily for tests and diagnostics).
	pub fn as_sql(&self) -> &str {
		&self.sql
	}

	/// Finishes the builder into a simple-protocol [`RawSql`] with no bind parameters.
	///
	/// Uses [`unparameterized_simple_query`](super::unparameterized_simple_query) because the
	/// rendered SQL has no bind parameters.
	pub fn into_raw_sql(self) -> RawSql {
		super::unparameterized_simple_query(sqlx::AssertSqlSafe(self.sql))
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use proptest::prelude::*;

	fn encode(literal: PgLiteral<'_>) -> Result<String, LiteralEncodingError> {
		Ok(SimpleQuery::new()
			.push_literal(literal)?
			.as_sql()
			.to_owned())
	}

	#[test]
	fn scalars_encode_to_expected_literals() {
		assert_eq!(encode(PgLiteral::Null).unwrap(), "(NULL)");
		assert_eq!(encode(PgLiteral::Bool(true)).unwrap(), "(TRUE)");
		assert_eq!(encode(PgLiteral::Bool(false)).unwrap(), "(FALSE)");
		assert_eq!(encode(PgLiteral::I16(-5)).unwrap(), "(-5)::pg_catalog.int2");
		assert_eq!(encode(PgLiteral::I32(0)).unwrap(), "(0)::pg_catalog.int4");
		assert_eq!(
			encode(PgLiteral::I64(9_223_372_036_854_775_807)).unwrap(),
			"(9223372036854775807)::pg_catalog.int8"
		);
	}

	#[test]
	fn bool_and_null_render_as_one_parenthesized_group() {
		for literal in [
			PgLiteral::Bool(true),
			PgLiteral::Bool(false),
			PgLiteral::Null,
		] {
			let rendered = encode(literal.clone()).unwrap();
			assert!(
				rendered.starts_with('(') && rendered.ends_with(')'),
				"{literal:?} renders as {rendered}"
			);
		}
	}

	#[test]
	fn text_uses_escape_string_syntax_and_escapes_quote_and_backslash() {
		assert_eq!(encode(PgLiteral::Text("theme")).unwrap(), "E'theme'");
		assert_eq!(
			encode(PgLiteral::Text("'; DROP TABLE users; --")).unwrap(),
			"E'''; DROP TABLE users; --'"
		);
		assert_eq!(
			encode(PgLiteral::Text("back\\slash")).unwrap(),
			"E'back\\\\slash'"
		);
	}

	#[test]
	fn text_with_nul_is_rejected() {
		assert!(encode(PgLiteral::Text("a\0b")).is_err());
	}

	#[test]
	fn text_cast_with_nul_is_rejected() {
		assert!(
			encode(PgLiteral::TextCast {
				value: "a\0b",
				pg_type: crate::audited_sql!("uuid"),
			})
			.is_err()
		);
	}

	#[test]
	fn array_element_nul_is_rejected_without_producing_partial_output() {
		let result = SimpleQuery::new().push_literal(PgLiteral::Array {
			elements: &[PgLiteral::Text("ok"), PgLiteral::Text("a\0b")],
			element_pg_type: crate::audited_sql!("text"),
		});
		assert!(result.is_err());
	}

	#[test]
	fn bytea_encodes_as_hex_escape_string() {
		assert_eq!(
			encode(PgLiteral::Bytea(&[0x00, 0xde, 0xad, 0xff])).unwrap(),
			"E'\\\\x00deadff'::pg_catalog.bytea"
		);
		assert_eq!(
			encode(PgLiteral::Bytea(&[])).unwrap(),
			"E'\\\\x'::pg_catalog.bytea"
		);
	}

	#[test]
	fn non_finite_floats_encode_as_special_literals() {
		assert_eq!(
			encode(PgLiteral::F64(f64::NAN)).unwrap(),
			"('NaN'::pg_catalog.float8)"
		);
		assert_eq!(
			encode(PgLiteral::F64(f64::INFINITY)).unwrap(),
			"('Infinity'::pg_catalog.float8)"
		);
		assert_eq!(
			encode(PgLiteral::F64(f64::NEG_INFINITY)).unwrap(),
			"('-Infinity'::pg_catalog.float8)"
		);
		assert!(encode(PgLiteral::F64(1.5)).unwrap().starts_with("(1.5::"));
	}

	#[test]
	fn text_cast_emits_vouched_type_including_multiword() {
		assert_eq!(
			encode(PgLiteral::TextCast {
				value: "2026-07-02T00:00:00Z",
				pg_type: crate::audited_sql!("timestamptz"),
			})
			.unwrap(),
			"E'2026-07-02T00:00:00Z'::timestamptz"
		);
		assert_eq!(
			encode(PgLiteral::TextCast {
				value: "2026-07-02T00:00:00Z",
				pg_type: crate::audited_sql!("timestamp with time zone"),
			})
			.unwrap(),
			"E'2026-07-02T00:00:00Z'::timestamp with time zone"
		);
	}

	#[test]
	fn arrays_use_the_constructor_and_recurse_through_the_safe_encoder() {
		assert_eq!(
			encode(PgLiteral::Array {
				elements: &[PgLiteral::I32(1), PgLiteral::I32(2), PgLiteral::Null,],
				element_pg_type: crate::audited_sql!("int4"),
			})
			.unwrap(),
			"ARRAY[(1)::pg_catalog.int4,(2)::pg_catalog.int4,(NULL)]::int4[]"
		);
		assert_eq!(
			encode(PgLiteral::Array {
				elements: &[PgLiteral::Text("a'b"), PgLiteral::Text("c\\d")],
				element_pg_type: crate::audited_sql!("text"),
			})
			.unwrap(),
			"ARRAY[E'a''b',E'c\\\\d']::text[]"
		);
		assert_eq!(
			encode(PgLiteral::Array {
				elements: &[],
				element_pg_type: crate::audited_sql!("int4"),
			})
			.unwrap(),
			"ARRAY[]::int4[]"
		);
	}

	#[test]
	fn interleaving_produces_expected_sql() {
		let query = SimpleQuery::new()
			.push_sql(crate::audited_sql!("SELECT * FROM t WHERE k = "))
			.push_literal(PgLiteral::Text("k'v"))
			.unwrap()
			.push_sql(crate::audited_sql!(" AND n = "))
			.push_literal(PgLiteral::I64(42))
			.unwrap();
		assert_eq!(
			query.as_sql(),
			"SELECT * FROM t WHERE k =  E'k''v'  AND n =  (42)::pg_catalog.int8"
		);
	}

	#[test]
	fn a_text_literal_after_a_fragment_is_a_constant_of_its_own() {
		// `audited_sql!` rejects a template that ends inside a string constant, so no fragment
		// can open a quote for the literal; the literal is a constant of its own after a space.
		let query = SimpleQuery::new()
			.push_sql(crate::audited_sql!("SELECT * FROM t WHERE name ="))
			.push_literal(PgLiteral::Text("'; DROP TABLE t; --"))
			.unwrap()
			.push_sql(crate::audited_sql!("AND TRUE"));
		assert_eq!(
			query.as_sql(),
			"SELECT * FROM t WHERE name = E'''; DROP TABLE t; --' AND TRUE"
		);
	}

	#[test]
	fn a_fragment_that_ends_with_an_identifier_character_stays_apart_from_the_next_piece() {
		let literal = SimpleQuery::new()
			.push_sql(crate::audited_sql!("SELECT flag_"))
			.push_literal(PgLiteral::Bool(true))
			.unwrap();
		assert_eq!(literal.as_sql(), "SELECT flag_ (TRUE)");
		let fragment = SimpleQuery::new()
			.push_sql(crate::audited_sql!("SELECT flag_"))
			.push_sql(crate::audited_sql!("{b} FROM t", b = &false));
		assert_eq!(fragment.as_sql(), "SELECT flag_ (FALSE) FROM t");
	}

	#[test]
	fn only_a_piece_after_text_gets_a_space() {
		assert_eq!(
			SimpleQuery::new()
				.push_sql(crate::audited_sql!("SELECT 1"))
				.as_sql(),
			"SELECT 1"
		);
		assert_eq!(
			SimpleQuery::new()
				.push_literal(PgLiteral::Bool(true))
				.unwrap()
				.push_literal(PgLiteral::Null)
				.unwrap()
				.as_sql(),
			"(TRUE) (NULL)"
		);
	}

	#[test]
	fn signed_integer_minimums_encode_in_parentheses_before_the_cast() {
		assert_eq!(
			encode(PgLiteral::I16(i16::MIN)).unwrap(),
			"(-32768)::pg_catalog.int2"
		);
		assert_eq!(
			encode(PgLiteral::I32(i32::MIN)).unwrap(),
			"(-2147483648)::pg_catalog.int4"
		);
		assert_eq!(
			encode(PgLiteral::I64(i64::MIN)).unwrap(),
			"(-9223372036854775808)::pg_catalog.int8"
		);
	}

	#[test]
	fn negative_zero_and_whole_floats_keep_a_decimal_point() {
		assert_eq!(
			encode(PgLiteral::F64(-0.0)).unwrap(),
			"(-0.0::pg_catalog.float8)"
		);
		assert_eq!(
			encode(PgLiteral::F64(1.0)).unwrap(),
			"(1.0::pg_catalog.float8)"
		);
	}

	#[test]
	fn bytea_edge_bytes_encode_as_two_hex_digits_each() {
		assert_eq!(
			encode(PgLiteral::Bytea(&[0x00, 0xff, 0x27, 0x5c])).unwrap(),
			"E'\\\\x00ff275c'::pg_catalog.bytea"
		);
	}

	#[test]
	fn arrays_of_arrays_keep_each_inner_constructor_and_the_outer_dimension() {
		let inner = PgLiteral::Array {
			elements: &[PgLiteral::Null, PgLiteral::I32(5)],
			element_pg_type: crate::audited_sql!("int4"),
		};
		assert_eq!(
			encode(PgLiteral::Array {
				elements: &[inner],
				element_pg_type: crate::audited_sql!("int4[]"),
			})
			.unwrap(),
			"ARRAY[ARRAY[(NULL),(5)::pg_catalog.int4]::int4[]]::int4[][]"
		);
	}

	/// A nullable, possibly nested array of integers and the text that its encoding must have.
	#[derive(Clone, Debug)]
	enum Element {
		Null,
		Integer(i32),
		Nested(Vec<Element>),
	}

	fn element() -> impl Strategy<Value = Element> {
		let scalar = prop_oneof![Just(Element::Null), any::<i32>().prop_map(Element::Integer)];
		scalar.clone().prop_recursive(1, 12, 3, move |_| {
			prop::collection::vec(scalar.clone(), 0..3).prop_map(Element::Nested)
		})
	}

	fn expected_element(element: &Element) -> String {
		match element {
			Element::Null => "(NULL)".to_owned(),
			Element::Integer(value) => format!("({value})::pg_catalog.int4"),
			Element::Nested(elements) => format!(
				"ARRAY[{}]::int4[]",
				elements
					.iter()
					.map(expected_element)
					.collect::<Vec<_>>()
					.join(",")
			),
		}
	}

	fn literal_of(element: &Element) -> PgLiteral<'_> {
		match element {
			Element::Null => PgLiteral::Null,
			Element::Integer(value) => PgLiteral::I32(*value),
			Element::Nested(_) => unreachable!("nested arrays are built by the caller"),
		}
	}

	proptest! {
		#![proptest_config(ProptestConfig {
			cases: 512,
			failure_persistence: None,
			..ProptestConfig::default()
		})]

		#[test]
		fn text_without_nul_is_escaped_exactly(value in "[^\0]{0,64}") {
			let expected = format!("E'{}'", value.replace('\\', "\\\\").replace('\'', "''"));
			prop_assert_eq!(encode(PgLiteral::Text(&value)).unwrap(), expected);
		}

		#[test]
		fn text_with_a_nul_is_always_rejected(prefix in "[^\0]{0,16}", suffix in "[^\0]{0,16}") {
			let value = format!("{prefix}\0{suffix}");
			prop_assert!(encode(PgLiteral::Text(&value)).is_err());
		}

		#[test]
		fn bytes_are_encoded_as_lowercase_hex(bytes in prop::collection::vec(any::<u8>(), 0..256)) {
			let hex = bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
			prop_assert_eq!(
				encode(PgLiteral::Bytea(&bytes)).unwrap(),
				format!("E'\\\\x{hex}'::pg_catalog.bytea")
			);
		}

		#[test]
		fn every_signed_64_bit_integer_keeps_its_spelling(value in any::<i64>()) {
			prop_assert_eq!(
				encode(PgLiteral::I64(value)).unwrap(),
				format!("({value})::pg_catalog.int8")
			);
		}

		#[test]
		fn floats_keep_their_bits_or_their_special_spelling(bits in any::<u64>()) {
			let value = f64::from_bits(bits);
			let encoded = encode(PgLiteral::F64(value)).unwrap();
			if value.is_nan() {
				prop_assert_eq!(encoded, "('NaN'::pg_catalog.float8)");
			} else if value.is_infinite() {
				let sign = if value < 0.0 { "-" } else { "" };
				prop_assert_eq!(encoded, format!("('{sign}Infinity'::pg_catalog.float8)"));
			} else {
				let digits = encoded
					.strip_prefix("(")
					.unwrap()
					.strip_suffix("::pg_catalog.float8)")
					.unwrap();
				prop_assert_eq!(digits.parse::<f64>().unwrap().to_bits(), bits);
			}
		}

		#[test]
		fn integer_arrays_are_constructed_from_their_encoded_elements(
			values in prop::collection::vec(any::<i32>(), 0..24),
		) {
			let literals = values.iter().map(|value| PgLiteral::I32(*value)).collect::<Vec<_>>();
			let expected = format!(
				"ARRAY[{}]::int4[]",
				values
					.iter()
					.map(|value| format!("({value})::pg_catalog.int4"))
					.collect::<Vec<_>>()
					.join(",")
			);
			prop_assert_eq!(
				encode(PgLiteral::Array {
					elements: &literals,
					element_pg_type: crate::audited_sql!("int4"),
				})
				.unwrap(),
				expected
			);
		}

		#[test]
		fn nested_nullable_arrays_match_an_independent_rendering(
			elements in prop::collection::vec(element(), 0..3),
		) {
			let inner = elements
				.iter()
				.map(|element| match element {
					Element::Nested(inner) => inner.iter().map(literal_of).collect::<Vec<_>>(),
					scalar => vec![literal_of(scalar)],
				})
				.collect::<Vec<_>>();
			let wrapped = inner
				.iter()
				.zip(&elements)
				.map(|(literals, element)| match element {
					Element::Nested(_) => PgLiteral::Array {
						elements: literals,
						element_pg_type: crate::audited_sql!("int4"),
					},
					_ => literals[0].clone(),
				})
				.collect::<Vec<_>>();
			let expected = format!(
				"ARRAY[{}]::int4[]",
				elements.iter().map(expected_element).collect::<Vec<_>>().join(",")
			);
			prop_assert_eq!(
				encode(PgLiteral::Array {
					elements: &wrapped,
					element_pg_type: crate::audited_sql!("int4"),
				})
				.unwrap(),
				expected
			);
		}
	}
}
