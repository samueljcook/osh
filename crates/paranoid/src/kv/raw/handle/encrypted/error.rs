//! Actual input, Raw and Crypto causes retain their independent ownership and logical context.

use primitives_rs::crypto::encryption::{DecryptError, EncryptError};
use std::{error::Error, fmt, str::Utf8Error};

use crate::db::DatabaseError;
use crate::kv::{
	KeyError, PgRawError, PgRawErrorKind, raw::model::encryption::PublishedItemKeyError,
};
use crate::records::{RecordIdentity, RecordKey, RecordNamespace};
use crate::scope::ScopeIdentity;

use super::super::PgRaw;

pub(super) enum EncryptedRawCause {
	Input(PublishedItemKeyError),
	Raw(PgRawError),
	Encryption(EncryptError),
	Decryption(DecryptError),
}
pub(super) struct PgEncryptedRawErrorFields<'a, 'scope> {
	pub(super) raw: &'a PgRaw<'scope>,
	pub(super) key: &'a RecordKey,
	pub(super) source: EncryptedRawCause,
}
struct EncryptedRawDiagnostic {
	scope: Option<ScopeIdentity>,
	record: RecordIdentity,
	source: EncryptedRawCause,
}

/// A failed encrypted Raw operation on one record.
///
/// Formatting the error omits keysets, keys, plaintext, and ciphertext. A key or encryption
/// failure happens before any database work, and a decryption failure happens after a
/// successful read.
pub struct PgEncryptedRawError {
	diagnostic: Box<EncryptedRawDiagnostic>,
}
/// The class of a failed encrypted Raw operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PgEncryptedRawErrorKind {
	/// The key is not a valid v10 key, which the encrypted format binds to the value.
	V10Key,
	/// The Raw operation failed with this kind. `PgEncryptedRawError::raw_error` returns the
	/// Raw error.
	Raw(PgRawErrorKind),
	/// Encrypting the value failed, so Paranoid wrote nothing.
	Encryption,
	/// Decrypting the stored live value failed.
	Decryption,
}
impl PgEncryptedRawError {
	pub(super) fn new(fields: PgEncryptedRawErrorFields<'_, '_>) -> Self {
		Self {
			diagnostic: Box::new(EncryptedRawDiagnostic {
				scope: fields.raw.scope().cloned(),
				record: RecordIdentity::new(fields.raw.namespace().clone(), fields.key.clone()),
				source: fields.source,
			}),
		}
	}
	/// The handle's app scope, or `None` for the core Raw namespace.
	pub fn scope(&self) -> Option<&ScopeIdentity> {
		self.diagnostic.scope.as_ref()
	}
	/// The handle's namespace.
	pub fn namespace(&self) -> &RecordNamespace {
		self.diagnostic.record.namespace()
	}
	/// The requested record.
	pub fn record_identity(&self) -> &RecordIdentity {
		&self.diagnostic.record
	}
	/// The class of the failure.
	pub fn kind(&self) -> PgEncryptedRawErrorKind {
		match &self.diagnostic.source {
			EncryptedRawCause::Input(_) => PgEncryptedRawErrorKind::V10Key,
			EncryptedRawCause::Raw(source) => PgEncryptedRawErrorKind::Raw(source.kind()),
			EncryptedRawCause::Encryption(_) => PgEncryptedRawErrorKind::Encryption,
			EncryptedRawCause::Decryption(_) => PgEncryptedRawErrorKind::Decryption,
		}
	}
	/// For `V10Key`, the UTF-8 failure when the key is not UTF-8.
	pub fn utf8_error(&self) -> Option<&Utf8Error> {
		match &self.diagnostic.source {
			EncryptedRawCause::Input(PublishedItemKeyError::Utf8(source)) => Some(source),
			_ => None,
		}
	}
	/// For `V10Key`, the reason that the key is not a valid v10 key.
	pub fn key_error(&self) -> Option<&KeyError> {
		match &self.diagnostic.source {
			EncryptedRawCause::Input(PublishedItemKeyError::Key(source)) => Some(source),
			_ => None,
		}
	}
	/// For `Raw`, the Raw error.
	pub fn raw_error(&self) -> Option<&PgRawError> {
		match &self.diagnostic.source {
			EncryptedRawCause::Raw(source) => Some(source),
			_ => None,
		}
	}
	/// For `Encryption`, the encryption failure.
	pub fn encryption_error(&self) -> Option<&EncryptError> {
		match &self.diagnostic.source {
			EncryptedRawCause::Encryption(source) => Some(source),
			_ => None,
		}
	}
	/// For `Decryption`, the decryption failure.
	pub fn decryption_error(&self) -> Option<&DecryptError> {
		match &self.diagnostic.source {
			EncryptedRawCause::Decryption(source) => Some(source),
			_ => None,
		}
	}
	/// The database error of a failed Raw operation.
	pub fn database_error(&self) -> Option<&DatabaseError> {
		self.raw_error().and_then(PgRawError::database_error)
	}
	/// Whether the transaction's own `COMMIT` failed, which means that the operation's changes
	/// may or may not have been stored.
	pub fn commit_outcome_is_unknown(&self) -> bool {
		self.raw_error()
			.is_some_and(PgRawError::commit_outcome_is_unknown)
	}
}
impl fmt::Debug for PgEncryptedRawError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("PgEncryptedRawError")
			.finish_non_exhaustive()
	}
}
impl fmt::Display for PgEncryptedRawError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("the encrypted Raw operation failed")
	}
}
impl Error for PgEncryptedRawError {
	fn source(&self) -> Option<&(dyn Error + 'static)> {
		Some(match &self.diagnostic.source {
			EncryptedRawCause::Input(source) => source,
			EncryptedRawCause::Raw(source) => source,
			EncryptedRawCause::Encryption(source) => source,
			EncryptedRawCause::Decryption(source) => source,
		})
	}
}
