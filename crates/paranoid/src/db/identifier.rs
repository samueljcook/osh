use std::fmt;
use std::str::FromStr;

/// Maximum byte length for a Postgres identifier.
pub const MAX_PG_IDENTIFIER_LEN: usize = 63;

/// A validated unqualified Postgres identifier.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PgIdentifier(String);

/// A validated Postgres schema name.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PgSchemaName(PgIdentifier);

/// A validated table name that always carries its schema, which means that it names the same
/// relation in every session regardless of `search_path`.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PgQualifiedTableName {
	schema: PgSchemaName,
	table: PgIdentifier,
}

/// Display wrapper that emits a quoted Postgres identifier.
#[derive(Clone, Copy, Debug)]
pub struct QuotedPgIdentifier<'a>(&'a PgIdentifier);

/// Display wrapper that emits a quoted Postgres qualified table name.
#[derive(Clone, Copy, Debug)]
pub struct QuotedPgQualifiedTableName<'a>(&'a PgQualifiedTableName);

/// Error returned when a Postgres identifier is rejected.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InvalidPgIdentifierError {
	/// Identifier text must not be empty.
	Empty,
	/// Identifier text exceeded its maximum byte length.
	TooLong {
		/// The supplied identifier's length in bytes.
		actual: usize,
		/// Maximum accepted byte length.
		max: usize,
	},
	/// The first identifier byte was not an ASCII letter or underscore.
	InvalidFirstByte {
		/// Invalid first byte.
		byte: u8,
	},
	/// A non-first identifier byte was not an ASCII letter, digit, or underscore.
	InvalidByte {
		/// Zero-based byte index where validation failed.
		index: usize,
		/// Invalid byte value.
		byte: u8,
	},
}

impl PgIdentifier {
	/// Validates and copies an unqualified Postgres identifier.
	pub fn new(input: impl AsRef<str>) -> Result<Self, InvalidPgIdentifierError> {
		validate_pg_identifier(input.as_ref(), MAX_PG_IDENTIFIER_LEN)?;
		Ok(Self(input.as_ref().to_owned()))
	}

	/// Returns the validated identifier text.
	pub fn as_str(&self) -> &str {
		&self.0
	}

	/// Returns a display wrapper for quoted SQL generation.
	pub fn quoted(&self) -> QuotedPgIdentifier<'_> {
		QuotedPgIdentifier(self)
	}
}

impl PgSchemaName {
	/// Wraps a validated identifier as a schema name.
	pub fn new(identifier: PgIdentifier) -> Self {
		Self(identifier)
	}

	/// Validates and copies a Postgres schema name.
	pub fn from_identifier_text(input: impl AsRef<str>) -> Result<Self, InvalidPgIdentifierError> {
		Ok(Self(PgIdentifier::new(input)?))
	}

	/// Returns the schema identifier.
	pub fn identifier(&self) -> &PgIdentifier {
		&self.0
	}

	/// Returns the validated schema text.
	pub fn as_str(&self) -> &str {
		self.0.as_str()
	}
}

impl PgQualifiedTableName {
	/// Creates a table name from validated schema and table parts.
	pub fn new(schema: PgSchemaName, table: PgIdentifier) -> Self {
		Self { schema, table }
	}

	/// Returns the schema name.
	pub fn schema(&self) -> &PgSchemaName {
		&self.schema
	}

	/// Returns the table identifier.
	pub fn table(&self) -> &PgIdentifier {
		&self.table
	}

	/// Returns a display wrapper for quoted SQL generation.
	pub fn quoted(&self) -> QuotedPgQualifiedTableName<'_> {
		QuotedPgQualifiedTableName(self)
	}
}

impl fmt::Display for QuotedPgIdentifier<'_> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "\"{}\"", self.0.as_str())
	}
}

impl fmt::Display for QuotedPgQualifiedTableName<'_> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(
			f,
			"{}.{}",
			self.0.schema().identifier().quoted(),
			self.0.table().quoted()
		)
	}
}

pub(crate) fn pg_table_name_set_contains_duplicate(table_names: &[&PgQualifiedTableName]) -> bool {
	for (index, left) in table_names.iter().enumerate() {
		for right in &table_names[index + 1..] {
			if left == right {
				return true;
			}
		}
	}
	false
}

impl fmt::Display for InvalidPgIdentifierError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Empty => write!(f, "Postgres identifier must not be empty"),
			Self::TooLong { actual, max } => {
				write!(f, "Postgres identifier is {actual} bytes, maximum is {max}")
			}
			Self::InvalidFirstByte { byte } => write!(
				f,
				"Postgres identifier first byte 0x{byte:02x} is not an ASCII letter or underscore"
			),
			Self::InvalidByte { index, byte } => write!(
				f,
				"Postgres identifier byte 0x{byte:02x} at index {index} is not an ASCII letter, digit, or underscore"
			),
		}
	}
}

impl std::error::Error for InvalidPgIdentifierError {}

impl FromStr for PgIdentifier {
	type Err = InvalidPgIdentifierError;

	fn from_str(input: &str) -> Result<Self, Self::Err> {
		Self::new(input)
	}
}

impl FromStr for PgSchemaName {
	type Err = InvalidPgIdentifierError;

	fn from_str(input: &str) -> Result<Self, Self::Err> {
		Self::from_identifier_text(input)
	}
}

fn validate_pg_identifier(input: &str, max_len: usize) -> Result<(), InvalidPgIdentifierError> {
	if input.is_empty() {
		return Err(InvalidPgIdentifierError::Empty);
	}
	if input.len() > max_len {
		return Err(InvalidPgIdentifierError::TooLong {
			actual: input.len(),
			max: max_len,
		});
	}

	let bytes = input.as_bytes();
	let first = bytes[0];
	if !is_pg_identifier_first_byte(first) {
		return Err(InvalidPgIdentifierError::InvalidFirstByte { byte: first });
	}

	for (index, byte) in bytes.iter().copied().enumerate().skip(1) {
		if !is_pg_identifier_trailing_byte(byte) {
			return Err(InvalidPgIdentifierError::InvalidByte { index, byte });
		}
	}

	Ok(())
}

fn is_pg_identifier_first_byte(byte: u8) -> bool {
	byte == b'_' || byte.is_ascii_alphabetic()
}

fn is_pg_identifier_trailing_byte(byte: u8) -> bool {
	is_pg_identifier_first_byte(byte) || byte.is_ascii_digit()
}

/// Names a table in the `public` schema for synthetic internal handles.
#[cfg(any(test, feature = "__internal"))]
pub(crate) fn table_name_for_tests(table: &str) -> PgQualifiedTableName {
	PgQualifiedTableName::new(
		PgSchemaName::from_identifier_text("public")
			.expect("test table name must be a valid Postgres identifier"),
		PgIdentifier::new(table).expect("test table name must be a valid Postgres identifier"),
	)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn table(schema: &str, table: &str) -> PgQualifiedTableName {
		PgQualifiedTableName::new(
			PgSchemaName::from_identifier_text(schema).expect("table"),
			PgIdentifier::new(table).expect("table"),
		)
	}

	#[test]
	fn two_tables_collide_exactly_when_schema_and_name_are_both_equal() {
		let schemas = ["public", "tenant_a", "tenant_b"];
		for left_schema in schemas {
			for right_schema in schemas {
				let left = table(left_schema, "same_table");
				let right = table(right_schema, "same_table");
				assert_eq!(
					pg_table_name_set_contains_duplicate(&[&left, &right]),
					left_schema == right_schema,
					"{left_schema} and {right_schema}"
				);
			}
		}
	}

	#[test]
	fn tables_with_different_names_in_one_schema_do_not_collide() {
		let jobs = table("public", "jobs");
		let pauses = table("public", "pauses");
		assert!(!pg_table_name_set_contains_duplicate(&[&jobs, &pauses]));
		assert!(!pg_table_name_set_contains_duplicate(&[&jobs]));
		assert!(!pg_table_name_set_contains_duplicate(&[]));
	}

	#[test]
	fn a_duplicate_anywhere_in_the_set_is_found() {
		let jobs = table("public", "jobs");
		let pauses = table("public", "pauses");
		let again = table("public", "jobs");
		assert!(pg_table_name_set_contains_duplicate(&[
			&jobs, &pauses, &again
		]));
	}

	#[test]
	fn a_qualified_name_is_built_from_validated_components() {
		let qualified = PgQualifiedTableName::new(
			PgSchemaName::from_identifier_text("queue").expect("schema"),
			PgIdentifier::new("jobs").expect("table"),
		);
		assert_eq!(qualified.quoted().to_string(), "\"queue\".\"jobs\"");
		assert_eq!(qualified.schema().as_str(), "queue");
		assert_eq!(qualified.table().as_str(), "jobs");
	}
}
