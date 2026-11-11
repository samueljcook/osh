//! Consistent SQLite snapshots and offline recovery with atomic, create-only file publication.
//!
//! Enable `sqlite-backup` on Unix or Windows. [`Connection::backup_to_file`] uses
//! SQLite's online backup API to copy a pinned read snapshot, including committed
//! WAL data. [`recover_offline_to_file`] copies an offline database and any
//! sibling `-journal` or `-wal` files into a private temporary dir, replays
//! recovery there without modifying the source, and verifies `PRAGMA integrity_check`.
//! In both operations, the published output is a standalone database in
//! rollback-journal mode, and only a completed database is published.
//! Applications must not rename or unlink an open database or its journal files.
//!
//! A backup or recovered copy contains database pages, including any recoverable
//! deleted content that SQLite retains in those pages. Neither operation encrypts
//! the database, copies external files, or establishes an application-level
//! backup retention policy.
//!
//! [`Connection::backup_to_file`] requires a readable SQLite snapshot. A read-only
//! source with a hot rollback journal fails in [`Connection::backup_to_file`]
//! because recovery requires writes. Online backup does not bypass recovery or
//! fall back to raw file copying. When a source database and its companion
//! journals form an offline snapshot with no active writer or attached database,
//! call [`recover_offline_to_file`] explicitly to recover a standalone copy
//! without modifying the source files.

//! ```no_run
//! use primitives_rs::db::sqlite::{Connection, backup::BackupOptions};
//! use primitives_rs::fs::{Durability, Permissions, UnixMode};
//! use std::{ops::ControlFlow, time::Duration};
//! # fn example(db: &mut Connection) -> Result<(), Box<dyn std::error::Error>> {
//! let completed = db.backup_to_file("./snapshots/backup-001.db", BackupOptions {
//!     pages_per_step: 128.try_into()?, max_size: 512 * 1024 * 1024,
//!     timeout: Duration::from_secs(60),
//!     permissions: Permissions::Unix(UnixMode::OWNER_READ_WRITE),
//!     durability: Durability::FileAndDir,
//! }, |_| ControlFlow::Continue(()))?;
//! println!("Saved {} bytes", completed.size);
//! # Ok(())
//! # }
//! ```

use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::num::NonZeroU32;
use std::ops::ControlFlow;
use std::path::Path;
use std::time::{Duration, Instant};

use cap_fs_ext::MetadataExt;
use rusqlite::backup::{Backup, StepResult};
use rusqlite::limits::Limit;
use rusqlite::{OpenFlags, OptionalExtension};

use super::{Connection, TransactionError};
use crate::fs::{
	Dir, Durability, FileMoveSource, Permissions, RelativePath, WriteError, WriteMode, WriteOptions,
};

/// Resource and publication settings for one database backup.
#[derive(Clone, Copy, Debug)]
pub struct BackupOptions {
	/// Maximum pages per SQLite backup step. The value must fit in `i32`.
	pub pages_per_step: NonZeroU32,
	/// Maximum snapshot size in bytes, checked before page copying starts.
	pub max_size: u64,
	/// Cooperative duration limit, checked between steps and before publication.
	/// A SQLite call, filesystem call, or application callback can exceed this time.
	pub timeout: Duration,
	/// Permissions for the published database. Private staging protects partial data.
	pub permissions: Permissions,
	/// Synchronization required before successful return.
	pub durability: Durability,
}

/// Progress within one pinned source snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BackupProgress {
	/// Number of source pages copied into the temporary database.
	pub copied_pages: u32,
	/// Total source pages in this snapshot.
	pub total_pages: u32,
}

/// A completed database backup or offline recovery.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BackupOutcome {
	/// Number of pages in the published database.
	pub pages: u32,
	/// Size of the published database in bytes.
	pub size: u64,
}

/// Source and destination filesystem paths for [`recover_offline_to_file`].
#[derive(Clone, Copy, Debug)]
pub struct OfflineRecoveryPaths<'a> {
	/// Path to the offline source database file. Any sibling `-journal` or `-wal`
	/// companion in the same dir is staged alongside it.
	pub source: &'a Path,
	/// Path where the recovered standalone database file will be published.
	pub destination: &'a Path,
}

/// Resource and publication settings for [`recover_offline_to_file`].
#[derive(Clone, Copy, Debug)]
pub struct OfflineRecoveryOptions {
	/// Maximum combined byte size of the source database and companion journals,
	/// and maximum recovered database size. Each bound applies independently.
	/// SQLite's temporary journal and shared-memory overhead is outside these bounds.
	/// During recovery and verification, SQLite also limits each string, BLOB, row,
	/// and SQL statement to this size or to SQLite's compile-time maximum of
	/// 1,000,000,000 bytes, whichever is smaller. That limit bounds each value that
	/// an expression in the source schema computes, such as a `CHECK` constraint
	/// that `PRAGMA integrity_check` evaluates.
	pub max_size: u64,
	/// Cooperative duration limit, checked between copy chunks, during SQLite VM
	/// execution, and before publication. Filesystem calls and pager recovery can
	/// exceed this duration before control returns to a checkpoint.
	pub timeout: Duration,
	/// Permissions for the published database. Private staging protects partial data.
	pub permissions: Permissions,
	/// Synchronization required before successful return.
	pub durability: Durability,
}

/// Byte progress while staging source files for [`recover_offline_to_file`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OfflineRecoveryProgress {
	/// Number of source bytes copied into the private staging dir so far.
	pub copied_bytes: u64,
	/// Total bytes across the source database and any present `-journal` or `-wal` files.
	pub total_bytes: u64,
}

/// Offline recovery failed. Only `Publication(PublishedButNotDurable(_))` means
/// that a recovered database was published; every other variant published no
/// destination and left the source files unchanged.
#[derive(Debug, thiserror::Error)]
pub enum OfflineRecoveryError {
	/// A source or destination path was empty or named `:memory:`.
	#[error("SQLite offline recovery requires filesystem paths")]
	NonPersistentPath,
	/// Reading the source, staging the copy, or checking paths failed.
	#[error("SQLite offline recovery I/O failed: {0}")]
	Io(#[from] std::io::Error),
	/// The source database or a companion journal changed while being copied.
	#[error("SQLite offline recovery observed a concurrent source modification")]
	SourceModified,
	/// A rollback journal contains a possible external super-journal reference.
	/// Recovery across attached databases is unsupported.
	#[error("SQLite offline recovery rejects external super-journal references")]
	ExternalJournalReference,
	/// The source files or recovered database exceeded the configured byte bound.
	#[error("SQLite offline recovery exceeds its byte limit of {max_size}")]
	SizeLimitExceeded {
		/// Configured maximum byte size.
		max_size: u64,
	},
	/// The progress callback requested cancellation.
	#[error("SQLite offline recovery was cancelled")]
	Cancelled,
	/// The cooperative duration budget expired before publication.
	#[error("SQLite offline recovery timed out")]
	TimedOut,
	/// Opening, replaying, converting, or closing the staged database failed.
	#[error("SQLite offline recovery database operation failed: {0}")]
	Sqlite(#[from] rusqlite::Error),
	/// `PRAGMA integrity_check` reported corruption in the recovered database.
	#[error("SQLite offline recovery integrity check failed: {detail}")]
	IntegrityCheckFailed {
		/// Diagnostic row returned by `PRAGMA integrity_check`.
		detail: String,
	},
	/// A staged journal or WAL file remained after recovery and mode conversion.
	#[error("SQLite offline recovery left an unconsumed companion file")]
	UnconsumedCompanion,
	/// Publishing the recovered file failed. This error retains publication status.
	#[error("SQLite offline recovery publication failed: {0}")]
	Publication(#[from] WriteError),
}

/// A backup failed. Only `Publication(PublishedButNotDurable(_))` means that a
/// complete database was published; every other variant published no destination.
#[derive(Debug, thiserror::Error)]
pub enum BackupError {
	/// The configured page step exceeds SQLite's signed integer range.
	#[error("SQLite backup page step exceeds i32::MAX")]
	InvalidPageStep,
	/// Source snapshot acquisition, copying, or transaction completion failed.
	#[error("SQLite backup snapshot failed: {0}")]
	Snapshot(#[source] TransactionError<BackupCopyError>),
	/// Creating, closing, or inspecting the private output failed.
	#[error("SQLite backup staging failed: {0}")]
	Io(#[from] std::io::Error),
	/// Configuring or closing the destination database failed.
	#[error("SQLite backup destination failed: {0}")]
	Sqlite(#[from] rusqlite::Error),
	/// The time budget expired after copying and before publication.
	#[error("SQLite backup timed out before publication")]
	TimedOut,
	/// Publishing the complete file failed. This error retains publication status.
	#[error("SQLite backup publication failed: {0}")]
	Publication(#[from] WriteError),
}

/// A source snapshot could not be copied completely.
#[derive(Debug, thiserror::Error)]
pub enum BackupCopyError {
	/// SQLite rejected snapshot acquisition or a backup step.
	#[error("SQLite backup copy failed: {0}")]
	Sqlite(#[from] rusqlite::Error),
	/// The pinned snapshot exceeds the configured size bound.
	#[error("SQLite backup exceeds its byte limit of {max_size}")]
	SizeLimitExceeded {
		/// Configured maximum snapshot size in bytes.
		max_size: u64,
	},
	/// The progress callback requested cancellation.
	#[error("SQLite backup was cancelled")]
	Cancelled,
	/// The cooperative duration budget expired.
	#[error("SQLite backup timed out")]
	TimedOut,
	/// SQLite could not acquire a required lock. No automatic retry is performed.
	#[error("SQLite backup could not acquire a required lock")]
	Busy,
	/// SQLite returned an unrecognized backup state.
	#[error("SQLite backup returned an unsupported progress state")]
	UnexpectedState,
}

impl Connection {
	/// Copies one read snapshot to a new database file without replacing an existing
	/// destination. Parent directories must exist and exclude hostile path changes.
	/// The destination must not name this database or its `-journal`, `-wal`, or
	/// `-shm` companion in the database's dir; names are compared without regard
	/// to letter case.
	///
	/// The snapshot starts with the first source read. WAL writers can commit while
	/// copying proceeds; those later commits do not enter this backup. Rollback-journal
	/// writers may wait for the read transaction. A held WAL snapshot can delay WAL
	/// reclamation until copying finishes.
	///
	/// The callback runs before copying and after every step, including the final
	/// step. `Break(())` cancels without publishing. The timeout is cooperative;
	/// the connection's busy timeout still applies during snapshot acquisition.
	/// The operation performs no application callback replay or unbounded lock retry.
	///
	/// A private temporary dir protects partial output. After copying and closing
	/// the database, publication atomically refuses any existing destination. The
	/// complete file requires no WAL or journal companion. A failure or panic before
	/// publication removes temporary output through RAII; process termination can
	/// leave a private temporary dir, but cannot publish a partial destination.
	pub fn backup_to_file(
		&mut self,
		destination: impl AsRef<Path>,
		options: BackupOptions,
		mut progress_fn: impl FnMut(BackupProgress) -> ControlFlow<()>,
	) -> Result<BackupOutcome, BackupError> {
		let pages_per_step = i32::try_from(options.pages_per_step.get())
			.map_err(|_| BackupError::InvalidPageStep)?;
		let started = Instant::now();
		let destination = super::resolve_database_path(destination.as_ref())?;
		let parent_path = destination.parent().ok_or_else(|| {
			std::io::Error::new(
				std::io::ErrorKind::InvalidInput,
				"backup destination must name a file",
			)
		})?;
		let name = destination.file_name().ok_or_else(|| {
			std::io::Error::new(
				std::io::ErrorKind::InvalidInput,
				"backup destination must name a file",
			)
		})?;
		let destination_name = RelativePath::new(name).map_err(std::io::Error::other)?;
		let destination_dir = Dir::open(parent_path)?;
		let source_dir_meta = Dir::open(
			self.path
				.parent()
				.expect("a resolved database path has a parent dir"),
		)?
		.inner
		.dir_metadata()?;
		let destination_dir_meta = destination_dir.inner.dir_metadata()?;
		let source_name = self
			.path
			.file_name()
			.expect("a resolved database path names a file");
		if source_dir_meta.dev() == destination_dir_meta.dev()
			&& source_dir_meta.ino() == destination_dir_meta.ino()
			&& database_file_names(source_name)
				.iter()
				.any(|database_file_name| file_names_match_ignoring_case(database_file_name, name))
		{
			return Err(std::io::Error::new(
				std::io::ErrorKind::InvalidInput,
				"SQLite backup destination conflicts with the source database or its companion paths",
			)
			.into());
		}
		// Reject an existing destination before spending work on a snapshot. The
		// final no-replace rename also handles a competing creator.
		match std::fs::symlink_metadata(&destination) {
			Ok(_) => {
				return Err(std::io::Error::new(
					std::io::ErrorKind::AlreadyExists,
					"SQLite backup destination already exists",
				)
				.into());
			}
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
			Err(error) => return Err(error.into()),
		}
		let temporary = tempfile::Builder::new()
			.prefix(".primitives-sqlite-backup-")
			.tempdir_in(parent_path)?;
		let staged_file = tempfile::NamedTempFile::new_in(temporary.path())?;
		let mut output = rusqlite::Connection::open_with_flags(
			staged_file.path(),
			OpenFlags::SQLITE_OPEN_READ_WRITE
				| OpenFlags::SQLITE_OPEN_NO_MUTEX
				| OpenFlags::SQLITE_OPEN_NOFOLLOW,
		)?;
		output.busy_timeout(Duration::ZERO)?;
		output.pragma_update(None, "journal_mode", "delete")?;
		output.pragma_update(None, "synchronous", "EXTRA")?;
		#[cfg(target_vendor = "apple")]
		output.pragma_update(None, "fullfsync", true)?;
		let outcome = self
			.read(|tx| {
				// BEGIN DEFERRED alone does not pin a snapshot. This schema read starts
				// the read transaction before the page count and first backup step.
				tx.owner
					.raw()
					.query_row("SELECT 1 FROM main.sqlite_schema LIMIT 1", [], |row| {
						row.get::<_, i64>(0)
					})
					.optional()?;
				let (page_count, page_size): (u32, u32) = tx.owner.with_library_control(|db| {
					Ok((
						db.pragma_query_value(None, "page_count", |row| row.get(0))?,
						db.pragma_query_value(None, "page_size", |row| row.get(0))?,
					))
				})?;
				// SQLite materializes one schema page when it backs up an empty database.
				let page_count = page_count.max(1);
				let size = u64::from(page_count) * u64::from(page_size);
				if size > options.max_size {
					return Err(BackupCopyError::SizeLimitExceeded {
						max_size: options.max_size,
					});
				}
				let mut progress = BackupProgress {
					copied_pages: 0,
					total_pages: page_count,
				};
				if progress_fn(progress).is_break() {
					return Err(BackupCopyError::Cancelled);
				}
				let backup = Backup::new(tx.owner.raw(), &mut output)?;
				loop {
					if started.elapsed() >= options.timeout {
						return Err(BackupCopyError::TimedOut);
					}
					let state = backup.step(pages_per_step)?;
					let current = backup.progress();
					progress.copied_pages = if state == StepResult::Done {
						page_count
					} else {
						(current.pagecount - current.remaining).max(0) as u32
					};
					if progress_fn(progress).is_break() {
						return Err(BackupCopyError::Cancelled);
					}
					match state {
						StepResult::Done => break,
						StepResult::More => {}
						StepResult::Busy | StepResult::Locked => return Err(BackupCopyError::Busy),
						_ => return Err(BackupCopyError::UnexpectedState),
					}
				}
				Ok(BackupOutcome {
					pages: page_count,
					size,
				})
			})
			.map_err(BackupError::Snapshot)?;
		// The copied header can describe WAL mode. Complete the journal-mode
		// transition before closing so that the published file is self-contained.
		let mode: String =
			output.pragma_update_and_check(None, "journal_mode", "delete", |row| row.get(0))?;
		if mode != "delete" {
			return Err(BackupError::Sqlite(rusqlite::Error::InvalidQuery));
		}
		output
			.close()
			.map_err(|(_, error)| BackupError::Sqlite(error))?;
		if started.elapsed() >= options.timeout {
			return Err(BackupError::TimedOut);
		}
		let staged_dir = Dir::open(temporary.path())?;
		let staged_name = RelativePath::new(
			staged_file
				.path()
				.file_name()
				.expect("temporary file has a name"),
		)
		.map_err(std::io::Error::other)?;
		destination_dir.move_file_atomically(
			&destination_name,
			FileMoveSource {
				dir: &staged_dir,
				path: &staged_name,
			},
			WriteOptions {
				permissions: options.permissions,
				mode: WriteMode::CreateNew,
				durability: options.durability,
			},
		)?;
		Ok(outcome)
	}
}

/// Copies an offline SQLite database and any sibling `-journal` or `-wal` files
/// into a private staging dir, replays recovery there, verifies `PRAGMA integrity_check`,
/// and atomically publishes a standalone rollback-journal database at `paths.destination`.
///
/// The source database and its companion journals must form a stable offline
/// snapshot with no active writer and no multi-database transaction across attached
/// files. This function verifies that the source files' identities, byte lengths,
/// and modification timestamps remain unchanged across the staging copy, and it
/// leaves the source database and its companion files unmodified.
/// Rollback journals with a possible super-journal footer are rejected before
/// SQLite opens the staged database, including footers with invalid checksums.
/// Journal and WAL size declarations are checked before replay. SQLite validates
/// their remaining structure and checksums during recovery.
///
/// The source and destination parent dirs must exist and exclude hostile path
/// changes: the staging dir is created in the destination parent dir, and SQLite
/// opens the staged database by path. The function confirms that the staging
/// path still names its staging dir immediately before SQLite opens it.
///
/// Symbolic links in the final path component of the source database, its
/// `-journal`, `-wal`, or `-shm` companions, or the destination are rejected.
/// The destination must not name the source database or one of those companions
/// in the source dir; names are compared without regard to letter case.
/// Any existing destination entry is rejected before staging and at publication.
/// The progress callback runs before copying starts and after each bounded copy
/// chunk; returning `ControlFlow::Break(())` cancels before publication.
///
/// ```no_run
/// use primitives_rs::db::sqlite::backup::{
///     OfflineRecoveryOptions, OfflineRecoveryPaths, recover_offline_to_file,
/// };
/// use primitives_rs::fs::{Durability, Permissions, UnixMode};
/// use std::{ops::ControlFlow, path::Path, time::Duration};
/// # fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let completed = recover_offline_to_file(
///     OfflineRecoveryPaths {
///         source: Path::new("./crashed/app.db"),
///         destination: Path::new("./recovered/app.db"),
///     },
///     OfflineRecoveryOptions {
///         max_size: 512 * 1024 * 1024,
///         timeout: Duration::from_secs(60),
///         permissions: Permissions::Unix(UnixMode::OWNER_READ_WRITE),
///         durability: Durability::FileAndDir,
///     },
///     |_| ControlFlow::Continue(()),
/// )?;
/// println!("Recovered {} pages ({} bytes)", completed.pages, completed.size);
/// # Ok(())
/// # }
/// ```
pub fn recover_offline_to_file(
	paths: OfflineRecoveryPaths<'_>,
	options: OfflineRecoveryOptions,
	mut progress_fn: impl FnMut(OfflineRecoveryProgress) -> ControlFlow<()>,
) -> Result<BackupOutcome, OfflineRecoveryError> {
	if paths.source.as_os_str().is_empty()
		|| paths.source == Path::new(":memory:")
		|| paths.destination.as_os_str().is_empty()
		|| paths.destination == Path::new(":memory:")
	{
		return Err(OfflineRecoveryError::NonPersistentPath);
	}
	let started = Instant::now();
	let source_path = super::resolve_database_path(paths.source)?;
	let destination_path = super::resolve_database_path(paths.destination)?;
	let source_parent = source_path.parent().ok_or_else(|| {
		io::Error::new(
			io::ErrorKind::InvalidInput,
			"SQLite recovery source must name a file",
		)
	})?;
	let source_file_name = source_path.file_name().ok_or_else(|| {
		io::Error::new(
			io::ErrorKind::InvalidInput,
			"SQLite recovery source must name a file",
		)
	})?;
	let destination_parent = destination_path.parent().ok_or_else(|| {
		io::Error::new(
			io::ErrorKind::InvalidInput,
			"SQLite recovery destination must name a file",
		)
	})?;
	let destination_file_name = destination_path.file_name().ok_or_else(|| {
		io::Error::new(
			io::ErrorKind::InvalidInput,
			"SQLite recovery destination must name a file",
		)
	})?;

	let source_name = RelativePath::new(source_file_name).map_err(io::Error::other)?;
	let journal_name = companion_relative_path(source_file_name, "-journal")?;
	let wal_name = companion_relative_path(source_file_name, "-wal")?;
	let shm_name = companion_relative_path(source_file_name, "-shm")?;
	let destination_name = RelativePath::new(destination_file_name).map_err(io::Error::other)?;

	let source_dir = Dir::open(source_parent)?;
	let destination_dir = Dir::open(destination_parent)?;
	let source_dir_meta = source_dir.inner.dir_metadata()?;
	let destination_dir_meta = destination_dir.inner.dir_metadata()?;
	if source_dir_meta.dev() == destination_dir_meta.dev()
		&& source_dir_meta.ino() == destination_dir_meta.ino()
		&& database_file_names(source_file_name)
			.iter()
			.any(|database_file_name| {
				file_names_match_ignoring_case(database_file_name, destination_file_name)
			}) {
		return Err(io::Error::new(
			io::ErrorKind::InvalidInput,
			"SQLite recovery destination conflicts with the source database or its companion paths",
		)
		.into());
	}

	match destination_dir
		.inner
		.symlink_metadata(destination_name.as_path())
	{
		Ok(_) => {
			return Err(io::Error::new(
				io::ErrorKind::AlreadyExists,
				"SQLite recovery destination already exists",
			)
			.into());
		}
		Err(error) if error.kind() == io::ErrorKind::NotFound => {}
		Err(error) => return Err(error.into()),
	}

	match source_dir.inner.symlink_metadata(shm_name.as_path()) {
		Ok(metadata) if metadata.is_file() => {}
		Ok(_) => return Err(crate::fs::invalid_entry().into()),
		Err(error) if error.kind() == io::ErrorKind::NotFound => {}
		Err(error) => return Err(error.into()),
	}

	let mut main_source = open_required_source_file(&source_dir, &source_name, "recovered.db")?;
	let mut journal_source =
		open_optional_source_file(&source_dir, &journal_name, "recovered.db-journal")?;
	let mut wal_source = open_optional_source_file(&source_dir, &wal_name, "recovered.db-wal")?;

	let journal_len = journal_source.as_ref().map_or(0, |entry| entry.stamp.len);
	let wal_len = wal_source.as_ref().map_or(0, |entry| entry.stamp.len);
	let total_bytes = main_source
		.stamp
		.len
		.checked_add(journal_len)
		.and_then(|sum| sum.checked_add(wal_len))
		.ok_or(OfflineRecoveryError::SizeLimitExceeded {
			max_size: options.max_size,
		})?;
	if total_bytes > options.max_size {
		return Err(OfflineRecoveryError::SizeLimitExceeded {
			max_size: options.max_size,
		});
	}

	if progress_fn(OfflineRecoveryProgress {
		copied_bytes: 0,
		total_bytes,
	})
	.is_break()
	{
		return Err(OfflineRecoveryError::Cancelled);
	}
	if started.elapsed() >= options.timeout {
		return Err(OfflineRecoveryError::TimedOut);
	}

	let temporary = tempfile::Builder::new()
		.prefix(".primitives-sqlite-backup-")
		.tempdir_in(destination_parent)?;
	let staged_dir = Dir::open(temporary.path())?;
	let mut copied_bytes = 0u64;
	for entry in [
		Some(&mut main_source),
		journal_source.as_mut(),
		wal_source.as_mut(),
	]
	.into_iter()
	.flatten()
	{
		stage_source_file(
			&staged_dir,
			entry,
			&started,
			options.timeout,
			total_bytes,
			&mut copied_bytes,
			&mut progress_fn,
		)?;
	}
	if copied_bytes != total_bytes {
		return Err(OfflineRecoveryError::SourceModified);
	}

	verify_source_file_unchanged(&source_dir, &source_name, Some(&main_source))?;
	verify_source_file_unchanged(&source_dir, &journal_name, journal_source.as_ref())?;
	verify_source_file_unchanged(&source_dir, &wal_name, wal_source.as_ref())?;
	drop(main_source);
	drop(journal_source);
	drop(wal_source);

	if started.elapsed() >= options.timeout {
		return Err(OfflineRecoveryError::TimedOut);
	}

	let staged_db_path = temporary.path().join("recovered.db");
	check_recovery_input(&staged_dir, options.max_size, started, options.timeout)?;
	// SQLite opens the staged database by path, so the path must still name the
	// staging dir that this function filled and checked.
	let staged_dir_meta = staged_dir.inner.dir_metadata()?;
	let staged_path_meta = Dir::open(temporary.path())?.inner.dir_metadata()?;
	if staged_path_meta.dev() != staged_dir_meta.dev()
		|| staged_path_meta.ino() != staged_dir_meta.ino()
	{
		return Err(io::Error::other("SQLite recovery staging dir was replaced").into());
	}
	let output = rusqlite::Connection::open_with_flags(
		&staged_db_path,
		OpenFlags::SQLITE_OPEN_READ_WRITE
			| OpenFlags::SQLITE_OPEN_NO_MUTEX
			| OpenFlags::SQLITE_OPEN_NOFOLLOW,
	)?;
	// SQLite caps a larger value at its compile-time maximum.
	let length_limit = i32::try_from(options.max_size).unwrap_or(i32::MAX);
	output.set_limit(Limit::SQLITE_LIMIT_LENGTH, length_limit)?;
	output.set_limit(Limit::SQLITE_LIMIT_SQL_LENGTH, length_limit)?;
	let timeout = options.timeout;
	output.progress_handler(1000, Some(move || started.elapsed() >= timeout))?;
	let recovered = (|| -> Result<u32, OfflineRecoveryError> {
		output.busy_timeout(Duration::ZERO)?;
		output.pragma_update(None, "synchronous", "EXTRA")?;
		#[cfg(target_vendor = "apple")]
		output.pragma_update(None, "fullfsync", true)?;

		// Reading sqlite_schema acquires a shared lock on the staged pager, which
		// replays a hot rollback journal or recovers the WAL before mode conversion.
		output
			.query_row("SELECT 1 FROM main.sqlite_schema LIMIT 1", [], |row| {
				row.get::<_, i64>(0)
			})
			.optional()?;
		check_recovered_size(&output, options.max_size)?;
		let mode: String =
			output.pragma_update_and_check(None, "journal_mode", "delete", |row| row.get(0))?;
		if mode != "delete" {
			return Err(OfflineRecoveryError::Sqlite(rusqlite::Error::InvalidQuery));
		}
		let initial_pages: u32 = output.pragma_query_value(None, "page_count", |row| row.get(0))?;
		if initial_pages == 0 {
			let page_size = output.pragma_query_value(None, "page_size", |row| row.get(0))?;
			check_page_bound(1, page_size, options.max_size)?;
			let user_version: i32 =
				output.pragma_query_value(None, "user_version", |row| row.get(0))?;
			output.pragma_update(None, "user_version", user_version)?;
		}
		let integrity: String =
			output.pragma_query_value(None, "integrity_check", |row| row.get(0))?;
		if integrity != "ok" {
			return Err(OfflineRecoveryError::IntegrityCheckFailed { detail: integrity });
		}
		let pages: u32 = output.pragma_query_value(None, "page_count", |row| row.get(0))?;
		check_recovered_size(&output, options.max_size)?;
		Ok(pages)
	})();
	output.progress_handler(0, None::<fn() -> bool>)?;
	let recovered = recovered.map_err(|error| match error {
		// The progress handler interrupts SQLite only after the deadline.
		OfflineRecoveryError::Sqlite(rusqlite::Error::SqliteFailure(failure, _))
			if failure.code == rusqlite::ErrorCode::OperationInterrupted =>
		{
			OfflineRecoveryError::TimedOut
		}
		error => error,
	})?;
	output
		.close()
		.map_err(|(_, error)| OfflineRecoveryError::Sqlite(error))?;

	for companion in [
		"recovered.db-journal",
		"recovered.db-wal",
		"recovered.db-shm",
	] {
		match staged_dir.inner.symlink_metadata(companion) {
			Err(error) if error.kind() == io::ErrorKind::NotFound => {}
			Ok(_) => return Err(OfflineRecoveryError::UnconsumedCompanion),
			Err(error) => return Err(error.into()),
		}
	}

	let staged_name = RelativePath::new("recovered.db").map_err(io::Error::other)?;
	let size = staged_dir
		.inner
		.symlink_metadata(staged_name.as_path())?
		.len();
	if size > options.max_size {
		return Err(OfflineRecoveryError::SizeLimitExceeded {
			max_size: options.max_size,
		});
	}
	if started.elapsed() >= options.timeout {
		return Err(OfflineRecoveryError::TimedOut);
	}

	destination_dir.move_file_atomically(
		&destination_name,
		FileMoveSource {
			dir: &staged_dir,
			path: &staged_name,
		},
		WriteOptions {
			permissions: options.permissions,
			mode: WriteMode::CreateNew,
			durability: options.durability,
		},
	)?;
	Ok(BackupOutcome {
		pages: recovered,
		size,
	})
}

const JOURNAL_MAGIC: [u8; 8] = [0xd9, 0xd5, 0x05, 0xf9, 0x20, 0xa1, 0x63, 0xd7];

fn check_recovery_input(
	dir: &Dir,
	max_size: u64,
	started: Instant,
	timeout: Duration,
) -> Result<(), OfflineRecoveryError> {
	let mut db = dir.open_file(&RelativePath::new("recovered.db").map_err(io::Error::other)?)?;
	// A crash before the first commit can leave an empty or unfinished header.
	// SQLite may repair that header from a hot journal. Bound legacy journals
	// without a page-size field by the maximum supported page size in that case.
	let mut header = [0u8; 100];
	let header_len = db.metadata()?.len().min(header.len() as u64) as usize;
	db.read_exact(&mut header[..header_len])?;
	let header_page_size = if header_len == header.len() && &header[..16] == b"SQLite format 3\0" {
		let encoded_page_size = u16::from_be_bytes([header[16], header[17]]);
		let candidate = if encoded_page_size == 1 {
			65536
		} else {
			u32::from(encoded_page_size)
		};
		check_page_size(candidate).ok().map(|()| candidate)
	} else {
		None
	};
	if let Some(page_size) = header_page_size {
		check_page_bound(read_u32(&header[28..32]), page_size, max_size)?;
	}
	let page_size = header_page_size.unwrap_or(65536);

	if let Some(mut journal) = open_staged_companion(dir, "recovered.db-journal")? {
		let size = journal.metadata()?.len();
		if size >= 16 {
			let mut footer = [0u8; 16];
			journal.seek(SeekFrom::End(-16))?;
			journal.read_exact(&mut footer)?;
			let name_len = u64::from(read_u32(&footer[..4]));
			// SQLite can follow this footer before validating the journal header.
			// Reject possible references without interpreting paths or checksums.
			if footer[8..] == JOURNAL_MAGIC && name_len > 0 && name_len <= size - 16 {
				return Err(OfflineRecoveryError::ExternalJournalReference);
			}
		}
		if size >= 28 {
			let mut header = [0u8; 28];
			journal.rewind()?;
			journal.read_exact(&mut header)?;
			if header[..8] == JOURNAL_MAGIC {
				let journal_page_size = read_u32(&header[24..28]);
				let page_size = if journal_page_size == 0 {
					page_size
				} else {
					journal_page_size
				};
				check_page_size(page_size)?;
				// Hot rollback truncates to the first header's original page count
				// and ignores record page numbers beyond that count.
				check_page_bound(read_u32(&header[16..20]), page_size, max_size)?;
			}
		}
	}
	if let Some(mut wal) = open_staged_companion(dir, "recovered.db-wal")? {
		// SQLite writes a valid database header before it writes a WAL, and it
		// cannot change the page size of a database in WAL mode. SQLite therefore
		// never pairs a WAL with an invalid database header or a different page size.
		let Some(database_page_size) = header_page_size else {
			return Err(io::Error::new(
				io::ErrorKind::InvalidData,
				"SQLite WAL requires a database with a valid header",
			)
			.into());
		};
		let size = wal.metadata()?.len();
		if size >= 32 {
			let mut header = [0u8; 32];
			wal.read_exact(&mut header)?;
			if matches!(read_u32(&header[..4]), 0x377f0682 | 0x377f0683) {
				let page_size = read_u32(&header[8..12]);
				check_page_size(page_size)?;
				if page_size != database_page_size {
					return Err(io::Error::new(
						io::ErrorKind::InvalidData,
						"SQLite WAL page size differs from the database page size",
					)
					.into());
				}
				let stride = 24 + u64::from(page_size);
				let mut offset = 32;
				while size - offset >= stride {
					if started.elapsed() >= timeout {
						return Err(OfflineRecoveryError::TimedOut);
					}
					let mut frame = [0u8; 24];
					wal.seek(SeekFrom::Start(offset))?;
					wal.read_exact(&mut frame)?;
					// Bound every complete frame, including stale or uncommitted
					// frames, before SQLite can rebuild an index or checkpoint.
					check_page_bound(read_u32(&frame[..4]), page_size, max_size)?;
					check_page_bound(read_u32(&frame[4..8]), page_size, max_size)?;
					offset += stride;
				}
			}
		}
	}
	Ok(())
}

fn open_staged_companion(dir: &Dir, name: &str) -> io::Result<Option<std::fs::File>> {
	match dir.open_file(&RelativePath::new(name).map_err(io::Error::other)?) {
		Ok(file) => Ok(Some(file)),
		Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
		Err(error) => Err(error),
	}
}

fn read_u32(bytes: &[u8]) -> u32 {
	u32::from_be_bytes(bytes.try_into().expect("fixed SQLite header field"))
}

fn check_page_size(size: u32) -> io::Result<()> {
	if (512..=65536).contains(&size) && size.is_power_of_two() {
		Ok(())
	} else {
		Err(io::Error::new(
			io::ErrorKind::InvalidData,
			"invalid SQLite page size",
		))
	}
}

fn check_page_bound(pages: u32, page_size: u32, max_size: u64) -> Result<(), OfflineRecoveryError> {
	if u64::from(pages) * u64::from(page_size) > max_size {
		Err(OfflineRecoveryError::SizeLimitExceeded { max_size })
	} else {
		Ok(())
	}
}

fn check_recovered_size(
	db: &rusqlite::Connection,
	max_size: u64,
) -> Result<(), OfflineRecoveryError> {
	let pages: u32 = db.pragma_query_value(None, "page_count", |row| row.get(0))?;
	let page_size: u32 = db.pragma_query_value(None, "page_size", |row| row.get(0))?;
	check_page_bound(pages, page_size, max_size)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SourceFileStamp {
	dev: u64,
	ino: u64,
	len: u64,
	modified: cap_std::time::SystemTime,
}

impl SourceFileStamp {
	fn from_metadata(metadata: &cap_std::fs::Metadata) -> io::Result<Self> {
		Ok(Self {
			dev: metadata.dev(),
			ino: metadata.ino(),
			len: metadata.len(),
			modified: metadata.modified()?,
		})
	}
}

struct OpenSourceFile {
	file: std::fs::File,
	stamp: SourceFileStamp,
	staged_name: &'static str,
}

/// Returns the names of a database file and its `-journal`, `-wal`, and `-shm`
/// companions.
fn database_file_names(database_name: &OsStr) -> [OsString; 4] {
	["", "-journal", "-wal", "-shm"].map(|suffix| {
		let mut name = database_name.to_os_string();
		name.push(suffix);
		name
	})
}

/// Reports whether two file names are equal when letter case is ignored. Each
/// character is mapped to uppercase and then to lowercase, so characters that
/// share an uppercase form compare equal. Names that are not valid Unicode are
/// compared with ASCII case folding.
fn file_names_match_ignoring_case(left: &OsStr, right: &OsStr) -> bool {
	match (left.to_str(), right.to_str()) {
		(Some(left), Some(right)) => fold_case(left).eq(fold_case(right)),
		_ => left.eq_ignore_ascii_case(right),
	}
}

fn fold_case(name: &str) -> impl Iterator<Item = char> + '_ {
	name.chars()
		.flat_map(char::to_uppercase)
		.flat_map(char::to_lowercase)
}

fn companion_relative_path(base_name: &OsStr, suffix: &str) -> io::Result<RelativePath> {
	let mut full_name = OsString::from(base_name);
	full_name.push(suffix);
	RelativePath::new(full_name).map_err(io::Error::other)
}

fn open_required_source_file(
	source_dir: &Dir,
	path: &RelativePath,
	staged_name: &'static str,
) -> Result<OpenSourceFile, OfflineRecoveryError> {
	let path_metadata = source_dir.inner.symlink_metadata(path.as_path())?;
	if !path_metadata.is_file() {
		return Err(crate::fs::invalid_entry().into());
	}
	let file = source_dir.open_file(path)?;
	let handle_metadata = cap_std::fs::Metadata::from_file(&file)?;
	let stamp = SourceFileStamp::from_metadata(&path_metadata)?;
	if stamp != SourceFileStamp::from_metadata(&handle_metadata)? {
		return Err(OfflineRecoveryError::SourceModified);
	}
	Ok(OpenSourceFile {
		file,
		stamp,
		staged_name,
	})
}

fn open_optional_source_file(
	source_dir: &Dir,
	path: &RelativePath,
	staged_name: &'static str,
) -> Result<Option<OpenSourceFile>, OfflineRecoveryError> {
	match source_dir.inner.symlink_metadata(path.as_path()) {
		Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
		Err(error) => Err(error.into()),
		Ok(path_metadata) => {
			if !path_metadata.is_file() {
				return Err(crate::fs::invalid_entry().into());
			}
			let file = match source_dir.open_file(path) {
				Ok(file) => file,
				Err(error) if error.kind() == io::ErrorKind::NotFound => {
					return Err(OfflineRecoveryError::SourceModified);
				}
				Err(error) => return Err(error.into()),
			};
			let handle_metadata = cap_std::fs::Metadata::from_file(&file)?;
			let stamp = SourceFileStamp::from_metadata(&path_metadata)?;
			if stamp != SourceFileStamp::from_metadata(&handle_metadata)? {
				return Err(OfflineRecoveryError::SourceModified);
			}
			Ok(Some(OpenSourceFile {
				file,
				stamp,
				staged_name,
			}))
		}
	}
}

fn stage_source_file(
	staged_dir: &Dir,
	source: &mut OpenSourceFile,
	started: &Instant,
	timeout: Duration,
	total_bytes: u64,
	copied_bytes: &mut u64,
	progress_fn: &mut impl FnMut(OfflineRecoveryProgress) -> ControlFlow<()>,
) -> Result<(), OfflineRecoveryError> {
	let mut open_options = cap_std::fs::OpenOptions::new();
	open_options.create_new(true).read(true).write(true);
	#[cfg(unix)]
	{
		use cap_std::fs::OpenOptionsExt;
		open_options.mode(0o600);
	}
	let mut destination = staged_dir
		.inner
		.open_with(source.staged_name, &open_options)?;
	let mut buffer = zeroize::Zeroizing::new([0u8; 8192]);
	let mut file_copied = 0u64;
	loop {
		if started.elapsed() >= timeout {
			return Err(OfflineRecoveryError::TimedOut);
		}
		let read = loop {
			match source.file.read(&mut buffer[..]) {
				Err(error) if error.kind() == io::ErrorKind::Interrupted => {
					if started.elapsed() >= timeout {
						return Err(OfflineRecoveryError::TimedOut);
					}
				}
				Ok(read) => break read,
				Err(error) => return Err(error.into()),
			}
		};
		if read == 0 {
			break;
		}
		file_copied = file_copied
			.checked_add(read as u64)
			.ok_or(OfflineRecoveryError::SourceModified)?;
		*copied_bytes = copied_bytes
			.checked_add(read as u64)
			.ok_or(OfflineRecoveryError::SourceModified)?;
		if file_copied > source.stamp.len || *copied_bytes > total_bytes {
			return Err(OfflineRecoveryError::SourceModified);
		}
		destination.write_all(&buffer[..read])?;
		if progress_fn(OfflineRecoveryProgress {
			copied_bytes: *copied_bytes,
			total_bytes,
		})
		.is_break()
		{
			return Err(OfflineRecoveryError::Cancelled);
		}
	}
	if file_copied != source.stamp.len {
		return Err(OfflineRecoveryError::SourceModified);
	}
	destination.flush()?;
	Ok(())
}

fn verify_source_file_unchanged(
	source_dir: &Dir,
	path: &RelativePath,
	expected: Option<&OpenSourceFile>,
) -> Result<(), OfflineRecoveryError> {
	match (expected, source_dir.inner.symlink_metadata(path.as_path())) {
		(None, Err(error)) if error.kind() == io::ErrorKind::NotFound => Ok(()),
		(None, Ok(_)) => Err(OfflineRecoveryError::SourceModified),
		(None, Err(error)) => Err(error.into()),
		(Some(source), Ok(path_metadata)) => {
			if !path_metadata.is_file() {
				return Err(OfflineRecoveryError::SourceModified);
			}
			let handle_metadata = cap_std::fs::Metadata::from_file(&source.file)?;
			if SourceFileStamp::from_metadata(&path_metadata)? != source.stamp
				|| SourceFileStamp::from_metadata(&handle_metadata)? != source.stamp
			{
				return Err(OfflineRecoveryError::SourceModified);
			}
			Ok(())
		}
		(Some(_), Err(error)) if error.kind() == io::ErrorKind::NotFound => {
			Err(OfflineRecoveryError::SourceModified)
		}
		(Some(_), Err(error)) => Err(error.into()),
	}
}
