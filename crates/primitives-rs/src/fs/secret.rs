use std::io;
#[cfg(unix)]
use std::io::Read;

use super::{Dir, RelativePath};
use crate::{
	io::bounded_reads::{ReadError, ReadOptions},
	secret_bytes::SecretBytes,
};

/// Synchronization after changing access permissions through an open handle.
#[derive(Clone, Copy, Debug)]
pub struct PermissionChangeOptions {
	/// Synchronizes the handle even if its permissions already match.
	pub synchronize: bool,
}

impl Dir {
	/// Requires effective-user ownership and applies dir mode `0700`.
	///
	/// The operation uses this dir's open handle, including after a rename.
	/// Other platforms return `Unsupported` without changing permissions.
	pub fn restrict_permissions_to_owner(
		&self,
		options: PermissionChangeOptions,
	) -> io::Result<()> {
		#[cfg(unix)]
		{
			let file = super::dir::open_dir_for_io(&self.inner)?;
			restrict_permissions(&file, 0o700, options)
		}
		#[cfg(not(unix))]
		{
			let _ = options;
			Err(unsupported_private_permissions())
		}
	}

	/// Requires effective-user ownership and applies regular-file mode `0600`.
	///
	/// Symbolic links, special files, and multiple hard links are rejected before
	/// changing permissions. Atomic replacement can leave this operation holding
	/// an older unlinked file; callers coordinate writes when path identity matters.
	pub fn restrict_file_permissions_to_owner(
		&self,
		path: &RelativePath,
		options: PermissionChangeOptions,
	) -> io::Result<()> {
		#[cfg(unix)]
		{
			use std::os::unix::fs::MetadataExt;
			let file = self.open_file(path)?;
			if file.metadata()?.nlink() > 1 {
				return Err(super::invalid_entry());
			}
			restrict_permissions(&file, 0o600, options)
		}
		#[cfg(not(unix))]
		{
			let _ = (path, options);
			Err(unsupported_private_permissions())
		}
	}

	/// Reads a regular secret file into a zeroizing allocation.
	///
	/// Unix files must belong to the effective user, have no additional hard links,
	/// and have no group, other, or special permission bits. The size must fit the
	/// explicit limit. A length change during the read fails without returning any
	/// bytes. Applications must exclude concurrent in-place writes to obtain a
	/// consistent snapshot. Atomic replacements preserve the opened version.
	///
	/// Other platforms return `Unsupported` before opening the file because this
	/// API does not implement an equivalent owner-only ACL check.
	pub fn read_secret_file(
		&self,
		path: &RelativePath,
		options: ReadOptions,
	) -> Result<SecretBytes, ReadError> {
		#[cfg(unix)]
		{
			use std::os::unix::fs::MetadataExt;
			let mut file = self.open_file(path).map_err(ReadError::Io)?;
			let metadata = file.metadata().map_err(ReadError::Io)?;
			if metadata.uid() != rustix::process::geteuid().as_raw()
				|| metadata.mode() & 0o7077 != 0
				|| metadata.nlink() > 1
			{
				return Err(ReadError::Io(io::Error::new(
					io::ErrorKind::PermissionDenied,
					"secret file must belong to the effective user, have no additional hard links, and have only owner permissions",
				)));
			}
			let size = usize::try_from(metadata.len()).map_err(|_| ReadError::LimitExceeded {
				max: options.max_size,
			})?;
			if size > options.max_size {
				return Err(ReadError::LimitExceeded {
					max: options.max_size,
				});
			}
			let mut bytes = SecretBytes::zeroed(size).map_err(|_| ReadError::AllocationFailed)?;
			file.read_exact(bytes.expose_secret_mut())
				.map_err(ReadError::Io)?;
			let mut extra = zeroize::Zeroizing::new([0u8; 1]);
			loop {
				match file.read(extra.as_mut()) {
					Ok(0) => return Ok(bytes),
					Ok(_) => {
						return Err(ReadError::Io(io::Error::new(
							io::ErrorKind::InvalidData,
							"secret file length changed during reading",
						)));
					}
					Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
					Err(error) => return Err(ReadError::Io(error)),
				}
			}
		}
		#[cfg(not(unix))]
		{
			let _ = (path, options);
			Err(ReadError::Io(io::Error::new(
				io::ErrorKind::Unsupported,
				"owner-private file checks are unavailable on this platform",
			)))
		}
	}
}

#[cfg(unix)]
fn restrict_permissions(
	file: &std::fs::File,
	mode: u32,
	options: PermissionChangeOptions,
) -> io::Result<()> {
	use std::os::unix::fs::{MetadataExt, PermissionsExt};
	let metadata = file.metadata()?;
	if metadata.uid() != rustix::process::geteuid().as_raw() {
		return Err(io::Error::new(
			io::ErrorKind::PermissionDenied,
			"entry must belong to the effective user",
		));
	}
	if metadata.mode() & 0o7777 != mode {
		file.set_permissions(std::fs::Permissions::from_mode(mode))?;
	}
	if options.synchronize {
		file.sync_all()?;
	}
	Ok(())
}

#[cfg(not(unix))]
fn unsupported_private_permissions() -> io::Error {
	io::Error::new(
		io::ErrorKind::Unsupported,
		"owner-private permissions are unavailable on this platform",
	)
}
