//! OS credential storage for small byte secrets.
//!
//! Applications select Secret Service on Linux or the user's default Keychain on
//! macOS through an explicit constructor and Cargo feature. No process-global
//! backend is configured. Missing or inaccessible storage returns an error;
//! there is no file fallback. Store access is synchronous and may invoke the OS
//! unlock or access prompt. Async applications should use a blocking executor.
//!
//! Service and account identifiers are public OS metadata. They must not contain
//! secret material. These identifiers select entries; they do not establish
//! application authorization. Protection at rest and access prompts depend on
//! the user's OS credential-store configuration.
//!
//! Values use the UTF-8 format `primitives-rs:credential:v1:` followed by canonical
//! unpadded Base64url. This permits arbitrary bytes, including empty values, on
//! Secret Service providers that accept only UTF-8 secrets. The format is an
//! encoding, not encryption; the OS store provides confidentiality. Other stored
//! formats are rejected. Library-owned secret buffers are zeroized on drop, but
//! OS services and backend dependencies control their own memory copies.
//!
//! Each write creates or replaces one entry. Operations do not provide transactions
//! or compare-and-swap. Applications must coordinate concurrent mutations to the
//! same identity. Ambiguous existing entries produce an error instead of selecting
//! or deleting an arbitrary entry. Password recovery should remain independent of
//! this device-local convenience facility.

//! ```no_run
//! use primitives_rs::os::credential_store::{Store, EntryIdentity, EntryOptions};
//! # fn example(store: &Store) -> Result<(), Box<dyn std::error::Error>> {
//! let entry = store.entry(EntryIdentity {
//!     service: "com.example.notes", account: "local-unlock-key",
//! }, EntryOptions { max_secret_size: 32 })?;
//! let key = primitives_rs::secret_bytes::SecretBytes::generate_random(32)?;
//! entry.write(key.expose_secret())?;
//! let recovered = entry.read()?;
//! assert_eq!(recovered.expose_secret(), key.expose_secret());
//! entry.delete()?;
//! # Ok(())
//! # }
//! ```

use std::sync::Arc;
use zeroize::{Zeroize, Zeroizing};

use crate::encoding::binary_to_text::{ByteEncoding, DecodeError, DecodeOptions};
use crate::secret_bytes::SecretBytes;

const RECORD_PREFIX: &str = "primitives-rs:credential:v1:";

/// An explicit OS backend. Entries retain shared ownership of their backend.
pub struct Store {
	backend: Arc<keyring_core::CredentialStore>,
}

impl Store {
	/// Connects to the desktop session's Secret Service using encrypted D-Bus
	/// transport. Entries are created in its default collection and searched across
	/// collections. The service must already provide an accessible default collection.
	#[cfg(all(target_os = "linux", feature = "credential-store-secret-service"))]
	pub fn secret_service() -> Result<Self, Error> {
		Ok(Self {
			backend: zbus_secret_service_keyring_store::Store::new().map_err(map_error)?,
		})
	}

	/// Selects the user domain's default Keychain, normally the login Keychain.
	/// This does not select iCloud Keychain or an application's protected-data
	/// keychain. OS access checks occur during use.
	#[cfg(all(target_os = "macos", feature = "credential-store-macos-keychain"))]
	pub fn macos_keychain() -> Result<Self, Error> {
		Ok(Self {
			backend: apple_native_keyring_store::keychain::Store::new().map_err(map_error)?,
		})
	}

	/// Selects one service/account identity without creating or reading its value.
	/// Both identifiers must be nonempty and contain no NUL characters. Backend
	/// length restrictions remain additional constraints.
	pub fn entry(
		&self,
		identity: EntryIdentity<'_>,
		options: EntryOptions,
	) -> Result<Entry, Error> {
		if [identity.service, identity.account]
			.iter()
			.any(|value| value.is_empty() || value.contains('\0'))
		{
			return Err(Error::InvalidIdentity);
		}
		let inner = self
			.backend
			.build(identity.service, identity.account, None)
			.map_err(map_error)?;
		Ok(Entry {
			inner,
			max_secret_size: options.max_secret_size,
		})
	}
}

impl std::fmt::Debug for Store {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Store").finish_non_exhaustive()
	}
}

/// Public OS metadata that identifies one secret.
#[derive(Clone, Copy, Debug)]
pub struct EntryIdentity<'a> {
	/// Application or service identifier, for example a reverse DNS application name.
	pub service: &'a str,
	/// Account or application-defined entry identifier within that service.
	pub account: &'a str,
}

/// Limits for an entry's plaintext bytes.
#[derive(Clone, Copy, Debug)]
pub struct EntryOptions {
	/// Maximum bytes accepted by `write` and returned by `read`.
	/// The OS backend allocates its response before this bound can be checked.
	/// Backend limits also apply to the larger encoded representation.
	pub max_secret_size: usize,
}

/// One credential identity with a bounded byte-value contract.
pub struct Entry {
	inner: keyring_core::Entry,
	max_secret_size: usize,
}

impl std::fmt::Debug for Entry {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Entry")
			.field("max_secret_size", &self.max_secret_size)
			.finish_non_exhaustive()
	}
}

impl Entry {
	/// Creates or replaces this entry's value. The caller retains responsibility
	/// for clearing the borrowed input. A backend error does not establish whether
	/// a remote service applied a write before communication failed.
	pub fn write(&self, secret: &[u8]) -> Result<(), Error> {
		if secret.len() > self.max_secret_size {
			return Err(Error::SecretTooLarge);
		}
		let encoded = Zeroizing::new(ByteEncoding::Base64UrlUnpadded.encode(secret));
		let mut record = Zeroizing::new(String::new());
		let size = RECORD_PREFIX
			.len()
			.checked_add(encoded.len())
			.ok_or(Error::AllocationFailed)?;
		record
			.try_reserve_exact(size)
			.map_err(|_| Error::AllocationFailed)?;
		record.push_str(RECORD_PREFIX);
		record.push_str(&encoded);
		self.inner.set_secret(record.as_bytes()).map_err(map_error)
	}

	/// Retrieves and decodes the value into a zeroizing buffer. Missing entries
	/// return `NotFound`; invalid encodings and excessive lengths remain errors.
	pub fn read(&self) -> Result<SecretBytes, Error> {
		let record = Zeroizing::new(self.inner.get_secret().map_err(map_error)?);
		let text = std::str::from_utf8(&record).map_err(|_| Error::InvalidStoredValue)?;
		let encoded = text
			.strip_prefix(RECORD_PREFIX)
			.ok_or(Error::InvalidStoredValue)?;
		let bytes = ByteEncoding::Base64UrlUnpadded
			.decode(
				encoded,
				DecodeOptions {
					max_decoded_size: self.max_secret_size,
				},
			)
			.map_err(|error| match error {
				DecodeError::OutputTooSmall => Error::SecretTooLarge,
				DecodeError::AllocationFailed => Error::AllocationFailed,
				_ => Error::InvalidStoredValue,
			})?;
		Ok(SecretBytes::from_vec(bytes))
	}

	/// Deletes one matching credential. An absent entry returns `NotFound`.
	/// Deletion is not a guarantee of erasure from OS backups or device storage.
	pub fn delete(&self) -> Result<(), Error> {
		self.inner.delete_credential().map_err(map_error)
	}
}

/// Credential-store failures. Diagnostics contain no secret values, identity
/// strings, or backend-supplied text that might contain stored data.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum Error {
	/// No matching entry exists.
	#[error("credential entry was not found")]
	NotFound,
	/// Storage is missing, locked, inaccessible, or its access prompt was rejected.
	#[error("OS credential storage is not accessible")]
	AccessUnavailable,
	/// A native backend or its transport failed. The result of a mutation may be unknown.
	#[error("OS credential backend operation failed")]
	BackendFailure,
	/// More than one entry matches the selected identity.
	#[error("multiple OS credential entries match this identity")]
	Ambiguous,
	/// An identity is empty or contains a NUL character.
	#[error("OS credential identity is invalid")]
	InvalidIdentity,
	/// The stored value does not use this facility's canonical record format.
	#[error("OS credential value has an invalid format")]
	InvalidStoredValue,
	/// The OS store has an invalid internal format.
	#[error("OS credential store has an invalid format")]
	InvalidStore,
	/// A byte value exceeds the entry's explicit bound.
	#[error("OS credential value exceeds its byte limit")]
	SecretTooLarge,
	/// A backend limit rejects an identity or the encoded value.
	#[error("OS credential backend rejected a value or exceeded a size limit")]
	BackendRejectedValue,
	/// The selected backend does not implement a required operation.
	#[error("OS credential backend does not support this operation")]
	Unsupported,
	/// A record or decoded secret allocation failed.
	#[error("OS credential buffer allocation failed")]
	AllocationFailed,
}

fn map_error(error: keyring_core::Error) -> Error {
	match error {
		keyring_core::Error::NoEntry => Error::NotFound,
		keyring_core::Error::NoStorageAccess(_) => Error::AccessUnavailable,
		keyring_core::Error::PlatformFailure(source) => {
			#[cfg(all(target_os = "macos", feature = "credential-store-macos-keychain"))]
			if let Some(error) = source.downcast_ref::<security_framework::base::Error>()
				// The native adapter reports these Security.framework access errors
				// as PlatformFailure rather than NoStorageAccess.
				// errSecUserCanceled, errSecAuthFailed, errSecNoDefaultKeychain,
				// errSecInteractionNotAllowed, and errSecInteractionRequired.
				&& matches!(error.code(), -128 | -25293 | -25307 | -25308 | -25315)
			{
				return Error::AccessUnavailable;
			}
			let _ = source;
			Error::BackendFailure
		}
		keyring_core::Error::Ambiguous(_) => Error::Ambiguous,
		keyring_core::Error::BadEncoding(mut bytes)
		| keyring_core::Error::BadDataFormat(mut bytes, _) => {
			bytes.zeroize();
			Error::InvalidStoredValue
		}
		keyring_core::Error::BadStoreFormat(_) => Error::InvalidStore,
		keyring_core::Error::TooLong(_, _) | keyring_core::Error::Invalid(_, _) => {
			Error::BackendRejectedValue
		}
		keyring_core::Error::NotSupportedByStore(_) => Error::Unsupported,
		_ => Error::BackendFailure,
	}
}
