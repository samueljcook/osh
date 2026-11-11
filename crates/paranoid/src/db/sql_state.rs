use std::fmt;

#[doc(hidden)]
pub const SQLSTATE_UNIQUE_VIOLATION: &str = "23505";
#[doc(hidden)]
pub const SQLSTATE_FOREIGN_KEY_VIOLATION: &str = "23503";
#[doc(hidden)]
pub const SQLSTATE_CHECK_VIOLATION: &str = "23514";
#[doc(hidden)]
pub const SQLSTATE_NOT_NULL_VIOLATION: &str = "23502";
#[doc(hidden)]
pub const SQLSTATE_SERIALIZATION_FAILURE: &str = "40001";
#[doc(hidden)]
pub const SQLSTATE_DEADLOCK_DETECTED: &str = "40P01";

/// Public SQLSTATE categories used by Paranoid storage code.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PgSqlState {
	/// SQLSTATE `23505`.
	UniqueViolation,
	/// SQLSTATE `23503`.
	ForeignKeyViolation,
	/// SQLSTATE `23514`.
	CheckViolation,
	/// SQLSTATE `23502`.
	NotNullViolation,
	/// SQLSTATE `40001`.
	SerializationFailure,
	/// SQLSTATE `40P01`.
	DeadlockDetected,
	/// Any SQLSTATE without a more specific Paranoid category.
	Other(String),
}

impl PgSqlState {
	/// Maps a five-byte SQLSTATE code into a public semantic category.
	pub fn from_code(code: impl AsRef<str>) -> Self {
		match code.as_ref() {
			SQLSTATE_UNIQUE_VIOLATION => Self::UniqueViolation,
			SQLSTATE_FOREIGN_KEY_VIOLATION => Self::ForeignKeyViolation,
			SQLSTATE_CHECK_VIOLATION => Self::CheckViolation,
			SQLSTATE_NOT_NULL_VIOLATION => Self::NotNullViolation,
			SQLSTATE_SERIALIZATION_FAILURE => Self::SerializationFailure,
			SQLSTATE_DEADLOCK_DETECTED => Self::DeadlockDetected,
			other => Self::Other(other.to_owned()),
		}
	}

	/// Returns this SQLSTATE category's raw five-byte code.
	pub fn as_str(&self) -> &str {
		match self {
			Self::UniqueViolation => SQLSTATE_UNIQUE_VIOLATION,
			Self::ForeignKeyViolation => SQLSTATE_FOREIGN_KEY_VIOLATION,
			Self::CheckViolation => SQLSTATE_CHECK_VIOLATION,
			Self::NotNullViolation => SQLSTATE_NOT_NULL_VIOLATION,
			Self::SerializationFailure => SQLSTATE_SERIALIZATION_FAILURE,
			Self::DeadlockDetected => SQLSTATE_DEADLOCK_DETECTED,
			Self::Other(code) => code,
		}
	}
}

impl fmt::Display for PgSqlState {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(self.as_str())
	}
}
