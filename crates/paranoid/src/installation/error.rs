//! Failures to open an installation.

use std::fmt;

use super::creation::NativeInstallationError;
use crate::db::{DatabaseError, TransactionFailure};

pub(super) enum InstallationOpeningFailure {
	Installation(NativeInstallationError),
	ScopeSelections(crate::scope::selection::ScopeSelectionError),
}

/// An installation that could not be opened.
pub struct PgInstallationError {
	source: Box<InstallationOpeningFailure>,
}

/// The class of an opening failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PgInstallationErrorKind {
	/// A database statement or the transaction's own control failed. The error's source chain
	/// holds the database error.
	Database,
	/// The app's declarations do not form a valid initial state, for example because a scope's
	/// code writes no version as JSON.
	ScopeSelections,
	/// The installation that the database records, or one of its tables, does not match this
	/// code or this v10 location.
	RecordedInstallationMismatch,
	/// The database's encoding is neither `UTF8` nor `SQL_ASCII`, and opening would import a
	/// published v10 installation or open an installation that still reads v10 data. The error's
	/// source names the encoding. See [`PgV10Fields`](crate::installation::PgV10Fields).
	UnsupportedDatabaseEncoding,
	/// A local failure or a defect in Paranoid.
	Internal,
}

impl PgInstallationError {
	pub(super) fn new(source: InstallationOpeningFailure) -> Self {
		Self {
			source: Box::new(source),
		}
	}

	/// The class of the failure.
	pub fn kind(&self) -> PgInstallationErrorKind {
		match self.source.as_ref() {
			InstallationOpeningFailure::Installation(source) => source.opening_kind(),
			InstallationOpeningFailure::ScopeSelections(_) => {
				PgInstallationErrorKind::ScopeSelections
			}
		}
	}

	/// The database error, when a database statement or a transaction's own control failed.
	/// When both a statement and the following rollback failed, this is the statement's
	/// error.
	pub fn database_error(&self) -> Option<&DatabaseError> {
		match self.source.as_ref() {
			InstallationOpeningFailure::Installation(source) => source.database_error(),
			InstallationOpeningFailure::ScopeSelections(_) => None,
		}
	}

	/// Whether a transaction's own `COMMIT` failed, which means that its changes may or may
	/// not have been committed. Opening again checks the database and reopens a committed
	/// installation.
	pub fn commit_outcome_is_unknown(&self) -> bool {
		match self.source.as_ref() {
			InstallationOpeningFailure::Installation(source) => source.commit_outcome_is_unknown(),
			InstallationOpeningFailure::ScopeSelections(_) => false,
		}
	}
}

impl From<NativeInstallationError> for PgInstallationError {
	fn from(source: NativeInstallationError) -> Self {
		Self::new(InstallationOpeningFailure::Installation(source))
	}
}

// The causes' messages name categories, positions, and roles, never stored values, so the
// complete chain is safe to format.
impl fmt::Debug for PgInstallationError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(formatter, "PgInstallationError: {self}")?;
		let mut source = std::error::Error::source(self);
		while let Some(error) = source {
			write!(formatter, ": {error}")?;
			source = error.source();
		}
		Ok(())
	}
}

impl fmt::Display for PgInstallationError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str("PostgreSQL installation opening failed")
	}
}

impl std::error::Error for PgInstallationError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self.source.as_ref() {
			InstallationOpeningFailure::Installation(source) => Some(source),
			InstallationOpeningFailure::ScopeSelections(source) => Some(source),
		}
	}
}
