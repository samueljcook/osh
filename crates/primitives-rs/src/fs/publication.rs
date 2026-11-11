use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Seek, Write};

use crate::io::cooperative::IoOperation;

use cap_fs_ext::MetadataExt;
use cap_std::fs::{Dir as CapDir, File, OpenOptions};

use super::dir::{FileSync, apply_permissions, open_regular_file, sync_dir};
use super::{
	Dir, Durability, FileMoveSource, Permissions, RelativePath, WriteError, WriteFromReaderOptions,
	WriteMode, WriteOptions, WriteOutcome,
};

impl Dir {
	/// Publishes complete bytes by renaming a newly created sibling file.
	///
	/// Parent directories must exist. Replacement destinations must be regular files
	/// with one hard link. Replacement does not preserve ownership, ACLs, or other
	/// metadata from the previous file. `PlatformDefault` uses ordinary creation
	/// permissions; an explicit Unix mode is applied before any publication.
	/// Unix temporary files start with mode `0600`, subject to the process umask;
	/// inherited ACLs are not removed. `PlatformDefault` uses a separate empty file
	/// to determine the ordinary creation mode without exposing input contents or
	/// changing the process umask.
	///
	/// `IfChanged` compares bounded chunks without allocating another file-sized
	/// buffer. An equal file is synchronized if requested, but its contents, inode,
	/// permissions, and modification time remain unchanged. An explicit permission
	/// mismatch requires replacement even when bytes match.
	///
	/// `CreateNew` atomically refuses every existing destination, including links
	/// and directories. An existing destination returns `BeforePublication` with
	/// `AlreadyExists`; its contents, permissions, and durability are not checked.
	/// Concurrent create-only writers cannot overwrite each other's published file.
	/// Native no-replace rename support is required; an unsupported platform or
	/// filesystem returns `Unsupported` without using a non-atomic substitute.
	///
	/// Other writers can replace the name after a comparison or publication. Use
	/// external coordination for compound updates. The containing dir must
	/// not be writable by a hostile actor who could replace temporary names.
	pub fn write_file_atomically(
		&self,
		path: &RelativePath,
		bytes: &[u8],
		options: WriteOptions,
	) -> Result<WriteOutcome, WriteError> {
		options.permissions.check_supported()?;
		options.durability.check_supported()?;
		options.mode.check_supported()?;
		let (parent, name) = self.open_parent_dir(path)?;
		let existing = inspect_write_destination(&parent, name, options.mode)?;
		if options.mode == WriteMode::IfChanged
			&& let Some(metadata) = existing
			&& metadata.len() == bytes.len() as u64
			&& permissions_match(&metadata, options.permissions)
			&& let mut file = open_regular_file(&parent, name, file_sync(options.durability))?
			&& file_contents_match(&mut file, bytes)?
			&& path_matches_open_file(&parent, name, &file)?
		{
			if options.durability != Durability::None {
				file.sync_all()?;
			}
			if options.durability == Durability::FileAndDir {
				sync_dir(&parent)?;
			}
			return Ok(WriteOutcome::Unchanged);
		}

		publish_file(&parent, name, options, |file| file.write_all(bytes))
	}

	/// Publishes a bounded reader's complete input through a temporary sibling file.
	///
	/// The reader is consumed once, from its current position through EOF, without
	/// seeking. Fixed-size zeroizing buffers bound library memory use. At the size
	/// limit, one extra byte is read to distinguish EOF from overflow; that byte is
	/// consumed but never written. An overflow reports `BeforePublication` with
	/// `InvalidData`. Interrupted reads are retried. Other reader errors are preserved.
	///
	/// `IfChanged` stores the complete input in the temporary file before comparing
	/// bounded chunks against the destination. Equal contents and requested
	/// permissions preserve the destination's inode and modification time. The
	/// temporary file requires disk space up to `max_size`, even for equal input.
	/// `CreateNew` rejects a destination observed before reading without consuming
	/// the reader and atomically rejects a competing destination at publication.
	///
	/// On Unix, temporary files start with mode `0600`, subject to the process
	/// umask; inherited ACLs are not removed.
	/// Requested publication permissions are applied after the input is complete.
	/// `PlatformDefault` preserves the mode that ordinary creation would select.
	/// Other platforms use their ordinary inherited creation permissions.
	/// Before-publication failures preserve the destination; temporary-file removal
	/// is attempted on errors and unwinding. A dir synchronization failure after
	/// rename reports `PublishedButNotDurable`, as in [`Self::write_file_atomically`].
	/// That method's path, link, metadata, and concurrent-writer rules also apply.
	///
	/// An optional `operation` reports cumulative source consumption and checks
	/// cancellation between reads and writes, during comparison, and before rename.
	/// Each source read is at most 8192 bytes. Comparison reads do not add progress.
	/// Accepted cancellation reports `IoControl(IoControlError::Cancelled)` and
	/// preserves the destination. Publication and cancellation use one atomic state
	/// transition immediately before rename. After that transition, cancellation
	/// returns `TooLate`; the write reports its actual publication result, including
	/// `PublishedButNotDurable` if the subsequent dir synchronization fails.
	/// A syscall or progress callback already in progress must return before the
	/// writer can act on cancellation. Callback errors prevent publication, and
	/// their `Interrupted` kind is not retried. Callback panics unwind normally;
	/// temporary-file cleanup is attempted during unwinding.
	///
	/// ```no_run
	/// use primitives_rs::fs::{Dir, RelativePath, WriteFromReaderOptions, WriteOptions,
	///     WriteMode, Permissions, Durability};
	/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
	/// let dir = Dir::open("media")?;
	/// let mut input = std::fs::File::open("incoming.bin")?;
	/// dir.write_file_from_reader_atomically(
	///     &RelativePath::new("asset.bin")?, &mut input,
	///     WriteFromReaderOptions {
	///         max_size: 10 * 1024 * 1024 * 1024, operation: None,
	///         file: WriteOptions { mode: WriteMode::CreateNew,
	///             permissions: Permissions::PlatformDefault,
	///             durability: Durability::FileAndDir },
	///     },
	/// )?;
	/// # Ok(())
	/// # }
	/// ```
	pub fn write_file_from_reader_atomically<R: Read + ?Sized>(
		&self,
		path: &RelativePath,
		reader: &mut R,
		mut options: WriteFromReaderOptions<'_>,
	) -> Result<WriteOutcome, WriteError> {
		let policy = options.file;
		let mut operation = options.operation.take();
		check_cancellation(operation.as_ref())?;
		policy.permissions.check_supported()?;
		policy.durability.check_supported()?;
		policy.mode.check_supported()?;
		let (parent, name) = self.open_parent_dir(path)?;
		inspect_write_destination(&parent, name, policy.mode)?;
		let mut temporary = TemporaryFile::create(&parent, policy.permissions)?;
		copy_bounded(
			reader,
			&mut temporary.file,
			options.max_size,
			operation.as_mut(),
		)?;
		check_cancellation(operation.as_ref())?;
		if policy.mode == WriteMode::IfChanged
			&& let Some(metadata) = inspect_destination(&parent, name)?
			&& metadata.len() == temporary.file.metadata()?.len()
			&& permissions_match(&metadata, policy.permissions)
		{
			let mut existing = open_regular_file(&parent, name, file_sync(policy.durability))?;
			temporary.file.rewind()?;
			if stream_contents_match(
				&mut temporary.file,
				&mut existing,
				metadata.len(),
				operation.as_ref(),
			)? && path_matches_open_file(&parent, name, &existing)?
			{
				check_cancellation(operation.as_ref())?;
				if policy.durability != Durability::None {
					existing.sync_all()?;
				}
				if policy.durability == Durability::FileAndDir {
					sync_dir(&parent)?;
				}
				if let Some(operation) = operation {
					operation.finish_cancellable_work()?;
				}
				return Ok(WriteOutcome::Unchanged);
			}
		}
		publish_temporary_file(temporary, name, policy, operation.as_ref())
	}

	/// Publishes an existing regular file by atomically moving its directory entry.
	///
	/// The source and destination must be on the same filesystem. Both parent dirs
	/// must exist and must exclude hostile name replacement. The caller must exclude
	/// concurrent source writes until this method returns. Source links, multiple
	/// hard links, and source/destination aliases are rejected.
	///
	/// Explicit permissions are applied to the source before publication; a failure
	/// can therefore leave changed source permissions. `PlatformDefault` preserves
	/// source permissions. `IfChanged` preserves both files when the destination
	/// already matches. Every other success removes the source name. Cross-filesystem
	/// moves fail without copying. `FileAndDir` synchronizes both parent dirs after
	/// publication; a synchronization failure reports `PublishedButNotDurable`.
	pub fn move_file_atomically(
		&self,
		destination: &RelativePath,
		source: FileMoveSource<'_>,
		options: WriteOptions,
	) -> Result<WriteOutcome, WriteError> {
		options.permissions.check_supported()?;
		options.durability.check_supported()?;
		options.mode.check_supported()?;
		let (source_parent, source_name) = source.dir.open_parent_dir(source.path)?;
		let mut file =
			open_regular_file(&source_parent, source_name, file_sync(options.durability))?;
		let source_metadata = file.metadata()?;
		if source_metadata.nlink() != 1 {
			return Err(super::invalid_entry().into());
		}
		let (parent, name) = self.open_parent_dir(destination)?;
		let existing = inspect_write_destination(&parent, name, options.mode)?;
		if let Some(metadata) = existing {
			if metadata.dev() == source_metadata.dev() && metadata.ino() == source_metadata.ino() {
				return Err(io::Error::new(
					io::ErrorKind::InvalidInput,
					"move source and destination refer to the same file",
				)
				.into());
			}
			if options.mode == WriteMode::IfChanged
				&& metadata.len() == source_metadata.len()
				&& permissions_match(&metadata, options.permissions)
			{
				let mut destination_file =
					open_regular_file(&parent, name, file_sync(options.durability))?;
				if stream_contents_match(
					&mut file,
					&mut destination_file,
					source_metadata.len(),
					None,
				)? && path_matches_open_file(&parent, name, &destination_file)?
					&& path_matches_open_file(&source_parent, source_name, &file)?
				{
					if options.durability != Durability::None {
						destination_file.sync_all()?;
					}
					if options.durability == Durability::FileAndDir {
						sync_dir(&parent)?;
					}
					return Ok(WriteOutcome::Unchanged);
				}
			}
		}
		apply_permissions(&file.try_clone()?.into_std(), options.permissions)?;
		if options.durability != Durability::None {
			file.sync_all()?;
		}
		if !path_matches_open_file(&source_parent, source_name, &file)? {
			return Err(super::invalid_entry().into());
		}
		if options.mode == WriteMode::CreateNew {
			rename_file_no_replace(&source_parent, source_name, &parent, name)?;
		} else {
			inspect_destination(&parent, name)?;
			source_parent.rename(source_name, &parent, name)?;
		}
		if options.durability == Durability::FileAndDir {
			sync_dir(&parent).map_err(WriteError::PublishedButNotDurable)?;
			sync_dir(&source_parent).map_err(WriteError::PublishedButNotDurable)?;
		}
		Ok(WriteOutcome::Published)
	}

	pub(super) fn copy_open_file_atomically(
		&self,
		path: &RelativePath,
		source: &mut std::fs::File,
		max_size: u64,
		options: WriteOptions,
	) -> Result<WriteOutcome, WriteError> {
		options.permissions.check_supported()?;
		options.durability.check_supported()?;
		options.mode.check_supported()?;
		let source_size = source.metadata()?.len();
		if source_size > max_size {
			return Err(copy_limit_error().into());
		}
		let (parent, name) = self.open_parent_dir(path)?;
		if let Some(metadata) = inspect_write_destination(&parent, name, options.mode)?
			&& options.mode == WriteMode::IfChanged
			&& metadata.len() == source_size
			&& permissions_match(&metadata, options.permissions)
		{
			let mut existing = open_regular_file(&parent, name, file_sync(options.durability))?;
			if stream_contents_match(&mut existing, source, source_size, None)?
				&& path_matches_open_file(&parent, name, &existing)?
			{
				if options.durability != Durability::None {
					existing.sync_all()?;
				}
				if options.durability == Durability::FileAndDir {
					sync_dir(&parent)?;
				}
				return Ok(WriteOutcome::Unchanged);
			}
			source.rewind()?;
		}
		publish_file(&parent, name, options, |file| {
			copy_bounded(source, file, max_size, None)
		})
	}
}

/// Returns whether this module synchronizes a handle for `durability`.
fn file_sync(durability: Durability) -> FileSync {
	match durability {
		Durability::None => FileSync::NotRequired,
		Durability::File | Durability::FileAndDir => FileSync::Required,
	}
}

fn publish_file(
	parent: &CapDir,
	name: &OsStr,
	options: WriteOptions,
	write_fn: impl FnOnce(&mut File) -> io::Result<()>,
) -> Result<WriteOutcome, WriteError> {
	let mut temporary = TemporaryFile::create(parent, options.permissions)?;
	write_fn(&mut temporary.file)?;
	publish_temporary_file(temporary, name, options, None)
}

fn publish_temporary_file(
	mut temporary: TemporaryFile<'_>,
	name: &OsStr,
	options: WriteOptions,
	operation: Option<&IoOperation<'_>>,
) -> Result<WriteOutcome, WriteError> {
	check_cancellation(operation)?;
	let parent = temporary.parent;
	if let Some(permissions) = temporary.platform_default_permissions.take() {
		temporary.file.set_permissions(permissions)?;
	}
	apply_permissions(&temporary.file.try_clone()?.into_std(), options.permissions)?;
	check_cancellation(operation)?;
	if options.durability != Durability::None {
		temporary.file.sync_all()?;
	}
	// Recheck names after writing so an observed link or special file cannot
	// be treated as an ordinary replacement destination.
	if options.mode != WriteMode::CreateNew {
		inspect_destination(parent, name)?;
	}
	if !path_matches_open_file(parent, temporary.staged_name(), &temporary.file)? {
		return Err(super::invalid_entry().into());
	}
	if let Some(operation) = operation {
		operation.begin_commit()?;
	}
	if options.mode == WriteMode::CreateNew {
		rename_file_no_replace(parent, temporary.staged_name(), parent, name)?;
	} else {
		parent.rename(temporary.staged_name(), parent, name)?;
	}
	temporary.publication = TemporaryPublication::Published;
	if options.durability == Durability::FileAndDir {
		sync_dir(parent).map_err(WriteError::PublishedButNotDurable)?;
	}
	Ok(WriteOutcome::Published)
}

fn inspect_write_destination(
	parent: &CapDir,
	name: &OsStr,
	mode: WriteMode,
) -> io::Result<Option<cap_std::fs::Metadata>> {
	if mode != WriteMode::CreateNew {
		return inspect_destination(parent, name);
	}
	match parent.symlink_metadata(name) {
		Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
		Err(error) => Err(error),
		Ok(_) => Err(io::Error::new(
			io::ErrorKind::AlreadyExists,
			"atomic create-only destination already exists",
		)),
	}
}

#[cfg(any(
	target_os = "linux",
	target_os = "android",
	target_vendor = "apple",
	target_os = "redox"
))]
fn rename_file_no_replace(
	source_parent: &CapDir,
	source: &OsStr,
	destination_parent: &CapDir,
	destination: &OsStr,
) -> io::Result<()> {
	use rustix::fs::{RenameFlags, renameat_with};
	use rustix::io::Errno;

	renameat_with(
		source_parent,
		source,
		destination_parent,
		destination,
		RenameFlags::NOREPLACE,
	)
	.map_err(|error| {
		if matches!(error, Errno::NOSYS | Errno::INVAL | Errno::NOTSUP) {
			io::Error::new(io::ErrorKind::Unsupported, error)
		} else {
			error.into()
		}
	})
}

#[cfg(not(any(
	target_os = "linux",
	target_os = "android",
	target_vendor = "apple",
	target_os = "redox"
)))]
fn rename_file_no_replace(_: &CapDir, _: &OsStr, _: &CapDir, _: &OsStr) -> io::Result<()> {
	Err(io::Error::new(
		io::ErrorKind::Unsupported,
		"atomic create-only publication is unavailable on this platform",
	))
}

fn inspect_destination(parent: &CapDir, name: &OsStr) -> io::Result<Option<cap_std::fs::Metadata>> {
	match parent.symlink_metadata(name) {
		Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
		Err(error) => Err(error),
		Ok(metadata) if metadata.is_file() && metadata.nlink() == 1 => Ok(Some(metadata)),
		// A concurrent replacement can unlink the inode while macOS obtains its
		// metadata. That inode no longer names a destination to inspect.
		Ok(metadata) if metadata.is_file() && metadata.nlink() == 0 => Ok(None),
		Ok(_) => Err(super::invalid_entry()),
	}
}

fn path_matches_open_file(parent: &CapDir, name: &OsStr, file: &File) -> io::Result<bool> {
	let Some(current) = inspect_destination(parent, name)? else {
		return Ok(false);
	};
	let opened = file.metadata()?;
	Ok(current.dev() == opened.dev() && current.ino() == opened.ino() && opened.nlink() == 1)
}

fn permissions_match(metadata: &cap_std::fs::Metadata, permissions: Permissions) -> bool {
	#[cfg(unix)]
	if let Permissions::Unix(mode) = permissions {
		use cap_std::fs::PermissionsExt;
		return metadata.permissions().mode() & 0o7777 == mode.bits();
	}
	let _ = (metadata, permissions);
	true
}

fn file_contents_match(file: &mut File, expected: &[u8]) -> io::Result<bool> {
	if file.metadata()?.len() != expected.len() as u64 {
		return Ok(false);
	}
	let mut buffer = zeroize::Zeroizing::new([0u8; 8192]);
	let mut offset = 0;
	while offset < expected.len() {
		let count = buffer.len().min(expected.len() - offset);
		match file.read(&mut buffer[..count]) {
			Ok(0) => return Ok(false),
			Ok(read) => {
				if buffer[..read] != expected[offset..offset + read] {
					return Ok(false);
				}
				offset += read;
			}
			Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
			Err(error) => return Err(error),
		}
	}
	loop {
		match file.read(&mut buffer[..1]) {
			Ok(read) => return Ok(read == 0),
			Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
			Err(error) => return Err(error),
		}
	}
}

struct TemporaryFile<'a> {
	parent: &'a CapDir,
	file: File,
	publication: TemporaryPublication,
	platform_default_permissions: Option<cap_std::fs::Permissions>,
}

enum TemporaryPublication {
	Staged { name: OsString },
	Published,
}

impl<'a> TemporaryFile<'a> {
	fn staged_name(&self) -> &OsStr {
		match &self.publication {
			TemporaryPublication::Staged { name } => name,
			TemporaryPublication::Published => {
				unreachable!("only a staged file has a temporary name")
			}
		}
	}

	fn create(parent: &'a CapDir, permissions: Permissions) -> io::Result<Self> {
		#[cfg(not(unix))]
		let _ = permissions;
		let mut options = OpenOptions::new();
		options.create_new(true).read(true).write(true);
		#[cfg(not(unix))]
		let platform_default_permissions = None;
		#[cfg(unix)]
		let platform_default_permissions = {
			use cap_std::fs::OpenOptionsExt;
			// An empty probe records the effective creation mode, including the umask,
			// without changing process-wide state. It never receives input contents:
			// chmod would not revoke a descriptor opened before a mode restriction.
			let inherited = if matches!(permissions, Permissions::PlatformDefault) {
				let probe = Self::create_with_options(parent, &options)?;
				Some(probe.file.metadata()?.permissions())
			} else {
				None
			};
			options.mode(0o600);
			inherited
		};
		let mut temporary = Self::create_with_options(parent, &options)?;
		temporary.platform_default_permissions = platform_default_permissions;
		Ok(temporary)
	}

	fn create_with_options(parent: &'a CapDir, options: &OpenOptions) -> io::Result<Self> {
		for _ in 0..32 {
			let mut random = [0u8; 16];
			crate::random::secure_bytes::fill_random_bytes(&mut random)
				.map_err(io::Error::other)?;
			let name = OsString::from(format!(
				".primitives-{:032x}.tmp",
				u128::from_le_bytes(random)
			));
			match parent.open_with(&name, options) {
				Ok(file) => {
					return Ok(Self {
						parent,
						file,
						publication: TemporaryPublication::Staged { name },
						platform_default_permissions: None,
					});
				}
				Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
				Err(error) => return Err(error),
			}
		}
		Err(io::Error::new(
			io::ErrorKind::AlreadyExists,
			"temporary-file names collided on every attempt",
		))
	}
}

impl Drop for TemporaryFile<'_> {
	fn drop(&mut self) {
		if let TemporaryPublication::Staged { name } = &self.publication {
			let _ = self.parent.remove_file(name);
		}
	}
}

fn copy_limit_error() -> io::Error {
	io::Error::new(
		io::ErrorKind::InvalidData,
		"file copy exceeds its byte limit",
	)
}

fn read_retrying_interrupted(
	reader: &mut (impl Read + ?Sized),
	buffer: &mut [u8],
	operation: Option<&IoOperation<'_>>,
) -> io::Result<usize> {
	loop {
		check_cancellation(operation)?;
		match reader.read(buffer) {
			Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
			result => return result,
		}
	}
}

fn copy_bounded(
	source: &mut (impl Read + ?Sized),
	destination: &mut impl Write,
	max_size: u64,
	mut operation: Option<&mut IoOperation<'_>>,
) -> io::Result<()> {
	let mut buffer = zeroize::Zeroizing::new([0u8; 8192]);
	let mut remaining = max_size;
	while remaining != 0 {
		let limit = remaining.min(buffer.len() as u64) as usize;
		let count = read_source(source, &mut buffer[..limit], operation.as_deref_mut())?;
		if count == 0 {
			return Ok(());
		}
		write_chunk(destination, &buffer[..count], operation.as_deref())?;
		remaining -= count as u64;
	}
	if read_source(source, &mut buffer[..1], operation)? != 0 {
		return Err(copy_limit_error());
	}
	Ok(())
}

fn stream_contents_match(
	first: &mut impl Read,
	second: &mut impl Read,
	size: u64,
	operation: Option<&IoOperation<'_>>,
) -> io::Result<bool> {
	let mut left = zeroize::Zeroizing::new([0u8; 8192]);
	let mut right = zeroize::Zeroizing::new([0u8; 8192]);
	let mut remaining = size;
	while remaining != 0 {
		let count = remaining.min(left.len() as u64) as usize;
		match read_exact_checked(first, &mut left[..count], operation)
			.and_then(|()| read_exact_checked(second, &mut right[..count], operation))
		{
			Ok(()) => {}
			Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(false),
			Err(error) => return Err(error),
		}
		if left[..count] != right[..count] {
			return Ok(false);
		}
		remaining -= count as u64;
	}
	Ok(
		read_retrying_interrupted(first, &mut left[..1], operation)? == 0
			&& read_retrying_interrupted(second, &mut right[..1], operation)? == 0,
	)
}

fn check_cancellation(operation: Option<&IoOperation<'_>>) -> io::Result<()> {
	if let Some(operation) = operation {
		operation.check_cancellation().map_err(io::Error::other)?;
	}
	Ok(())
}

fn read_source(
	reader: &mut (impl Read + ?Sized),
	buffer: &mut [u8],
	mut operation: Option<&mut IoOperation<'_>>,
) -> io::Result<usize> {
	loop {
		let result = match operation.as_deref_mut() {
			Some(operation) => operation.read(reader, buffer),
			None => reader.read(buffer),
		};
		match result {
			Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
			result => return result,
		}
	}
}

fn write_chunk(
	writer: &mut impl Write,
	mut bytes: &[u8],
	operation: Option<&IoOperation<'_>>,
) -> io::Result<()> {
	while !bytes.is_empty() {
		check_cancellation(operation)?;
		match writer.write(bytes) {
			Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
			Ok(count) => bytes = &bytes[count..],
			Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
			Err(error) => return Err(error),
		}
	}
	Ok(())
}

fn read_exact_checked(
	reader: &mut impl Read,
	mut buffer: &mut [u8],
	operation: Option<&IoOperation<'_>>,
) -> io::Result<()> {
	while !buffer.is_empty() {
		let count = read_retrying_interrupted(reader, buffer, operation)?;
		if count == 0 {
			return Err(io::ErrorKind::UnexpectedEof.into());
		}
		buffer = &mut buffer[count..];
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::io::cooperative::{
		CancelOutcome, CancellationHandle, IoControlError, IoOperationOptions,
	};

	struct CancellingReader(CancellationHandle);
	impl Read for CancellingReader {
		fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
			assert_eq!(self.0.cancel(), CancelOutcome::Accepted);
			Err(io::ErrorKind::Interrupted.into())
		}
	}

	#[test]
	fn comparison_retries_check_cancellation_without_changing_source_progress() {
		let operation = IoOperation::new(IoOperationOptions {
			on_progress_fn: None,
		});
		let mut source = CancellingReader(operation.cancellation_handle());
		let mut existing = b"same".as_slice();
		let error =
			stream_contents_match(&mut source, &mut existing, 4, Some(&operation)).unwrap_err();
		assert!(matches!(
			WriteError::from(error),
			WriteError::IoControl(IoControlError::Cancelled)
		));
		assert_eq!(existing, b"same");
		assert_eq!(operation.bytes_read(), 0);
	}

	struct CancellingWriter {
		handle: CancellationHandle,
		writes: usize,
	}
	impl Write for CancellingWriter {
		fn write(&mut self, _: &[u8]) -> io::Result<usize> {
			self.writes += 1;
			assert_eq!(self.handle.cancel(), CancelOutcome::Accepted);
			Ok(1)
		}
		fn flush(&mut self) -> io::Result<()> {
			Ok(())
		}
	}

	#[test]
	fn partial_writes_check_cancellation_before_writing_more_bytes() {
		let operation = IoOperation::new(IoOperationOptions {
			on_progress_fn: None,
		});
		let mut writer = CancellingWriter {
			handle: operation.cancellation_handle(),
			writes: 0,
		};
		let error = write_chunk(&mut writer, b"input", Some(&operation)).unwrap_err();
		assert!(matches!(
			WriteError::from(error),
			WriteError::IoControl(IoControlError::Cancelled)
		));
		assert_eq!(writer.writes, 1);
	}
}
