/// Unix permission bits without setuid, setgid, or sticky bits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnixMode(u32);

impl UnixMode {
	/// Owner read and write permissions, with no group or other permissions.
	pub const OWNER_READ_WRITE: Self = Self(0o600);
	/// Owner read, write, and search permissions, with no group or other permissions.
	pub const OWNER_ALL: Self = Self(0o700);

	/// Parses permission bits in the range `0o000..=0o777`.
	pub fn new(bits: u32) -> Result<Self, InvalidUnixMode> {
		if bits > 0o777 {
			return Err(InvalidUnixMode);
		}
		Ok(Self(bits))
	}

	/// Returns the permission bits.
	pub fn bits(self) -> u32 {
		self.0
	}
}

/// Unix permission bits contained bits other than owner, group, and other permissions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidUnixMode;

impl std::fmt::Display for InvalidUnixMode {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str("Unix mode must contain only owner, group, and other permission bits")
	}
}

impl std::error::Error for InvalidUnixMode {}

/// Permissions for a newly published file or a newly created dir.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Permissions {
	/// Uses the platform's ordinary creation permissions, including its umask or ACL.
	/// An unchanged file retains its existing permissions. Atomic moves preserve
	/// the source permissions when they publish a file.
	PlatformDefault,
	/// Applies these exact Unix permission bits. Other platforms return `Unsupported`.
	Unix(UnixMode),
}

impl Permissions {
	pub(crate) fn check_supported(self) -> std::io::Result<()> {
		#[cfg(not(unix))]
		if matches!(self, Self::Unix(_)) {
			return Err(std::io::Error::new(
				std::io::ErrorKind::Unsupported,
				"Unix permissions are unavailable on this platform",
			));
		}
		Ok(())
	}
}

/// Synchronization required before a filesystem operation returns successfully.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Durability {
	/// Performs no explicit storage synchronization.
	None,
	/// Synchronizes the file's contents and metadata.
	File,
	/// Synchronizes the file and its containing dir. Dir creation also
	/// synchronizes each new dir and its parent. This requires Unix support.
	FileAndDir,
}

impl Durability {
	pub(super) fn check_supported(self) -> std::io::Result<()> {
		#[cfg(not(unix))]
		if self == Self::FileAndDir {
			return Err(std::io::Error::new(
				std::io::ErrorKind::Unsupported,
				"dir synchronization is unavailable on this platform",
			));
		}
		Ok(())
	}
}

/// When an atomic write may publish a file at its destination.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WriteMode {
	/// Replaces the destination on every successful write.
	Always,
	/// Preserves an identical destination, including its inode and modification time.
	IfChanged,
	/// Publishes only if no entry exists at the destination. An existing entry
	/// returns `BeforePublication` with `AlreadyExists`, without reading or changing it.
	/// This requires native no-replace rename support on Linux, Android, Apple
	/// platforms, or Redox. An unsupported platform or filesystem returns `Unsupported`.
	CreateNew,
}

impl WriteMode {
	pub(super) fn check_supported(self) -> std::io::Result<()> {
		#[cfg(not(any(
			target_os = "linux",
			target_os = "android",
			target_vendor = "apple",
			target_os = "redox"
		)))]
		if self == Self::CreateNew {
			return Err(std::io::Error::new(
				std::io::ErrorKind::Unsupported,
				"atomic create-only publication is unavailable on this platform",
			));
		}
		Ok(())
	}
}

/// Explicit policy for atomic file publication. Parent directories must exist.
#[derive(Clone, Copy, Debug)]
pub struct WriteOptions {
	/// Permissions that the new file must have before publication.
	pub permissions: Permissions,
	/// Whether to replace existing files or require an absent destination.
	pub mode: WriteMode,
	/// Storage synchronization required for success, including unchanged files.
	pub durability: Durability,
}

/// Size bound and file policy for atomic publication from a reader.
pub struct WriteFromReaderOptions<'a> {
	/// Maximum input size in bytes. Zero permits only an empty input.
	/// The implementation uses fixed-size buffers regardless of this limit.
	pub max_size: u64,
	/// File publication, permissions, and durability policy.
	pub file: WriteOptions,
	/// Optional progress and cancellation for this complete write. The writer
	/// consumes the operation and resolves cancellation immediately before rename.
	/// `None` performs the write without cooperative cancellation or progress.
	pub operation: Option<crate::io::cooperative::IoOperation<'a>>,
}

/// Explicit policy for creating a dir chain.
#[derive(Clone, Copy, Debug)]
pub struct CreateDirOptions {
	/// Permissions for newly created directories. Existing permissions remain unchanged.
	pub permissions: Permissions,
	/// Whether to synchronize each dir and its parent before returning.
	/// This requires Unix support.
	pub synchronize: bool,
}

/// A successful atomic file write.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WriteOutcome {
	/// Contents and requested permissions matched, so the file was not replaced.
	Unchanged,
	/// A complete file was published at the destination.
	Published,
}

/// An atomic write failed before publication or after publication during synchronization.
#[derive(Debug)]
pub enum WriteError {
	/// Cooperative control stopped the operation before publication.
	IoControl(crate::io::cooperative::IoControlError),
	/// This operation published no file at the destination.
	BeforePublication(std::io::Error),
	/// The new file is visible, but synchronizing a containing dir failed.
	PublishedButNotDurable(std::io::Error),
}

impl std::fmt::Display for WriteError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::IoControl(error) => error.fmt(f),
			Self::BeforePublication(error) => {
				write!(f, "file write failed before publication: {error}")
			}
			Self::PublishedButNotDurable(error) => write!(
				f,
				"file publication succeeded but dir synchronization failed: {error}"
			),
		}
	}
}

impl std::error::Error for WriteError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		Some(match self {
			Self::IoControl(error) => error,
			Self::BeforePublication(error) | Self::PublishedButNotDurable(error) => error,
		})
	}
}

impl From<std::io::Error> for WriteError {
	fn from(error: std::io::Error) -> Self {
		if error
			.get_ref()
			.is_some_and(|source| source.is::<crate::io::cooperative::IoControlError>())
		{
			Self::IoControl(
				*error
					.into_inner()
					.expect("checked error payload")
					.downcast::<crate::io::cooperative::IoControlError>()
					.expect("checked control error type"),
			)
		} else {
			Self::BeforePublication(error)
		}
	}
}

impl From<crate::io::cooperative::IoControlError> for WriteError {
	fn from(error: crate::io::cooperative::IoControlError) -> Self {
		Self::IoControl(error)
	}
}
