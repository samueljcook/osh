//! Dir-relative file operations and atomic publication.
//!
//! Enable `fs` on Unix or Windows. [`Dir::open`](crate::fs::Dir::open) selects a trusted dir
//! through an ordinary path, which may contain links. Every subsequent relative
//! operation refuses symbolic links in dir components and file opens. Open
//! handles retain their dir identity if an ancestor is renamed. These
//! operations do not lock out other writers: applications must coordinate updates
//! that require read/modify/write exclusion and prevent hostile modification of
//! directories that hold secrets. Atomic replacement protects readers from partial
//! files; it does not make an entire dir tree transactional.
//!
//! Filesystem calls are synchronous. Async applications can run a complete operation
//! on their runtime's blocking executor. Dropping that executor's future does not
//! cancel an operation that has already started.
//!
//! ```no_run
//! use primitives_rs::fs::{Dir, RelativePath, Permissions, WriteOptions,
//!     WriteMode, Durability};
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let dir = Dir::open("./generated")?;
//! dir.write_file_atomically(&RelativePath::new("types.ts")?, b"export type Id = string;\n",
//!     WriteOptions { permissions: Permissions::PlatformDefault,
//!         mode: WriteMode::IfChanged, durability: Durability::None })?;
//! # Ok(())
//! # }
//! ```

mod copy;
mod dir;
#[cfg(feature = "fs-json")]
mod json;
mod options;
mod path;
mod publication;
#[cfg(feature = "fs-secret")]
mod secret;
#[cfg(feature = "fs-secret")]
pub use secret::PermissionChangeOptions;

pub use copy::{CopyDirError, CopyDirOptions, CopyDirOutcome, CopyFileOptions, FileCopySource};
pub use dir::Dir;
#[cfg(feature = "process-lock")]
pub(crate) use dir::apply_permissions;
#[cfg(feature = "fs-json")]
pub use json::{JsonFileError, WriteJsonOptions};
pub use options::{
	CreateDirOptions, Durability, InvalidUnixMode, Permissions, UnixMode, WriteError,
	WriteFromReaderOptions, WriteMode, WriteOptions, WriteOutcome,
};
pub use path::{InvalidRelativePath, RelativePath};

pub(crate) fn invalid_entry() -> std::io::Error {
	std::io::Error::new(
		std::io::ErrorKind::InvalidInput,
		"expected a regular file without symbolic or multiple hard links",
	)
}

/// The existing file that an atomic move will publish at its destination.
#[derive(Clone, Copy, Debug)]
pub struct FileMoveSource<'a> {
	/// Open source dir.
	pub dir: &'a Dir,
	/// Source file path relative to that dir.
	pub path: &'a RelativePath,
}
