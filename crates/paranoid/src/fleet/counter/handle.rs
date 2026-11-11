//! Public Counter operations of the native runtime.

use std::error::Error;
use std::fmt;

use crate::db::{Database, TransactionFailure, WriteDatabase};
use crate::fleet::CounterKey;
use crate::fleet::counter::engine::{NativeCounterError, PgNativeCounter};
use crate::fleet::counter::state::CounterOperationError;
use crate::fleet::lease::handle::runtime_stopped_in_chain;

/// The inputs of a counter handle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PgCounterFields {
	/// The counter's key.
	pub key: CounterKey,
}

/// One signed 64-bit counter of the runtime's installation.
///
/// Every operation checks the installation's Counter policy. While an imported installation
/// still coexists with v10 deployments, operations read and write the v10 counter.
#[derive(Clone)]
pub struct PgCounter {
	key: CounterKey,
	native: PgNativeCounter,
}

impl PgCounter {
	pub(crate) fn new(key: CounterKey, native: PgNativeCounter) -> Self {
		Self { key, native }
	}

	/// The counter's key.
	pub fn key(&self) -> &CounterKey {
		&self.key
	}

	/// Reads the counter's value. An absent counter has the value 0.
	pub async fn fetch_value(&self, database: impl Database) -> Result<i64, PgCounterError> {
		self.native
			.fetch_value(database)
			.await
			.map_err(PgCounterError::new)
	}

	/// Adds `delta` and returns the new value. A result outside the signed 64-bit range fails
	/// with [`PgCounterErrorKind::ArithmeticOverflow`] and changes nothing.
	pub async fn add(
		&self,
		database: impl WriteDatabase,
		delta: i64,
	) -> Result<i64, PgCounterError> {
		self.native
			.add(database, delta)
			.await
			.map_err(PgCounterError::new)
	}

	/// Replaces the counter's value.
	pub async fn set_value(
		&self,
		database: impl WriteDatabase,
		value: i64,
	) -> Result<(), PgCounterError> {
		self.native
			.set_value(database, value)
			.await
			.map_err(PgCounterError::new)
	}
}

/// The category of a Counter failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PgCounterErrorKind {
	/// The result would leave the signed 64-bit range.
	ArithmeticOverflow,
	/// The runtime cannot operate on the counter under the installation's recorded policy.
	Admission,
	/// A database statement or the transaction's own control failed.
	Database,
	/// The runtime stopped accepting work.
	RuntimeStopped,
	/// The stored counter state could not be interpreted or encoded.
	State,
}

/// A failed Counter operation, with its complete cause chain.
pub struct PgCounterError {
	source: Box<NativeCounterError>,
}

impl PgCounterError {
	pub(crate) fn new(source: NativeCounterError) -> Self {
		Self {
			source: Box::new(source),
		}
	}

	/// The failure's category.
	pub fn kind(&self) -> PgCounterErrorKind {
		match self.source.as_ref() {
			NativeCounterError::Mutation(error) if is_overflow(error) => {
				PgCounterErrorKind::ArithmeticOverflow
			}
			source if source.involves_database_failure() => PgCounterErrorKind::Database,
			source if runtime_stopped_in_chain(source) => PgCounterErrorKind::RuntimeStopped,
			NativeCounterError::Admission(_)
			| NativeCounterError::Protocol(_)
			| NativeCounterError::SourceMappingAbsent
			| NativeCounterError::StorageMappingMismatch => PgCounterErrorKind::Admission,
			_ => PgCounterErrorKind::State,
		}
	}

	/// Whether the operation's own `COMMIT` failed, which means that the change may or may not
	/// have committed.
	pub fn commit_outcome_is_unknown(&self) -> bool {
		self.source.commit_outcome_is_unknown()
	}
}

fn is_overflow<E>(error: &CounterOperationError<E>) -> bool {
	match error {
		CounterOperationError::ArithmeticOverflow => true,
		CounterOperationError::CleanupFailed {
			operation_error, ..
		} => is_overflow(operation_error),
		CounterOperationError::Representation(_) | CounterOperationError::Storage(_) => false,
	}
}

impl fmt::Display for PgCounterError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str("counter operation failed")
	}
}

impl fmt::Debug for PgCounterError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(formatter, "PgCounterError({:?}): {self}", self.kind())?;
		let mut source = self.source();
		while let Some(error) = source {
			write!(formatter, ": {error}")?;
			source = error.source();
		}
		Ok(())
	}
}

impl Error for PgCounterError {
	fn source(&self) -> Option<&(dyn Error + 'static)> {
		Some(self.source.as_ref())
	}
}
