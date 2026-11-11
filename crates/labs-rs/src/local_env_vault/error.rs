use primitives_rs::{
	crypto::{self, password_derivation},
	fs::WriteError,
	io::bounded_reads::ReadError,
	processes::local_locks::LockError,
	serialization::json::JsonError,
};
use std::{fmt, io};

/// A vault failure. Formatting excludes passwords, values, ciphertext, and JSON text.
#[derive(Debug)]
pub enum VaultError {
	/// The application supplied invalid configuration.
	InvalidConfig(&'static str),
	/// A profile name was not declared by the application.
	UnknownProfile,
	/// A requested environment variable was not declared by any profile.
	UnknownVariable,
	/// A name does not follow the documented grammar.
	InvalidName,
	/// An environment value contains a NUL, is not UTF-8, or exceeds its size limit.
	InvalidValue,
	/// The vault file does not exist.
	MissingVault,
	/// Creating a vault would replace an existing file.
	VaultAlreadyExists,
	/// At least one required profile value is absent.
	MissingValues,
	/// A stored record is malformed, unsupported, or exceeds an application limit.
	InvalidRecord,
	/// Saving a value would store more variables than the configured entry limit.
	EntryLimitExceeded,
	/// A password or stored record failed authentication.
	AuthenticationFailed,
	/// The password derivation costs exceed the configured resource limits.
	PasswordPolicy,
	/// A new password is shorter than the configured minimum length.
	PasswordTooShort,
	/// A password is longer than the configured maximum length.
	PasswordTooLong,
	/// A cryptographic operation failed without exposing its inputs.
	Crypto,
	/// A filesystem or terminal operation failed.
	Io(io::Error),
	/// Acquiring the writer lock failed.
	Lock(LockError),
	/// Atomic replacement failed, possibly after publication; inspect the inner variant.
	Write(WriteError),
	/// Bounded JSON serialization or parsing failed.
	Json(JsonError),
	/// A bounded file read failed.
	Read(ReadError),
	/// Interactive input or command execution was cancelled.
	#[cfg(feature = "local-env-vault-cli")]
	Cancelled,
	/// New-password confirmation did not match.
	#[cfg(feature = "local-env-vault-cli")]
	PasswordConfirmationMismatch,
	/// Command arguments do not match the wrapper's documented command grammar.
	#[cfg(feature = "local-env-vault-cli")]
	InvalidCmd,
	/// Subprocess creation, termination, or reaping failed.
	#[cfg(feature = "local-env-vault-cli")]
	Process(primitives_rs::processes::child_processes::ProcessError),
}

impl fmt::Display for VaultError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::InvalidConfig(reason) => {
				write!(f, "invalid vault configuration: {reason}")
			}
			Self::UnknownProfile => f.write_str("unknown vault profile"),
			Self::UnknownVariable => f.write_str("unknown vault variable"),
			Self::InvalidName => f.write_str("invalid vault profile or environment variable name"),
			Self::InvalidValue => {
				f.write_str("environment value must be bounded UTF-8 without NUL")
			}
			Self::MissingVault => f.write_str("vault does not exist; configure it before use"),
			Self::VaultAlreadyExists => f.write_str("vault already exists"),
			Self::MissingValues => f.write_str("profile has missing vault values"),
			Self::InvalidRecord => f.write_str("invalid or unsupported vault record"),
			Self::EntryLimitExceeded => {
				f.write_str("vault would exceed the configured entry limit")
			}
			Self::AuthenticationFailed => {
				f.write_str("vault password or record authentication failed")
			}
			Self::PasswordPolicy => {
				f.write_str("password derivation costs exceed the configured limits")
			}
			Self::PasswordTooShort => {
				f.write_str("password is shorter than the configured minimum length")
			}
			Self::PasswordTooLong => {
				f.write_str("password is longer than the configured maximum length")
			}
			Self::Crypto => f.write_str("vault cryptographic operation failed"),
			Self::Io(error) => write!(f, "vault I/O failed: {error}"),
			Self::Lock(error) => write!(f, "vault lock failed: {error}"),
			Self::Write(error) => write!(f, "vault write failed: {error}"),
			Self::Json(error) => write!(f, "vault JSON failed: {error}"),
			Self::Read(error) => write!(f, "vault read failed: {error}"),
			#[cfg(feature = "local-env-vault-cli")]
			Self::Cancelled => f.write_str("vault operation cancelled"),
			#[cfg(feature = "local-env-vault-cli")]
			Self::PasswordConfirmationMismatch => f.write_str("password confirmation did not match"),
			#[cfg(feature = "local-env-vault-cli")]
			Self::InvalidCmd => f.write_str(
				"expected configure, check PROFILE, or run PROFILE -- COMMAND [ARG ...]",
			),
			#[cfg(feature = "local-env-vault-cli")]
			Self::Process(error) => write!(f, "vault child process failed: {error}"),
		}
	}
}
impl std::error::Error for VaultError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self {
			Self::Io(error) => Some(error),
			Self::Lock(error) => Some(error),
			Self::Write(error) => Some(error),
			Self::Json(error) => Some(error),
			Self::Read(error) => Some(error),
			#[cfg(feature = "local-env-vault-cli")]
			Self::Process(error) => Some(error),
			_ => None,
		}
	}
}
impl From<io::Error> for VaultError {
	fn from(error: io::Error) -> Self {
		Self::Io(error)
	}
}
impl From<LockError> for VaultError {
	fn from(error: LockError) -> Self {
		Self::Lock(error)
	}
}
impl From<WriteError> for VaultError {
	fn from(error: WriteError) -> Self {
		Self::Write(error)
	}
}
impl From<JsonError> for VaultError {
	fn from(error: JsonError) -> Self {
		Self::Json(error)
	}
}
impl From<crypto::password_encrypted_key::Error> for VaultError {
	fn from(error: crypto::password_encrypted_key::Error) -> Self {
		use crypto::password_encrypted_key::Error;
		match error {
			Error::AuthenticationFailed => Self::AuthenticationFailed,
			Error::Derivation(error) => match error {
				password_derivation::Error::ResourceLimitExceeded => Self::PasswordPolicy,
				password_derivation::Error::PasswordTooLong => Self::PasswordTooLong,
				password_derivation::Error::InvalidParameters => Self::InvalidRecord,
				password_derivation::Error::Argon2id(_) => Self::Crypto,
			},
			_ => Self::Crypto,
		}
	}
}
