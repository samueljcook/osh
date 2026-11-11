//! Cooperative local process locks with persistent files and diagnostic records.
//!
//! Enable `process-lock` on Unix or Windows. Acquisition attempts an exclusive
//! OS file lock once and returns [`LockError::Contended`] if another handle holds
//! that lock. A returned [`ProcessLock`] retains ownership until its last duplicate
//! closes. Process suspension does not release ownership. Process termination
//! closes its handles; inherited handles in other processes can retain ownership.
//!
//! Every participant must use the same dedicated lock file. This module never
//! deletes or atomically replaces that file: replacement would let participants
//! lock different files at the same path. Applications must keep the lock file and
//! its ancestor directories stable throughout every participant's lifetime. The
//! directory must not permit hostile modifications. Acquisition refuses symbolic
//! links, multiple hard links, special files, and observed identity changes.
//!
//! The OS lock determines ownership. The file's PID record can be stale, partial,
//! or unreadable, so diagnostic data must never authorize lock recovery or process
//! termination. Acquisition repairs recognized incomplete records after obtaining
//! the OS lock and refuses unrelated contents. Empty files and exact record
//! prefixes are recognized, so the path must be dedicated to this protocol.
//!
//! These operations use synchronous filesystem calls without waiting for lock
//! availability. Applications choose whether and when to retry contention. Use
//! local filesystems with reliable OS locks; this protocol provides no distributed
//! lease or protection against uncooperative writers. Supported platforms are
//! Linux, FreeBSD, Apple platforms, and Windows. Other Unix platforms return
//! [`LockError::UnsupportedPlatform`] before opening or creating a file.
//!
//! ```no_run
//! use primitives_rs::fs::{Dir, Permissions, RelativePath, UnixMode};
//! use primitives_rs::processes::local_locks::{LockOptions, ProcessLock};
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let dir = Dir::open("./application-state")?;
//! let path = RelativePath::new("update.lock")?;
//! let guard = ProcessLock::try_acquire(&dir, &path, LockOptions {
//!     permissions: Permissions::Unix(UnixMode::OWNER_READ_WRITE),
//! })?;
//! // Perform the application's coordinated work while retaining the guard.
//! drop(guard);
//! # Ok(())
//! # }
//! ```

use std::ffi::OsStr;
use std::fmt;
use std::fs::{File, TryLockError};
use std::io::{self, Seek, Write};

use cap_fs_ext::{FollowSymlinks, MetadataExt, OpenOptionsFollowExt, OpenOptionsSyncExt};
use cap_std::fs::{Dir as CapDir, OpenOptions};

use crate::fs::{Dir, Permissions, RelativePath, apply_permissions};
use crate::io::bounded_reads::{ReadError, ReadOptions, read_to_end};

const RECORD_PREFIX: &[u8] = b"primitives-process-lock-v1\npid=";
// A u32 PID needs at most ten decimal digits followed by one newline.
const MAX_RECORD_SIZE: usize = RECORD_PREFIX.len() + 11;

#[cfg(all(test, unix))]
mod tests;

/// Creation policy for a dedicated lock file.
#[derive(Clone, Copy, Debug)]
pub struct LockOptions {
	/// Permissions for a new file. Existing files retain their permissions.
	/// Every participant needs read and write access to the file.
	pub permissions: Permissions,
}

/// A failure to acquire a local process lock.
#[derive(Debug)]
pub enum LockError {
	/// Another open handle held the OS lock when acquisition was attempted.
	Contended {
		/// A PID read from a complete diagnostic record, when available. This
		/// value can be stale even while the OS lock is held by another process.
		recorded_process_id: Option<u32>,
	},
	/// The entry is not a regular file with exactly one hard link.
	InvalidFile,
	/// The file contains unrelated, malformed, or oversized data.
	InvalidRecord,
	/// The opened file no longer matches its directory entry.
	PathChanged,
	/// This platform does not provide the supported handle-owned lock semantics.
	UnsupportedPlatform,
	/// Opening, inspecting, locking, or writing the file failed.
	Io(io::Error),
}

impl fmt::Display for LockError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Contended {
				recorded_process_id: Some(pid),
			} => write!(f, "process lock is held; diagnostic record names PID {pid}"),
			Self::Contended {
				recorded_process_id: None,
			} => f.write_str("process lock is held; no complete owner record is available"),
			Self::InvalidFile => f.write_str("process lock requires a singly linked regular file"),
			Self::InvalidRecord => f.write_str("process lock file contains an unrecognized record"),
			Self::PathChanged => {
				f.write_str("process lock file identity changed during acquisition")
			}
			Self::UnsupportedPlatform => {
				f.write_str("handle-owned process locks are unsupported on this platform")
			}
			Self::Io(source) => write!(f, "process lock I/O failed: {source}"),
		}
	}
}

impl std::error::Error for LockError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self {
			Self::Io(source) => Some(source),
			_ => None,
		}
	}
}

impl From<io::Error> for LockError {
	fn from(source: io::Error) -> Self {
		Self::Io(source)
	}
}

/// An acquired exclusive OS lock on a persistent local file.
///
/// Dropping a guard closes its handle without explicitly unlocking or removing
/// the file. All duplicated or inherited handles must close before ownership ends.
/// The file handle is private so that callers cannot unlock through a duplicate.
#[derive(Debug)]
#[must_use = "retain the guard throughout the coordinated operation"]
pub struct ProcessLock {
	file: File,
}

impl ProcessLock {
	/// Attempts acquisition once through an open directory and a parsed relative path.
	///
	/// Parent directories must exist. A new lock file receives the requested
	/// permissions; an existing file retains its permissions. Record writes occur
	/// only after locking and validating the file. A failure closes the acquired
	/// handle and may leave an empty or partial record that a later holder can repair.
	/// Records are diagnostic and are not synchronized to persistent storage.
	pub fn try_acquire(
		dir: &Dir,
		path: &RelativePath,
		options: LockOptions,
	) -> Result<Self, LockError> {
		ensure_supported_platform()?;
		options.permissions.check_supported()?;
		let (parent, name) = dir.open_parent_dir(path)?;
		let (mut file, created) = open_lock_file(&parent, name, options)?;
		validate_lock_path(dir, path, &file)?;
		match file.try_lock() {
			Ok(()) => {}
			Err(TryLockError::WouldBlock) => {
				return Err(LockError::Contended {
					recorded_process_id: read_record(&mut file)
						.ok()
						.and_then(|record| parse_process_id(&record)),
				});
			}
			Err(TryLockError::Error(error)) => return Err(error.into()),
		}
		validate_lock_path(dir, path, &file)?;
		let record = read_record(&mut file)?;
		if !is_complete_or_partial_record(&record) {
			return Err(LockError::InvalidRecord);
		}
		if created {
			apply_permissions(&file, options.permissions)?;
		}
		file.set_len(0)?;
		file.rewind()?;
		file.write_all(RECORD_PREFIX)?;
		writeln!(file, "{}", std::process::id())?;
		validate_lock_path(dir, path, &file)?;
		Ok(Self { file })
	}

	/// Duplicates this guard's handle to retain the same acquired lock.
	/// Dropping either guard leaves the lock held until every duplicate closes.
	pub fn try_clone(&self) -> io::Result<Self> {
		Ok(Self {
			file: self.file.try_clone()?,
		})
	}
}

fn ensure_supported_platform() -> Result<(), LockError> {
	if cfg!(any(
		windows,
		target_os = "linux",
		target_os = "freebsd",
		target_vendor = "apple"
	)) {
		Ok(())
	} else {
		Err(LockError::UnsupportedPlatform)
	}
}

fn open_lock_file(
	parent: &CapDir,
	name: &OsStr,
	policy: LockOptions,
) -> Result<(File, bool), LockError> {
	let mut options = OpenOptions::new();
	options
		.read(true)
		.write(true)
		.create_new(true)
		.follow(FollowSymlinks::No)
		.nonblock(true);
	#[cfg(unix)]
	if let Permissions::Unix(mode) = policy.permissions {
		use cap_std::fs::OpenOptionsExt;
		options.mode(mode.bits());
	}
	#[cfg(not(unix))]
	let _ = policy;
	match parent.open_with(name, &options) {
		Ok(file) => Ok((file.into_std(), true)),
		Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
			let metadata = parent.symlink_metadata(name)?;
			validate_file_metadata(&metadata)?;
			options.create_new(false);
			Ok((parent.open_with(name, &options)?.into_std(), false))
		}
		Err(error) => Err(error.into()),
	}
}

fn validate_file_metadata(metadata: &cap_std::fs::Metadata) -> Result<(), LockError> {
	if metadata.is_file() && metadata.nlink() == 1 {
		Ok(())
	} else {
		Err(LockError::InvalidFile)
	}
}

fn validate_lock_path(dir: &Dir, path: &RelativePath, file: &File) -> Result<(), LockError> {
	let opened = cap_std::fs::Metadata::from_file(file)?;
	validate_file_metadata(&opened)?;
	// Resolve from the root again to detect replacement of a parent directory.
	let (parent, name) = dir.open_parent_dir(path)?;
	let current = match parent.symlink_metadata(name) {
		Err(error) if error.kind() == io::ErrorKind::NotFound => {
			return Err(LockError::PathChanged);
		}
		result => result?,
	};
	validate_file_metadata(&current)?;
	if opened.dev() == current.dev() && opened.ino() == current.ino() {
		Ok(())
	} else {
		Err(LockError::PathChanged)
	}
}

fn read_record(file: &mut File) -> Result<Vec<u8>, LockError> {
	if file.metadata()?.len() > MAX_RECORD_SIZE as u64 {
		return Err(LockError::InvalidRecord);
	}
	file.rewind()?;
	let mut bytes = Vec::new();
	read_to_end(
		file,
		&mut bytes,
		ReadOptions {
			max_size: MAX_RECORD_SIZE,
		},
	)
	.map_err(|error| match error {
		ReadError::Io(error) => LockError::Io(error),
		ReadError::LimitExceeded { .. } => LockError::InvalidRecord,
		ReadError::AllocationFailed => LockError::Io(io::Error::from(io::ErrorKind::OutOfMemory)),
	})?;
	Ok(bytes)
}

fn is_complete_or_partial_record(record: &[u8]) -> bool {
	RECORD_PREFIX.starts_with(record)
		|| record.strip_prefix(RECORD_PREFIX).is_some_and(|tail| {
			let digits = tail.strip_suffix(b"\n").unwrap_or(tail);
			parse_pid_digits(digits).is_some()
		})
}

fn parse_process_id(record: &[u8]) -> Option<u32> {
	parse_pid_digits(record.strip_prefix(RECORD_PREFIX)?.strip_suffix(b"\n")?)
}

fn parse_pid_digits(digits: &[u8]) -> Option<u32> {
	if digits.first() == Some(&b'0') || !digits.iter().all(u8::is_ascii_digit) {
		return None;
	}
	std::str::from_utf8(digits).ok()?.parse().ok()
}
