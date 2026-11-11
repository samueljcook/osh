use super::{
	LocalEnvVault, VaultEditSession, VaultError, VaultLimits,
	record::{AuthenticatedVault, StoredVault, json_limits},
};
use primitives_rs::{
	fs::{
		CreateDirOptions, Dir, Durability, PermissionChangeOptions, Permissions, RelativePath,
		UnixMode, WriteError, WriteMode, WriteOptions,
	},
	io::bounded_reads::{ReadError, ReadOptions},
	processes::local_locks::{LockOptions, ProcessLock},
	serialization::json::{self, JsonFormat, SerializeOptions},
};
use std::io;

const VAULT_FILE: &str = "vault.json";
const LOCK_FILE: &str = "vault.lock";
const IGNORE_FILE: &str = ".gitignore";
const IGNORE_CONTENT: &[u8] = b"*\n";
const PERMISSION_OPTIONS: PermissionChangeOptions = PermissionChangeOptions { synchronize: true };

const fn write_options(mode: WriteMode) -> WriteOptions {
	WriteOptions {
		permissions: Permissions::Unix(UnixMode::OWNER_READ_WRITE),
		mode,
		durability: Durability::FileAndDir,
	}
}

impl LocalEnvVault {
	pub(super) fn open_directory(&self) -> Result<Dir, VaultError> {
		let directory = self
			.root_dir
			.open_dir(&self.relative_vault_path)
			.map_err(map_missing)?;
		directory.restrict_permissions_to_owner(PERMISSION_OPTIONS)?;
		Ok(directory)
	}
	pub(super) fn lock_directory(&self) -> Result<(Dir, ProcessLock), VaultError> {
		let directory = self.root_dir.create_dir_all(
			&self.relative_vault_path,
			CreateDirOptions {
				permissions: Permissions::Unix(UnixMode::OWNER_ALL),
				synchronize: true,
			},
		)?;
		directory.restrict_permissions_to_owner(PERMISSION_OPTIONS)?;
		match directory
			.restrict_file_permissions_to_owner(&relative_path(LOCK_FILE), PERMISSION_OPTIONS)
		{
			Ok(()) => {}
			Err(error) if error.kind() == io::ErrorKind::NotFound => {}
			Err(error) => return Err(error.into()),
		}
		let lock = ProcessLock::try_acquire(
			&directory,
			&relative_path(LOCK_FILE),
			LockOptions {
				permissions: Permissions::Unix(UnixMode::OWNER_READ_WRITE),
			},
		)?;
		ensure_ignore_file(&directory)?;
		Ok((directory, lock))
	}
	pub(super) fn read_record(&self, directory: &Dir) -> Result<StoredVault, VaultError> {
		let path = relative_path(VAULT_FILE);
		directory
			.restrict_file_permissions_to_owner(&path, PERMISSION_OPTIONS)
			.map_err(map_missing)?;
		let bytes = directory
			.read_secret_file(
				&path,
				ReadOptions {
					max_size: self.limits.max_file_size,
				},
			)
			.map_err(|error| match error {
				ReadError::Io(error) if error.kind() == io::ErrorKind::NotFound => {
					VaultError::MissingVault
				}
				other => VaultError::Read(other),
			})?;
		let record: StoredVault =
			json::deserialize_slice(bytes.expose_secret(), json_limits(self.limits))?;
		record.check_structure(&self.password_options, self.limits)?;
		Ok(record)
	}
	/// Writes the first record only if no vault file exists at publication.
	pub(super) fn publish_new_record(
		&self,
		directory: &Dir,
		record: &StoredVault,
	) -> Result<(), VaultError> {
		match write_record(directory, record, self.limits, WriteMode::CreateNew) {
			Err(VaultError::Write(WriteError::BeforePublication(error)))
				if error.kind() == io::ErrorKind::AlreadyExists =>
			{
				Err(VaultError::VaultAlreadyExists)
			}
			result => result,
		}
	}
}
impl VaultEditSession<'_> {
	pub(super) fn publish_record(&mut self, record: AuthenticatedVault) -> Result<(), VaultError> {
		match write_record(
			&self.directory,
			record.record(),
			self.vault.limits,
			WriteMode::IfChanged,
		) {
			Ok(()) => {
				self.record = record;
				Ok(())
			}
			Err(error @ VaultError::Write(WriteError::PublishedButNotDurable(_))) => {
				self.record = record;
				Err(error)
			}
			Err(error) => Err(error),
		}
	}
}
fn write_record(
	directory: &Dir,
	record: &StoredVault,
	limits: VaultLimits,
	mode: WriteMode,
) -> Result<(), VaultError> {
	let bytes = json::serialize_to_vec(
		record,
		SerializeOptions {
			limits: json_limits(limits),
			format: JsonFormat::Pretty { indent: "\t" },
		},
	)?;
	directory.write_file_atomically(&relative_path(VAULT_FILE), &bytes, write_options(mode))?;
	Ok(())
}
/// Writes the ignore file only when no entry with its name exists. Any other result
/// of the probe, including a link, a directory, or an unreadable file, is an
/// existing entry, which stays unchanged.
fn ensure_ignore_file(directory: &Dir) -> Result<(), VaultError> {
	let path = relative_path(IGNORE_FILE);
	match directory.open_file(&path) {
		Err(error) if error.kind() == io::ErrorKind::NotFound => {
			directory.write_file_atomically(
				&path,
				IGNORE_CONTENT,
				write_options(WriteMode::IfChanged),
			)?;
			Ok(())
		}
		Ok(_) | Err(_) => Ok(()),
	}
}
fn relative_path(name: &str) -> RelativePath {
	RelativePath::new(name).expect("vault file names are fixed relative paths")
}
fn map_missing(error: io::Error) -> VaultError {
	if error.kind() == io::ErrorKind::NotFound {
		VaultError::MissingVault
	} else {
		VaultError::Io(error)
	}
}
