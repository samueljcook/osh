//! SQL text with a structure that is fixed at compile time.
//!
//! [`AuditedSql`] comes from [`audited_sql!`](crate::audited_sql): a string literal template
//! with named placeholders for quoted identifiers and typed literals. The macro accepts a
//! placeholder only in SQL code and only between the neighbors that its documentation lists,
//! and it accepts a template only when the template ends in SQL code. The part of a rendering
//! that varies with the value stays inside one closed group, and the text after it is the same
//! for every value, so the value of an argument changes no token outside that group and never
//! changes the statement's structure. A literal argument, including a `bool` (`(TRUE)` or
//! `(FALSE)`), renders as an expression, so PostgreSQL never reads it as a column label or a
//! keyword. No function of the public API turns a runtime string into [`AuditedSql`].
//!
//! The macro checks only the neighbors inside one template. Code that joins an [`AuditedSql`]
//! with other SQL text at run time must separate the two pieces so that no token of one
//! continues into the other, as [`SimpleQuery`](super::SimpleQuery) does with one space.
//!
//! The macro's expansion builds the text with [`AuditedSqlBuilder`], which the hidden module
//! `paranoid::__private` exports. That module is the builder's only path outside this crate and
//! is not part of the public API: it exists for the expansion, and code other than the
//! expansion must not use it because [`AuditedSqlBuilder::push_text`] accepts any
//! `&'static str`, also a runtime string that a program leaks.

use std::borrow::Cow;
use std::fmt::Write as _;

use sqlx::{AssertSqlSafe, SqlSafeStr, SqlStr};

use super::simple_query::{
	encode_bool_literal, encode_bytea_literal, encode_f64_literal, encode_i16_literal,
	encode_i32_literal, encode_i64_literal,
};
use super::{PgIdentifier, PgQualifiedTableName, PgSchemaName};

/// SQL text built by [`audited_sql!`](crate::audited_sql).
///
/// The text is a string literal template in which each named placeholder is replaced by a
/// value that renders safely: a quoted identifier or a typed literal. The type asserts that
/// nobody built the text from a runtime string, not that the SQL is correct for the
/// statement that uses it.
///
/// ```
/// # use paranoid::db::{PgIdentifier, PgQualifiedTableName, PgSchemaName};
/// # fn main() -> Result<(), paranoid::db::InvalidPgIdentifierError> {
/// let items = PgQualifiedTableName::new(
///     PgSchemaName::from_identifier_text("app")?,
///     PgIdentifier::new("items")?,
/// );
/// let kind = PgIdentifier::new("kind")?;
/// let sql = paranoid::audited_sql!(
///     "CREATE TABLE IF NOT EXISTS {items} ({kind} pg_catalog.text NOT NULL)",
///     items = &items,
///     kind = &kind,
/// );
/// assert_eq!(
///     sql.as_str(),
///     r#"CREATE TABLE IF NOT EXISTS "app"."items" ("kind" pg_catalog.text NOT NULL)"#
/// );
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuditedSql(Cow<'static, str>);

impl AuditedSql {
	/// Returns the SQL text.
	pub fn as_str(&self) -> &str {
		&self.0
	}

	pub(crate) fn into_string(self) -> String {
		self.0.into_owned()
	}
}

impl SqlSafeStr for AuditedSql {
	fn into_sql_str(self) -> SqlStr {
		AssertSqlSafe(self.0).into_sql_str()
	}
}

/// A value that [`audited_sql!`](crate::audited_sql) can place in a template: a quoted
/// identifier ([`PgIdentifier`], [`PgSchemaName`], or [`PgQualifiedTableName`]) or a typed
/// literal that Paranoid's encoders render (`bool`, `i16`, `i32`, `i64`, `f64`, and bytes).
///
/// Applications cannot implement this trait. Text is not a literal argument because
/// encoding text can fail; write text in the template, or bind it with
/// [`portable_query`](super::portable_query).
pub trait AuditedSqlArgument: sealed::Sealed {
	/// Appends the SQL rendering of `self` to `out`.
	#[doc(hidden)]
	fn render_into(&self, out: &mut String);
}

mod sealed {
	pub trait Sealed {}
}

macro_rules! implement_argument {
	($($type:ty => |$value:ident, $out:ident| $render:expr;)*) => {
		$(
			impl sealed::Sealed for $type {}
			impl AuditedSqlArgument for $type {
				fn render_into(&self, $out: &mut String) {
					let $value = self;
					$render
				}
			}
		)*
	};
}

implement_argument! {
	PgIdentifier => |value, out| write!(out, "{}", value.quoted()).expect("write to String is infallible");
	PgSchemaName => |value, out| write!(out, "{}", value.identifier().quoted()).expect("write to String is infallible");
	PgQualifiedTableName => |value, out| write!(out, "{}", value.quoted()).expect("write to String is infallible");
	bool => |value, out| encode_bool_literal(*value, out);
	i16 => |value, out| encode_i16_literal(*value, out);
	i32 => |value, out| encode_i32_literal(*value, out);
	i64 => |value, out| encode_i64_literal(*value, out);
	f64 => |value, out| encode_f64_literal(*value, out);
	[u8] => |value, out| encode_bytea_literal(value, out);
	Vec<u8> => |value, out| encode_bytea_literal(value, out);
}

impl<const N: usize> sealed::Sealed for [u8; N] {}
impl<const N: usize> AuditedSqlArgument for [u8; N] {
	fn render_into(&self, out: &mut String) {
		encode_bytea_literal(self, out);
	}
}

impl<T: sealed::Sealed + ?Sized> sealed::Sealed for &T {}
impl<T: AuditedSqlArgument + ?Sized> AuditedSqlArgument for &T {
	fn render_into(&self, out: &mut String) {
		(**self).render_into(out);
	}
}

/// Assembles the expansion of [`audited_sql!`](crate::audited_sql).
///
/// The macro passes the template's text pieces as string literals and its arguments in
/// template order, after it checked the template at compile time. Code of this crate that calls
/// the builder directly gets no such check, so it must keep each argument between the
/// neighbors that the macro allows and must end the text in SQL code.
#[doc(hidden)]
#[derive(Debug, Default)]
pub struct AuditedSqlBuilder {
	sql: String,
}

impl AuditedSqlBuilder {
	/// Starts an empty statement.
	pub fn new() -> Self {
		Self::default()
	}

	/// Appends a piece of the template.
	pub fn push_text(&mut self, text: &'static str) {
		self.sql.push_str(text);
	}

	/// Appends the rendering of a placeholder's argument.
	pub fn push_argument<A: AuditedSqlArgument + ?Sized>(&mut self, argument: &A) {
		argument.render_into(&mut self.sql);
	}

	/// Finishes the statement.
	pub fn finish(self) -> AuditedSql {
		AuditedSql(Cow::Owned(self.sql))
	}
}
