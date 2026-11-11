//! Local encrypted environment values, application-defined profiles, and atomic edits.
//!
//! Enable `local-env-vault` on Unix. Applications choose a trusted root directory,
//! a relative vault directory, profiles, password costs, and resource limits.
//! Filesystem calls and password derivation are synchronous; async applications
//! can move a complete operation to their blocking executor. Once started, such
//! an operation is not cancelled by dropping its executor future.
//!
//! A password protects a random data key through Primitives' `PasswordEncryptedKey`.
//! Individual values use Primitives' hedged encryption protocol, and a separate
//! derived keyset authenticates the complete record. A password change generates a
//! new data key, encrypts every stored value again with keys derived from it, and
//! protects the new data key with the new password. An earlier copy of the vault
//! file and its password therefore neither decrypt values in the current file nor
//! authenticate changes to it. Variable names and format metadata are readable
//! without unlocking the vault.
//!
//! Passwords are bytes. The vault uses them as given, without Unicode
//! normalization, so the same visible text in another normalization form is a
//! different password. [`VaultPasswordOptions::min_password_len`] applies to
//! creation and password changes.
//!
//! Each profile selects stored environment variable names. Values are shared by
//! name across profiles. Reads decrypt only selected values after authenticating
//! the complete record. [`LocalEnvVault::check_profile_completeness`] checks presence
//! without a password; it does not authenticate ciphertext.
//!
//! [`VaultEditSession`] holds the writer lock until dropped. Each mutation saves
//! one complete file atomically and synchronizes the file and its directory.
//! Readers see complete snapshots and do not block on an interactive editor.
//! Vault creation publishes the first file only if no file exists at that moment.
//! Only vault creation needs this create-only publication, which Primitives
//! supports on Linux, Android, Apple platforms, and Redox; on other platforms,
//! vault creation fails. Storage requires effective-user ownership and applies mode `0700` to the vault
//! directory and `0600` to its files, including during read operations. Applications
//! must keep the vault directory and persistent lock entry stable
//! while cooperating processes use them. File authentication cannot detect
//! restoration of a complete previously valid file.
//!
//! Each operation that takes the writer lock creates a `.gitignore` file that
//! contains `*` in the vault directory when no entry with that name exists, so that
//! Git does not track the vault file, its lock file, or the ignore file. An existing
//! `.gitignore` entry is left unchanged.
//!
//! The `local-env-vault-cli` feature adds `VaultCmdRunner`. It provides
//! `configure`, `check PROFILE`, and `run PROFILE -- COMMAND [ARG ...]`.
//!
//! ```no_run
//! use labs_rs::local_env_vault::{LocalEnvVault, VaultOptions, VaultProfile,
//!     VaultLimits, VaultPasswordOptions, EnvironmentVariableName};
//! use primitives_rs::{fs::{Dir, RelativePath}, crypto::{algorithms::argon2id, password_derivation::ResourceLimits}};
//!
//! # fn example(root: &std::path::Path, password: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
//! let vault = LocalEnvVault::new(VaultOptions {
//!     root_dir: Dir::open(root)?,
//!     relative_vault_path: RelativePath::new(".local_env_vault")?,
//!     profiles: vec![VaultProfile::new("app", ["DATABASE_URL"])?],
//!     password_options: VaultPasswordOptions {
//!         argon2id_profile: argon2id::Profile::new(argon2id::Costs {
//!             memory_kib: 65536, iterations: 3, lanes: 4,
//!         })?,
//!         limits: ResourceLimits { max_memory_kib: 65536, max_iterations: 3,
//!             max_lanes: 4, max_password_len: 1024 },
//!         min_password_len: 12,
//!     },
//!     limits: VaultLimits { max_file_size: 4 * 1024 * 1024,
//!         max_value_size: 64 * 1024, max_entry_count: 256 },
//! })?;
//! let mut editor = vault.create(password)?;
//! editor.set_value(&EnvironmentVariableName::new("DATABASE_URL")?, b"example value")?;
//! drop(editor);
//! let environment = vault.decrypt_profile("app", password)?;
//! // Supply these secret values to the application's launcher without logging them.
//! # let _ = environment;
//! # Ok(()) }
//! ```

use primitives_rs::{
	crypto::{Key32, algorithms::argon2id, keysets::Keyset, password_derivation},
	fs::{Dir, RelativePath},
	processes::local_locks::ProcessLock,
	secret_bytes::SecretBytes,
};
use std::{
	collections::{BTreeMap, BTreeSet},
	sync::Arc,
};

#[cfg(feature = "local-env-vault-cli")]
mod command;
mod error;
mod record;
mod storage;
#[cfg(feature = "local-env-vault-cli")]
pub use command::{CmdEnvironmentInheritance, VaultCmdOptions, VaultCmdOutcome, VaultCmdRunner};
pub use error::VaultError;
use record::{AuthenticatedVault, StoredVault, VaultKeysets, VaultRecordBody};

/// A portable environment variable name: `[A-Za-z_][A-Za-z0-9_]*`.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct EnvironmentVariableName(String);
impl EnvironmentVariableName {
	/// Parses a nonempty ASCII name without including invalid input in errors.
	pub fn new(name: impl Into<String>) -> Result<Self, VaultError> {
		let name = name.into();
		if name.is_empty()
			|| !name.bytes().enumerate().all(|(index, byte)| {
				byte == b'_' || byte.is_ascii_alphabetic() || (index != 0 && byte.is_ascii_digit())
			}) {
			return Err(VaultError::InvalidName);
		}
		Ok(Self(name))
	}
	/// Returns the parsed environment variable name.
	pub fn as_str(&self) -> &str {
		&self.0
	}
}

/// A named set of required vault variables, supplied by the application.
#[derive(Clone, Debug)]
pub struct VaultProfile {
	name: String,
	required_variable_names: BTreeSet<EnvironmentVariableName>,
}
impl VaultProfile {
	/// Parses a profile and its nonempty variable inventory.
	/// Profile names use `[A-Za-z0-9_][A-Za-z0-9_-]*`; duplicate variables collapse.
	pub fn new<I, S>(
		name: impl Into<String>,
		required_variable_names: I,
	) -> Result<Self, VaultError>
	where
		I: IntoIterator<Item = S>,
		S: Into<String>,
	{
		let name = name.into();
		if name.is_empty()
			|| !name.bytes().enumerate().all(|(index, byte)| {
				byte.is_ascii_alphanumeric() || byte == b'_' || (index != 0 && byte == b'-')
			}) {
			return Err(VaultError::InvalidName);
		}
		let required_variable_names = required_variable_names
			.into_iter()
			.map(EnvironmentVariableName::new)
			.collect::<Result<BTreeSet<_>, _>>()?;
		if required_variable_names.is_empty() {
			return Err(VaultError::InvalidConfig(
				"a profile must require at least one variable",
			));
		}
		Ok(Self {
			name,
			required_variable_names,
		})
	}
	/// Returns the application-supplied profile name.
	pub fn name(&self) -> &str {
		&self.name
	}
	/// Returns the required variable names in sorted order.
	pub fn required_variable_names(&self) -> &BTreeSet<EnvironmentVariableName> {
		&self.required_variable_names
	}
}

/// Explicit storage bounds, applied to both incoming records and new writes.
#[derive(Clone, Copy, Debug)]
pub struct VaultLimits {
	/// Maximum complete JSON file size in bytes.
	pub max_file_size: usize,
	/// Maximum plaintext size of one UTF-8 environment value in bytes.
	pub max_value_size: usize,
	/// Maximum number of stored variables, including unused entries.
	pub max_entry_count: usize,
}

/// Password costs and length policy for new records, and independent ceilings for
/// records read from disk.
#[derive(Clone, Debug)]
pub struct VaultPasswordOptions {
	/// Costs for creation and password changes.
	pub argon2id_profile: argon2id::Profile,
	/// Resource ceilings applied before password derivation.
	pub limits: password_derivation::ResourceLimits,
	/// Minimum length in bytes of a password for creation or a password change.
	/// It must be from 1 through `limits.max_password_len`. Opening an existing
	/// vault does not check it.
	pub min_password_len: usize,
}
impl VaultPasswordOptions {
	fn check_new_password(&self, password: &[u8]) -> Result<(), VaultError> {
		if password.len() < self.min_password_len {
			return Err(VaultError::PasswordTooShort);
		}
		if password.len() > self.limits.max_password_len {
			return Err(VaultError::PasswordTooLong);
		}
		Ok(())
	}
}

/// Explicit application configuration for one vault.
pub struct VaultOptions {
	/// An already-opened trusted root; later ambient path changes do not redirect it.
	pub root_dir: Dir,
	/// A relative directory used exclusively for vault storage.
	pub relative_vault_path: RelativePath,
	/// The application's supported profiles.
	pub profiles: Vec<VaultProfile>,
	/// Password creation costs and accepted resource bounds.
	pub password_options: VaultPasswordOptions,
	/// Bounds for records and decrypted values.
	pub limits: VaultLimits,
}

/// Application-owned vault configuration. Construction performs no filesystem writes.
pub struct LocalEnvVault {
	root_dir: Dir,
	relative_vault_path: RelativePath,
	profiles: BTreeMap<String, VaultProfile>,
	required_variable_names: BTreeSet<EnvironmentVariableName>,
	password_options: VaultPasswordOptions,
	limits: VaultLimits,
}

/// Presence information from a structurally valid, unauthenticated record.
#[derive(Clone, Debug)]
pub struct ProfileCompleteness {
	/// Required variables with stored entries.
	pub stored_variable_names: Vec<EnvironmentVariableName>,
	/// Required variables without stored entries.
	pub missing_variable_names: Vec<EnvironmentVariableName>,
}

/// Decrypted values for one profile, with the complete managed-name inventory.
/// Values are zeroized on drop. No `Debug` representation exposes their contents.
pub struct ProfileEnvironment {
	values: BTreeMap<EnvironmentVariableName, SecretBytes>,
	managed_variable_names: BTreeSet<EnvironmentVariableName>,
}
impl ProfileEnvironment {
	/// Returns the selected profile's decrypted values for a custom launcher.
	pub fn values(&self) -> &BTreeMap<EnvironmentVariableName, SecretBytes> {
		&self.values
	}
	/// Returns configured and stored names that a launcher should remove before injection.
	pub fn managed_variable_names(&self) -> &BTreeSet<EnvironmentVariableName> {
		&self.managed_variable_names
	}
}

/// Owns an authenticated vault snapshot and its writer lock.
/// Every mutation persists immediately; dropping this value releases the lock.
pub struct VaultEditSession<'a> {
	vault: &'a LocalEnvVault,
	directory: Dir,
	_lock: ProcessLock,
	record: AuthenticatedVault,
}

impl LocalEnvVault {
	/// Checks configuration without creating directories, files, or processes.
	pub fn new(options: VaultOptions) -> Result<Self, VaultError> {
		let password_options = &options.password_options;
		password_options
			.limits
			.check(&password_options.argon2id_profile, 0)
			.map_err(|_| VaultError::PasswordPolicy)?;
		if password_options.min_password_len == 0
			|| password_options.min_password_len > password_options.limits.max_password_len
		{
			return Err(VaultError::InvalidConfig(
				"the minimum password length must be from 1 through the maximum password length",
			));
		}
		if options.limits.max_file_size == 0
			|| options.limits.max_entry_count == 0
			|| options.limits.max_value_size > primitives_rs::crypto::encryption::MAX_PLAINTEXT_LEN
		{
			return Err(VaultError::InvalidConfig("invalid vault storage bounds"));
		}
		let mut profiles = BTreeMap::new();
		for profile in options.profiles {
			if profiles.insert(profile.name.clone(), profile).is_some() {
				return Err(VaultError::InvalidConfig("profile names must be unique"));
			}
		}
		if profiles.is_empty() {
			return Err(VaultError::InvalidConfig(
				"at least one profile is required",
			));
		}
		let required_variable_names = profiles
			.values()
			.flat_map(|profile| profile.required_variable_names.iter().cloned())
			.collect::<BTreeSet<_>>();
		if required_variable_names.len() > options.limits.max_entry_count {
			return Err(VaultError::InvalidConfig(
				"required variables exceed the entry limit",
			));
		}
		Ok(Self {
			root_dir: options.root_dir,
			relative_vault_path: options.relative_vault_path,
			profiles,
			required_variable_names,
			password_options: options.password_options,
			limits: options.limits,
		})
	}
	/// Returns declared profiles in name order.
	pub fn profiles(&self) -> impl ExactSizeIterator<Item = &VaultProfile> {
		self.profiles.values()
	}
	/// Returns the union of the profiles' required variables.
	pub fn required_variable_names(&self) -> &BTreeSet<EnvironmentVariableName> {
		&self.required_variable_names
	}
	/// Checks required entry presence without deriving a key or authenticating ciphertext.
	pub fn check_profile_completeness(
		&self,
		profile_name: &str,
	) -> Result<ProfileCompleteness, VaultError> {
		let profile = self.profile(profile_name)?;
		let directory = self.open_directory()?;
		let record = self.read_record(&directory)?;
		Ok(profile_completeness(profile, &record.body))
	}
	/// Creates an empty encrypted vault and returns an editor with the writer lock held.
	/// Existing files, including invalid vaults and files that appear during this
	/// operation, are never replaced by this operation.
	pub fn create(&self, password: &[u8]) -> Result<VaultEditSession<'_>, VaultError> {
		let (directory, lock) = self.lock_directory()?;
		match self.read_record(&directory) {
			Err(VaultError::MissingVault) => {}
			Ok(_) => return Err(VaultError::VaultAlreadyExists),
			Err(error) => return Err(error),
		}
		self.create_locked(directory, lock, password)
	}
	fn create_locked(
		&self,
		directory: Dir,
		lock: ProcessLock,
		password: &[u8],
	) -> Result<VaultEditSession<'_>, VaultError> {
		self.password_options.check_new_password(password)?;
		let master_key = Key32::generate().map_err(|_| VaultError::Crypto)?;
		let body = VaultRecordBody::create(&master_key, password, &self.password_options)?;
		let keys = VaultKeysets::new(&master_key)?;
		let record = body.authenticate(Arc::new(keys), self.limits)?;
		self.publish_new_record(&directory, record.record())?;
		Ok(VaultEditSession {
			vault: self,
			directory,
			_lock: lock,
			record,
		})
	}
	/// Locks and authenticates an existing vault before returning an editor.
	pub fn edit(&self, password: &[u8]) -> Result<VaultEditSession<'_>, VaultError> {
		let (directory, lock) = self.lock_directory()?;
		let record = self.read_record(&directory)?;
		self.edit_locked(directory, lock, record, password)
	}
	fn edit_locked(
		&self,
		directory: Dir,
		lock: ProcessLock,
		record: StoredVault,
		password: &[u8],
	) -> Result<VaultEditSession<'_>, VaultError> {
		let record = record.unlock(password, &self.password_options, self.limits)?;
		Ok(VaultEditSession {
			vault: self,
			directory,
			_lock: lock,
			record,
		})
	}
	/// Authenticates one snapshot and decrypts only the selected profile's values.
	/// Missing required values fail before password derivation. No writer lock is held.
	pub fn decrypt_profile(
		&self,
		profile_name: &str,
		password: &[u8],
	) -> Result<ProfileEnvironment, VaultError> {
		let (profile, record) = self.read_profile_record(profile_name)?;
		self.decrypt_profile_record(profile, record, password)
	}
	fn profile(&self, name: &str) -> Result<&VaultProfile, VaultError> {
		self.profiles.get(name).ok_or(VaultError::UnknownProfile)
	}
	fn read_profile_record(&self, name: &str) -> Result<(&VaultProfile, StoredVault), VaultError> {
		let profile = self.profile(name)?;
		let record = self.read_record(&self.open_directory()?)?;
		if !profile_completeness(profile, &record.body)
			.missing_variable_names
			.is_empty()
		{
			return Err(VaultError::MissingValues);
		}
		Ok((profile, record))
	}
	fn decrypt_profile_record(
		&self,
		profile: &VaultProfile,
		record: StoredVault,
		password: &[u8],
	) -> Result<ProfileEnvironment, VaultError> {
		let record = record.unlock(password, &self.password_options, self.limits)?;
		let mut values = BTreeMap::new();
		for name in &profile.required_variable_names {
			values.insert(name.clone(), record.decrypt_value(name, self.limits)?);
		}
		let mut managed_variable_names = self.required_variable_names.clone();
		for name in record.body().entries.keys() {
			managed_variable_names.insert(EnvironmentVariableName::new(name.clone())?);
		}
		Ok(ProfileEnvironment {
			values,
			managed_variable_names,
		})
	}
}

impl VaultEditSession<'_> {
	/// Returns stored names without decrypting their values.
	pub fn stored_variable_names(&self) -> impl Iterator<Item = &str> {
		self.record.body().entries.keys().map(String::as_str)
	}
	/// Reports stored and missing variables for a declared profile.
	pub fn check_profile_completeness(
		&self,
		name: &str,
	) -> Result<ProfileCompleteness, VaultError> {
		Ok(profile_completeness(
			self.vault.profile(name)?,
			self.record.body(),
		))
	}
	/// Encrypts and atomically saves one declared variable; empty values are valid.
	pub fn set_value(
		&mut self,
		name: &EnvironmentVariableName,
		value: &[u8],
	) -> Result<(), VaultError> {
		if !self.vault.required_variable_names.contains(name) {
			return Err(VaultError::UnknownVariable);
		}
		validate_value(value, self.vault.limits)?;
		let record = self.record.with_value(name, value, self.vault.limits)?;
		self.publish_record(record)
	}
	/// Lists stored variables that no declared profile requires.
	pub fn unused_variable_names(&self) -> Vec<EnvironmentVariableName> {
		self.record
			.body()
			.entries
			.keys()
			.filter_map(|name| {
				let name = EnvironmentVariableName(name.clone());
				(!self.vault.required_variable_names.contains(&name)).then_some(name)
			})
			.collect()
	}
	/// Explicitly removes all unused variables and returns their names.
	pub fn remove_unused_values(&mut self) -> Result<Vec<EnvironmentVariableName>, VaultError> {
		let unused = self.unused_variable_names();
		if !unused.is_empty() {
			let record = self.record.without_values(&unused, self.vault.limits)?;
			self.publish_record(record)?;
		}
		Ok(unused)
	}
	/// Generates a new data key, encrypts every stored value again with keys derived
	/// from it, and protects it with `password` and the configured costs. The
	/// complete replacement is one atomic write. Afterward, an earlier copy of the
	/// vault file and the earlier password neither decrypt values in the saved file
	/// nor authenticate changes to it.
	pub fn change_password(&mut self, password: &[u8]) -> Result<(), VaultError> {
		let options = &self.vault.password_options;
		options.check_new_password(password)?;
		let record = self
			.record
			.with_new_master_key(password, options, self.vault.limits)?;
		self.publish_record(record)
	}
}

fn profile_completeness(profile: &VaultProfile, body: &VaultRecordBody) -> ProfileCompleteness {
	let (stored_variable_names, missing_variable_names) = profile
		.required_variable_names
		.iter()
		.cloned()
		.partition(|name| body.entries.contains_key(name.as_str()));
	ProfileCompleteness {
		stored_variable_names,
		missing_variable_names,
	}
}
fn validate_value(value: &[u8], limits: VaultLimits) -> Result<(), VaultError> {
	if value.len() > limits.max_value_size
		|| value.contains(&0)
		|| std::str::from_utf8(value).is_err()
	{
		Err(VaultError::InvalidValue)
	} else {
		Ok(())
	}
}
