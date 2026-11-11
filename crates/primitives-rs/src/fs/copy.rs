use std::io;

use cap_fs_ext::MetadataExt;

use super::{CreateDirOptions, Dir, RelativePath, WriteError, WriteOptions, WriteOutcome};

/// Source dir and relative file for a copy operation.
pub struct FileCopySource<'a> {
	/// Dir from which to read.
	pub dir: &'a Dir,
	/// File beneath that dir.
	pub path: &'a RelativePath,
}

/// Bounds and destination policy for copying one regular file.
#[derive(Clone, Copy, Debug)]
pub struct CopyFileOptions {
	/// Maximum source size in bytes. The copy uses fixed-size buffers.
	pub max_size: u64,
	/// Atomic publication policy. Source permissions are not copied implicitly.
	pub write: WriteOptions,
}

/// Bounds and destination policy for copying a dir's contents.
#[derive(Clone, Copy, Debug)]
pub struct CopyDirOptions {
	/// Maximum number of files and directories in the source tree, excluding its root.
	pub max_entries: usize,
	/// Maximum number of path components below the source root.
	pub max_depth: usize,
	/// Policy for each source file.
	pub file: CopyFileOptions,
	/// Policy for creating destination subdirectories.
	pub dir: CreateDirOptions,
}

/// Counts from a completed dir copy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CopyDirOutcome {
	/// Number of complete files published at their destination paths.
	pub files_published: usize,
	/// Number of files that already had identical bytes and requested permissions.
	pub files_unchanged: usize,
}

/// A dir copy failed. Files copied before the failure remain published.
#[derive(Debug)]
pub enum CopyDirError {
	/// Source traversal, destination validation, or dir creation failed.
	Io(io::Error),
	/// One file could not be copied. The source error identifies whether that file
	/// was published before synchronization failed.
	File {
		/// Relative file path within the source and destination trees.
		path: RelativePath,
		/// File publication error.
		source: WriteError,
	},
}

impl std::fmt::Display for CopyDirError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::Io(error) => write!(f, "dir copy failed: {error}"),
			Self::File { path, source } => {
				write!(f, "dir copy failed at {:?}: {source}", path.as_path())
			}
		}
	}
}

impl std::error::Error for CopyDirError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		Some(match self {
			Self::Io(error) => error,
			Self::File { source, .. } => source,
		})
	}
}

impl From<io::Error> for CopyDirError {
	fn from(error: io::Error) -> Self {
		Self::Io(error)
	}
}

impl Dir {
	/// Copies one regular file through bounded streaming and atomic publication.
	/// The operation rejects source/destination aliases before any write.
	/// Source writers must use atomic replacement or be excluded for a consistent copy.
	pub fn copy_file(
		&self,
		destination: &RelativePath,
		source: FileCopySource<'_>,
		options: CopyFileOptions,
	) -> Result<WriteOutcome, WriteError> {
		let mut source_file = source.dir.open_file(source.path)?;
		let source_metadata = cap_std::fs::Metadata::from_file(&source_file)?;
		let (parent, name) = self.open_parent_dir(destination)?;
		match parent.symlink_metadata(name) {
			Ok(destination_metadata) => {
				if source_metadata.dev() == destination_metadata.dev()
					&& source_metadata.ino() == destination_metadata.ino()
				{
					return Err(io::Error::new(
						io::ErrorKind::InvalidInput,
						"copy source and destination refer to the same file",
					)
					.into());
				}
			}
			Err(error) if error.kind() == io::ErrorKind::NotFound => {}
			Err(error) => return Err(error.into()),
		}
		self.copy_open_file_atomically(
			destination,
			&mut source_file,
			options.max_size,
			options.write,
		)
	}

	/// Copies regular files and directories from `source` into this dir.
	///
	/// A bounded source inventory is collected before destination mutations. Links,
	/// special files, identical roots, and a destination inside the source tree are
	/// rejected. Each file is published atomically, but a later failure can leave
	/// completed copies. Existing destination files that have no source counterpart
	/// remain untouched. Source permissions are not copied implicitly.
	/// Applications must exclude concurrent changes to source dir structure.
	pub fn copy_dir(
		&self,
		source: &Dir,
		options: CopyDirOptions,
	) -> Result<CopyDirOutcome, CopyDirError> {
		options.file.write.permissions.check_supported()?;
		options.file.write.durability.check_supported()?;
		options.file.write.mode.check_supported()?;
		options.dir.permissions.check_supported()?;
		if options.dir.synchronize {
			super::Durability::FileAndDir.check_supported()?;
		}
		let destination_id = self.inner.dir_metadata()?;
		let source_id = source.inner.dir_metadata()?;
		let mut inventory = Vec::new();
		let mut pending = vec![(std::path::PathBuf::new(), 0usize)];
		while let Some((prefix, depth)) = pending.pop() {
			let dir = if prefix.as_os_str().is_empty() {
				source.inner.try_clone()?
			} else {
				source
					.open_dir(&RelativePath::new(&prefix).map_err(io::Error::other)?)?
					.inner
			};
			let metadata = dir.dir_metadata()?;
			if metadata.dev() == destination_id.dev() && metadata.ino() == destination_id.ino() {
				return Err(io::Error::new(
					io::ErrorKind::InvalidInput,
					"copy destination must not be the source or a source descendant",
				)
				.into());
			}
			for entry in dir.entries()? {
				let entry = entry?;
				if inventory.len() == options.max_entries || depth >= options.max_depth {
					return Err(io::Error::new(
						io::ErrorKind::InvalidData,
						"dir copy exceeds its entry or depth limit",
					)
					.into());
				}
				let path =
					RelativePath::new(prefix.join(entry.file_name())).map_err(io::Error::other)?;
				let metadata = dir.symlink_metadata(entry.file_name())?;
				if metadata.is_dir() {
					pending.push((path.as_path().to_owned(), depth + 1));
				} else if !metadata.is_file() {
					return Err(super::invalid_entry().into());
				}
				inventory.push((path, metadata.is_dir()));
			}
		}
		// Reject overlap that would overwrite source entries during the copy.
		for (path, is_dir) in &inventory {
			if *is_dir {
				match self.open_dir(path) {
					Ok(dir) => {
						let metadata = dir.inner.dir_metadata()?;
						if metadata.dev() == source_id.dev() && metadata.ino() == source_id.ino() {
							return Err(io::Error::new(
								io::ErrorKind::InvalidInput,
								"copy destination overlaps the source tree",
							)
							.into());
						}
					}
					Err(error) if error.kind() == io::ErrorKind::NotFound => {}
					Err(error) => return Err(error.into()),
				}
			}
		}
		inventory.sort_by(|a, b| a.0.as_path().cmp(b.0.as_path()));
		let mut result = CopyDirOutcome {
			files_published: 0,
			files_unchanged: 0,
		};
		for (path, is_dir) in inventory {
			if is_dir {
				self.create_dir_all(&path, options.dir)?;
			} else {
				match self
					.copy_file(
						&path,
						FileCopySource {
							dir: source,
							path: &path,
						},
						options.file,
					)
					.map_err(|source| CopyDirError::File {
						path: path.clone(),
						source,
					})? {
					WriteOutcome::Published => result.files_published += 1,
					WriteOutcome::Unchanged => result.files_unchanged += 1,
				}
			}
		}
		Ok(result)
	}
}
