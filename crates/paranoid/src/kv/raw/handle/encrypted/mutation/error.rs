//! The complete Raw failure owns callback, Crypto, cleanup and ending causes together.

use primitives_rs::crypto::encryption::{DecryptError, EncryptError};
use std::{error::Error, fmt, str::Utf8Error};

use crate::db::DatabaseError;
use crate::kv::{
	KeyError, PgRawError, PgRawErrorKind, raw::model::encryption::PublishedItemKeyError,
};
use crate::records::{RecordIdentity, RecordKey, RecordNamespace};
use crate::scope::ScopeIdentity;

use super::super::super::PgRaw;

pub(super) enum EncryptedRawMutationDecisionError<C> {
	Callback(C),
	Encryption(EncryptError),
	Decryption(DecryptError),
}
impl<C> fmt::Debug for EncryptedRawMutationDecisionError<C> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("EncryptedRawMutationDecisionError")
			.finish_non_exhaustive()
	}
}
impl<C> fmt::Display for EncryptedRawMutationDecisionError<C> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("the encrypted Raw mutation decision failed")
	}
}
impl<C: Error + 'static> Error for EncryptedRawMutationDecisionError<C> {
	fn source(&self) -> Option<&(dyn Error + 'static)> {
		Some(match self {
			Self::Callback(source) => source,
			Self::Encryption(source) => source,
			Self::Decryption(source) => source,
		})
	}
}

pub(super) enum EncryptedRawMutationCause<C> {
	Input(PublishedItemKeyError),
	Raw(PgRawError<EncryptedRawMutationDecisionError<C>>),
}
pub(super) struct PgEncryptedRawMutationErrorFields<'a, 'scope, C> {
	pub(super) raw: &'a PgRaw<'scope>,
	pub(super) key: &'a RecordKey,
	pub(super) source: EncryptedRawMutationCause<C>,
}
struct EncryptedRawMutationDiagnostic<C> {
	scope: Option<ScopeIdentity>,
	record: RecordIdentity,
	source: EncryptedRawMutationCause<C>,
}

/// A failed encrypted Raw mutation. `C` is the error type of the app's callbacks.
///
/// Formatting the error omits keys, keysets, plaintext, ciphertext, and callback errors. A
/// callback, encryption, or decryption failure can come with an uncertain transaction ending,
/// which `commit_outcome_is_unknown` reports.
pub struct PgEncryptedRawMutationError<C> {
	diagnostic: Box<EncryptedRawMutationDiagnostic<C>>,
}

/// The class of a failed encrypted Raw mutation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PgEncryptedRawMutationErrorKind {
	/// The key is not a valid v10 key, which the encrypted format binds to the value.
	V10Key,
	/// The app's initializer or decision callback returned an error.
	Callback,
	/// Encrypting the replacement value that the decision chose failed.
	Encryption,
	/// Decrypting the stored live value failed.
	Decryption,
	/// The Raw operation failed with this kind.
	Raw(PgRawErrorKind),
}
impl<C> PgEncryptedRawMutationError<C> {
	pub(super) fn new(fields: PgEncryptedRawMutationErrorFields<'_, '_, C>) -> Self {
		Self {
			diagnostic: Box::new(EncryptedRawMutationDiagnostic {
				scope: fields.raw.scope().cloned(),
				record: RecordIdentity::new(fields.raw.namespace().clone(), fields.key.clone()),
				source: fields.source,
			}),
		}
	}
	fn raw(&self) -> Option<&PgRawError<EncryptedRawMutationDecisionError<C>>> {
		match &self.diagnostic.source {
			EncryptedRawMutationCause::Raw(source) => Some(source),
			EncryptedRawMutationCause::Input(_) => None,
		}
	}
	fn decision_failure(&self) -> Option<&EncryptedRawMutationDecisionError<C>> {
		self.raw().and_then(PgRawError::callback_error)
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
	pub fn kind(&self) -> PgEncryptedRawMutationErrorKind {
		match &self.diagnostic.source {
			EncryptedRawMutationCause::Input(_) => PgEncryptedRawMutationErrorKind::V10Key,
			EncryptedRawMutationCause::Raw(source) => match self.decision_failure() {
				Some(EncryptedRawMutationDecisionError::Callback(_)) => {
					PgEncryptedRawMutationErrorKind::Callback
				}
				Some(EncryptedRawMutationDecisionError::Encryption(_)) => {
					PgEncryptedRawMutationErrorKind::Encryption
				}
				Some(EncryptedRawMutationDecisionError::Decryption(_)) => {
					PgEncryptedRawMutationErrorKind::Decryption
				}
				None => PgEncryptedRawMutationErrorKind::Raw(source.kind()),
			},
		}
	}
	/// The kind of the underlying Raw error, or `None` for `V10Key`. A callback, encryption, or
	/// decryption failure has the Raw kind `Callback`.
	pub fn raw_kind(&self) -> Option<PgRawErrorKind> {
		self.raw().map(PgRawError::kind)
	}
	/// For `Callback`, the error that the app's callback returned.
	pub fn callback_error(&self) -> Option<&C> {
		match self.decision_failure() {
			Some(EncryptedRawMutationDecisionError::Callback(source)) => Some(source),
			_ => None,
		}
	}
	/// For `V10Key`, the UTF-8 failure when the key is not UTF-8.
	pub fn utf8_error(&self) -> Option<&Utf8Error> {
		match &self.diagnostic.source {
			EncryptedRawMutationCause::Input(PublishedItemKeyError::Utf8(source)) => Some(source),
			_ => None,
		}
	}
	/// For `V10Key`, the reason that the key is not a valid v10 key.
	pub fn key_error(&self) -> Option<&KeyError> {
		match &self.diagnostic.source {
			EncryptedRawMutationCause::Input(PublishedItemKeyError::Key(source)) => Some(source),
			_ => None,
		}
	}
	/// For `Encryption`, the encryption failure.
	pub fn encryption_error(&self) -> Option<&EncryptError> {
		match self.decision_failure() {
			Some(EncryptedRawMutationDecisionError::Encryption(source)) => Some(source),
			_ => None,
		}
	}
	/// For `Decryption`, the decryption failure.
	pub fn decryption_error(&self) -> Option<&DecryptError> {
		match self.decision_failure() {
			Some(EncryptedRawMutationDecisionError::Decryption(source)) => Some(source),
			_ => None,
		}
	}
	/// The database error of the operation, or of its cleanup or rollback when the operation
	/// itself has none.
	pub fn database_error(&self) -> Option<&DatabaseError> {
		self.raw().and_then(PgRawError::database_error)
	}
	/// Whether the transaction's own `COMMIT` failed, which means that the operation's changes
	/// may or may not have been stored. This can be true also when the primary failure is a callback,
	/// encryption, or decryption failure.
	pub fn commit_outcome_is_unknown(&self) -> bool {
		self.raw()
			.is_some_and(PgRawError::commit_outcome_is_unknown)
	}
}
impl<C> fmt::Debug for PgEncryptedRawMutationError<C> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("PgEncryptedRawMutationError")
			.finish_non_exhaustive()
	}
}
impl<C> fmt::Display for PgEncryptedRawMutationError<C> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("the encrypted Raw mutation failed")
	}
}
impl<C: Error + 'static> Error for PgEncryptedRawMutationError<C> {
	fn source(&self) -> Option<&(dyn Error + 'static)> {
		Some(match &self.diagnostic.source {
			EncryptedRawMutationCause::Input(source) => source,
			EncryptedRawMutationCause::Raw(source) => source,
		})
	}
}
