use std::fs::File;
use std::io;
use std::path::Path;

use cap_fs_ext::{
	DirExt, FollowSymlinks, OpenOptionsFollowExt, OpenOptionsMaybeDirExt, OpenOptionsSyncExt,
};
use cap_std::fs::{Dir as CapDir, OpenOptions};

use super::{CreateDirOptions, Durability, Permissions, RelativePath};
use crate::io::bounded_reads::{ReadError, ReadOptions, read_to_end};

/// An open dir for operations that refuse symlink traversal beneath it.
#[derive(Debug)]
pub struct Dir {
	pub(crate) inner: CapDir,
}

enum ChildDirPreparation {
	NeedsSync(CapDir),
	Synchronized(CapDir),
}

impl Dir {
	/// Opens a trusted path using the process's filesystem authority.
	/// Relative paths use the process's current dir. This initial lookup may
	/// follow links; subsequent operations are relative to the resulting handle.
	pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
		Ok(Self {
			inner: CapDir::open_ambient_dir(path, cap_std::ambient_authority())?,
		})
	}

	/// Duplicates this dir handle without resolving its path again.
	pub fn try_clone(&self) -> io::Result<Self> {
		Ok(Self {
			inner: self.inner.try_clone()?,
		})
	}

	/// Synchronizes this dir's entries and metadata through its open handle.
	/// This does not synchronize file contents, child directories, or ancestors.
	/// Synchronize relevant file handles before this dir when establishing durable
	/// file publication. Errors from the filesystem are preserved; platforms without
	/// dir synchronization report `Unsupported`.
	pub fn sync_all(&self) -> io::Result<()> {
		sync_dir(&self.inner)
	}

	/// Opens an existing subdirectory without following any symbolic links.
	pub fn open_dir(&self, path: &RelativePath) -> io::Result<Self> {
		let mut dir = self.inner.try_clone()?;
		for name in path.as_path().iter() {
			dir = dir.open_dir_nofollow(name)?;
		}
		Ok(Self { inner: dir })
	}

	/// Creates missing directories and returns a handle to the final dir.
	/// Existing directories retain their permissions. Each component is opened
	/// without following links, including when another creator wins a mkdir race.
	/// A failure can leave directories that this operation already created.
	pub fn create_dir_all(
		&self,
		path: &RelativePath,
		options: CreateDirOptions,
	) -> io::Result<Self> {
		options.permissions.check_supported()?;
		if options.synchronize {
			Durability::FileAndDir.check_supported()?;
		}
		let mut dir = self.inner.try_clone()?;
		for name in path.as_path().iter() {
			let child = match dir.open_dir_nofollow(name) {
				Ok(child) => ChildDirPreparation::NeedsSync(child),
				Err(error) if error.kind() == io::ErrorKind::NotFound => {
					let builder = cap_std::fs::DirBuilder::new();
					#[cfg(unix)]
					let mut builder = builder;
					#[cfg(unix)]
					if let Permissions::Unix(mode) = options.permissions {
						use cap_std::fs::DirBuilderExt;
						builder.mode(mode.bits() | 0o700);
					}
					let created = match dir.create_dir_with(name, &builder) {
						Ok(()) => true,
						Err(error) if error.kind() == io::ErrorKind::AlreadyExists => false,
						Err(error) => return Err(error),
					};
					let child = dir.open_dir_nofollow(name)?;
					if created {
						let handle = open_dir_for_io(&child)?;
						apply_permissions(&handle, options.permissions)?;
						if options.synchronize {
							handle.sync_all()?;
							ChildDirPreparation::Synchronized(child)
						} else {
							ChildDirPreparation::NeedsSync(child)
						}
					} else {
						ChildDirPreparation::NeedsSync(child)
					}
				}
				Err(error) => return Err(error),
			};
			let child = match child {
				ChildDirPreparation::NeedsSync(child) => {
					if options.synchronize {
						sync_dir(&child)?;
					}
					child
				}
				ChildDirPreparation::Synchronized(child) => child,
			};
			if options.synchronize {
				sync_dir(&dir)?;
			}
			dir = child;
		}
		Ok(Self { inner: dir })
	}

	/// Opens a regular file for reading without following links. Hard links are
	/// permitted for ordinary reads because this operation does not modify contents.
	pub fn open_file(&self, path: &RelativePath) -> io::Result<File> {
		let (parent, name) = self.open_parent_dir(path)?;
		Ok(open_regular_file(&parent, name, FileSync::NotRequired)?.into_std())
	}

	/// Reads a regular file with an explicit byte limit. A read failure returns no
	/// partial result; use [`Self::open_file`] and [`crate::io::bounded_reads::read_to_end`] when
	/// the application needs access to partial data.
	pub fn read_file(
		&self,
		path: &RelativePath,
		options: ReadOptions,
	) -> Result<Vec<u8>, ReadError> {
		let mut file = self.open_file(path).map_err(ReadError::Io)?;
		let mut bytes = Vec::new();
		read_to_end(&mut file, &mut bytes, options)?;
		Ok(bytes)
	}

	/// Removes a regular file's dir entry. A missing path remains an error.
	/// The operation refuses an observed symlink and never follows a replacement link.
	/// Concurrent writers require application coordination.
	pub fn remove_file(&self, path: &RelativePath) -> io::Result<()> {
		let (parent, name) = self.open_parent_dir(path)?;
		if !parent.symlink_metadata(name)?.is_file() {
			return Err(super::invalid_entry());
		}
		parent.remove_file(name)
	}

	/// Removes an empty dir without following links in its ancestors.
	pub fn remove_dir(&self, path: &RelativePath) -> io::Result<()> {
		let (parent, name) = self.open_parent_dir(path)?;
		parent.remove_dir(name)
	}

	pub(crate) fn open_parent_dir<'a>(
		&self,
		path: &'a RelativePath,
	) -> io::Result<(CapDir, &'a std::ffi::OsStr)> {
		let mut parts = path.as_path().iter();
		let name = parts.next_back().expect("RelativePath is nonempty");
		let mut dir = self.inner.try_clone()?;
		for part in parts {
			dir = dir.open_dir_nofollow(part)?;
		}
		Ok((dir, name))
	}
}

/// Whether code synchronizes a regular-file handle with `File::sync_all`.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum FileSync {
	NotRequired,
	Required,
}

pub(super) fn open_regular_file(
	dir: &CapDir,
	name: &std::ffi::OsStr,
	sync: FileSync,
) -> io::Result<cap_std::fs::File> {
	if !dir.symlink_metadata(name)?.is_file() {
		return Err(super::invalid_entry());
	}
	let mut options = OpenOptions::new();
	options.read(true).follow(FollowSymlinks::No).nonblock(true);
	// On Windows, `File::sync_all` calls `FlushFileBuffers`, which requires write access.
	#[cfg(windows)]
	if sync == FileSync::Required {
		options.write(true);
	}
	#[cfg(not(windows))]
	let _ = sync;
	let file = dir.open_with(name, &options)?;
	if !file.metadata()?.is_file() {
		return Err(super::invalid_entry());
	}
	Ok(file)
}

pub(super) fn sync_dir(dir: &CapDir) -> io::Result<()> {
	#[cfg(unix)]
	{
		open_dir_for_io(dir)?.sync_all()
	}
	#[cfg(not(unix))]
	{
		let _ = dir;
		Err(io::Error::new(
			io::ErrorKind::Unsupported,
			"dir synchronization is unavailable on this platform",
		))
	}
}

pub(crate) fn apply_permissions(file: &File, permissions: Permissions) -> io::Result<()> {
	permissions.check_supported()?;
	#[cfg(unix)]
	if let Permissions::Unix(mode) = permissions {
		use std::os::unix::fs::PermissionsExt;
		file.set_permissions(std::fs::Permissions::from_mode(mode.bits()))?;
	}
	#[cfg(not(unix))]
	let _ = file;
	Ok(())
}

pub(super) fn open_dir_for_io(dir: &CapDir) -> io::Result<File> {
	let mut options = OpenOptions::new();
	options.read(true).maybe_dir(true);
	Ok(dir.open_with(".", &options)?.into_std())
}
