//! SQLite page accounting and bounded incremental vacuum.
//!
//! Select [`AutoVacuumMode::Incremental`] through [`super::OpenOptions::auto_vacuum`]
//! before creating tables. A populated database without auto-vacuum metadata
//! requires an explicit rewrite, which opening a connection never performs.
//! Incremental vacuum removes free pages; it does not repack partially filled
//! pages or guarantee secure erasure. Applications choose when to reclaim space.
//!
//! In rollback-journal mode, committed reclamation can truncate the database
//! file. In WAL mode, the reduced page count first commits to the WAL; physical
//! truncation requires checkpoint progress and can be delayed by readers. These
//! operations do not request a WAL checkpoint or create application maintenance
//! files. SQLite's automatic checkpoint policy still applies at commit.
//!
//! ```no_run
//! use primitives_rs::db::sqlite::{Connection, JournalMode, OpenMode, OpenOptions};
//! use primitives_rs::db::sqlite::maintenance::{AutoVacuumMode, IncrementalVacuumOptions};
//! use std::{num::NonZeroU32, ops::ControlFlow, time::Duration};
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let mut db = Connection::open("cache.db", OpenOptions {
//!     mode: OpenMode::CreateIfMissing,
//!     journal_mode: JournalMode::Rollback,
//!     auto_vacuum: Some(AutoVacuumMode::Incremental),
//!     foreign_keys: true,
//!     busy_timeout: Duration::from_secs(1),
//! })?;
//! let usage = db.read(|tx| tx.page_stats())?;
//! let result = db.write(|tx| tx.incremental_vacuum(
//!     IncrementalVacuumOptions {
//!         max_pages: NonZeroU32::new(64).unwrap(),
//!         timeout: Duration::from_millis(100),
//!     },
//!     |_| ControlFlow::Continue(()),
//! ))?;
//! # let _ = (usage, result);
//! # Ok(())
//! # }
//! ```

use super::{LibraryControlGuard, OperationError, Transaction};
use std::{
	num::NonZeroU32,
	ops::ControlFlow,
	time::{Duration, Instant},
};

/// The database's persisted auto-vacuum mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AutoVacuumMode {
	/// Retains free pages for reuse without auto-vacuum metadata.
	None,
	/// Reclaims all free pages at each write commit, without a per-commit page bound.
	Full,
	/// Retains metadata for explicitly requested incremental reclamation.
	Incremental,
}

impl AutoVacuumMode {
	pub(super) fn as_sqlite_value(self) -> i32 {
		match self {
			Self::None => 0,
			Self::Full => 1,
			Self::Incremental => 2,
		}
	}
}

/// Page accounting within the transaction's SQLite snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PageStats {
	/// The persisted auto-vacuum mode observed in this snapshot.
	pub auto_vacuum: AutoVacuumMode,
	/// SQLite page size in bytes.
	pub page_size: u32,
	/// Logical database pages, including metadata and free pages.
	/// This is not the filesystem allocation or the size of the WAL and journals.
	pub total_pages: u32,
	/// Whole unused pages available for reuse or incremental reclamation.
	/// Unused space inside a partly filled page is not included.
	pub free_pages: u32,
}

/// Explicit limits for one incremental-vacuum operation.
#[derive(Clone, Copy, Debug)]
pub struct IncrementalVacuumOptions {
	/// Maximum free pages to remove. The value must fit in `i32`.
	/// SQLite may also remove pointer-map pages and relocate occupied pages.
	/// This does not bound total reads, writes, or elapsed time.
	pub max_pages: NonZeroU32,
	/// Cooperative time limit after entering this operation. Zero expires immediately.
	/// Checks occur between SQLite steps and callbacks and before successful return.
	/// A pending SQLite call, filesystem call, or callback can exceed this limit.
	/// Transaction acquisition, rollback, and commit are outside this limit.
	pub timeout: Duration,
}

/// Progress within the enclosing, uncommitted write transaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IncrementalVacuumProgress {
	/// Page accounting before the operation began reclaiming pages.
	pub initial_stats: PageStats,
	/// Completed SQLite incremental-vacuum steps. Each step removes one free page.
	pub completed_steps: u32,
}

/// Page accounting after the operation, before the enclosing transaction commits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IncrementalVacuumResult {
	/// Page accounting before reclamation.
	pub before: PageStats,
	/// Page accounting after reclamation in this transaction.
	pub after: PageStats,
}

/// Incremental reclamation could not complete. The enclosing transaction cannot commit.
#[derive(Debug, thiserror::Error)]
pub enum IncrementalVacuumError {
	/// A preceding transaction operation failed or SQLite ended the transaction.
	#[error("SQLite transaction is unavailable: {0}")]
	Operation(#[from] OperationError),
	/// SQLite rejected page accounting or reclamation.
	#[error("SQLite incremental vacuum failed: {0}")]
	Sqlite(#[from] rusqlite::Error),
	/// The operation requires a write transaction, including when no pages are free.
	#[error("SQLite incremental vacuum requires a write transaction")]
	ReadOnlyTransaction,
	/// The database does not use incremental mode. Full mode cannot honor the bound at commit.
	#[error("SQLite incremental vacuum requires incremental mode; actual mode is {actual:?}")]
	IncrementalModeRequired {
		/// The mode observed under the write transaction's lock.
		actual: AutoVacuumMode,
	},
	/// SQLite cannot represent the requested positive page bound.
	#[error("SQLite incremental vacuum page limit exceeds i32::MAX")]
	InvalidPageLimit,
	/// The progress callback requested cancellation before successful return.
	#[error("SQLite incremental vacuum was cancelled")]
	Cancelled,
	/// The cooperative time limit expired before successful return.
	#[error("SQLite incremental vacuum timed out")]
	TimedOut,
}

impl Transaction<'_> {
	/// Reads mode and page accounting from this transaction's snapshot.
	/// The first database read establishes a read transaction's snapshot. Write
	/// transactions include their own changes. Errors prevent transaction commit.
	pub fn page_stats(&mut self) -> Result<PageStats, OperationError> {
		self.run_operation(|tx| {
			tx.owner
				.with_library_control(read_page_stats)
				.map_err(OperationError::from)
		})
	}

	/// Reclaims bounded free pages in this write transaction.
	///
	/// The progress callback runs before reclamation and after each completed step,
	/// including the final step. `Break(())`, timeout, SQLite failure, and callback
	/// panic prevent commit even when the caller catches the error or panic.
	/// A successful method return is provisional: [`super::Connection::write`]
	/// commits the enclosing transaction or reports its completion failure.
	///
	/// Applications can compose deletes and reclamation in one transaction. This
	/// operation does not replay callbacks, retry locks, change the persisted mode,
	/// run a full vacuum, or request a WAL checkpoint. Only incremental mode is accepted;
	/// full mode could reclaim additional pages automatically at commit.
	pub fn incremental_vacuum(
		&mut self,
		options: IncrementalVacuumOptions,
		mut on_progress_fn: impl FnMut(IncrementalVacuumProgress) -> ControlFlow<()>,
	) -> Result<IncrementalVacuumResult, IncrementalVacuumError> {
		self.run_operation(|tx| {
			let started = Instant::now();
			if tx.is_read_only() {
				return Err(IncrementalVacuumError::ReadOnlyTransaction);
			}
			let limit = i32::try_from(options.max_pages.get())
				.map_err(|_| IncrementalVacuumError::InvalidPageLimit)?;
			let check_timeout = || {
				if started.elapsed() >= options.timeout {
					Err(IncrementalVacuumError::TimedOut)
				} else {
					Ok(())
				}
			};
			check_timeout()?;
			let before = tx.owner.with_library_control(read_page_stats)?;
			if before.auto_vacuum != AutoVacuumMode::Incremental {
				return Err(IncrementalVacuumError::IncrementalModeRequired {
					actual: before.auto_vacuum,
				});
			}
			let mut progress = IncrementalVacuumProgress {
				initial_stats: before,
				completed_steps: 0,
			};
			let mut report = |progress| {
				check_timeout()?;
				if on_progress_fn(progress).is_break() {
					return Err(IncrementalVacuumError::Cancelled);
				}
				check_timeout()
			};
			report(progress)?;
			let sql = format!("PRAGMA main.incremental_vacuum({limit})");
			{
				let permission = LibraryControlGuard::new(&tx.owner.library_control);
				let mut statement = tx.owner.raw().prepare(&sql)?;
				drop(permission);
				let mut rows = statement.query([])?;
				loop {
					check_timeout()?;
					let more = {
						let _permission = LibraryControlGuard::new(&tx.owner.library_control);
						rows.next()?.is_some()
					};
					if !more {
						break;
					}
					progress.completed_steps += 1;
					report(progress)?;
				}
			}
			let after = tx.owner.with_library_control(read_page_stats)?;
			check_timeout()?;
			Ok(IncrementalVacuumResult { before, after })
		})
	}
}

pub(super) fn read_auto_vacuum_mode(
	connection: &rusqlite::Connection,
) -> rusqlite::Result<AutoVacuumMode> {
	connection.pragma_query_value(None, "auto_vacuum", |row| match row.get::<_, i64>(0)? {
		0 => Ok(AutoVacuumMode::None),
		1 => Ok(AutoVacuumMode::Full),
		2 => Ok(AutoVacuumMode::Incremental),
		value => Err(rusqlite::Error::IntegralValueOutOfRange(0, value)),
	})
}

fn read_page_stats(connection: &rusqlite::Connection) -> rusqlite::Result<PageStats> {
	// Reading page_count establishes the snapshot before inspecting the mode.
	let total_pages =
		connection.pragma_query_value(Some("main"), "page_count", |row| row.get(0))?;
	let free_pages =
		connection.pragma_query_value(Some("main"), "freelist_count", |row| row.get(0))?;
	let page_size = connection.pragma_query_value(Some("main"), "page_size", |row| row.get(0))?;
	Ok(PageStats {
		auto_vacuum: read_auto_vacuum_mode(connection)?,
		page_size,
		total_pages,
		free_pages,
	})
}
