//! Plaintext decisions borrow zeroizing owners while the actual Raw engine owns mutation.

use primitives_rs::secret_bytes::SecretBytes;
use std::fmt;

use crate::kv::{AtomicMutationOutcome, RawLiveOrInitValue, StateTimestampMicros, Ttl};

/// What an encrypted mutation does with the record.
///
/// Paranoid moves a replacement value into a buffer that it zeroes, and then encrypts the
/// value. Copies that the app makes are not zeroed.
pub enum EncryptedRawMutation {
	/// Keep the record as it is. After `mutate_live_or_init` initialized the value, Paranoid
	/// stores the initial value with the initial TTL.
	KeepExisting,
	/// Encrypt and store a new value with a TTL.
	SetBytes {
		/// The new plaintext value.
		value: Vec<u8>,
		/// The TTL, counted from the database time of the write.
		ttl: Ttl,
	},
	/// Encrypt and store a new value and keep the record's current expiry. After
	/// `mutate_live_or_init` initialized the value, Paranoid uses the initial TTL. When the
	/// record is absent or expired, `mutate` fails with the kind `NotLive`.
	SetBytesPreservingExpiration {
		/// The new plaintext value.
		value: Vec<u8>,
	},
	/// Delete the record, including an expired stored value.
	Delete,
}
impl fmt::Debug for EncryptedRawMutation {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		let name = match self {
			Self::KeepExisting => "KeepExisting",
			Self::SetBytes { .. } => "SetBytes",
			Self::SetBytesPreservingExpiration { .. } => "SetBytesPreservingExpiration",
			Self::Delete => "Delete",
		};
		f.debug_struct(name).finish_non_exhaustive()
	}
}

/// What a decision of `mutate` receives: the decrypted live value, if any, and the database
/// time after Paranoid locked the record.
pub struct DecryptedRawMutationCurrent<'a> {
	live_value: Option<&'a SecretBytes>,
	observed_at: StateTimestampMicros,
}
impl<'a> DecryptedRawMutationCurrent<'a> {
	pub(crate) fn new(
		live_value: Option<&'a SecretBytes>,
		observed_at: StateTimestampMicros,
	) -> Self {
		Self {
			live_value,
			observed_at,
		}
	}
	/// The decrypted live value, or `None` when the record is absent or expired.
	pub fn live_value(&self) -> Option<&'a SecretBytes> {
		self.live_value
	}
	/// The database time after Paranoid locked the record.
	pub fn observed_at(&self) -> StateTimestampMicros {
		self.observed_at
	}
}
impl fmt::Debug for DecryptedRawMutationCurrent<'_> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("DecryptedRawMutationCurrent")
			.finish_non_exhaustive()
	}
}

/// What a decision of `mutate_live` or `mutate_live_or_init` receives: the decrypted live value
/// or the initialized value, and the database time after Paranoid locked the record. An
/// initialized value is not yet stored.
pub struct DecryptedRawLiveMutationCurrent<'a> {
	live_value: &'a SecretBytes,
	observed_at: StateTimestampMicros,
}
impl<'a> DecryptedRawLiveMutationCurrent<'a> {
	pub(crate) fn new(live_value: &'a SecretBytes, observed_at: StateTimestampMicros) -> Self {
		Self {
			live_value,
			observed_at,
		}
	}
	/// The decrypted live value or the initialized value.
	pub fn live_value(&self) -> &'a SecretBytes {
		self.live_value
	}
	/// The database time after Paranoid locked the record.
	pub fn observed_at(&self) -> StateTimestampMicros {
		self.observed_at
	}
}
impl fmt::Debug for DecryptedRawLiveMutationCurrent<'_> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("DecryptedRawLiveMutationCurrent")
			.finish_non_exhaustive()
	}
}

/// The result of an encrypted `mutate` or `mutate_live`. A change inside the caller's
/// transaction becomes durable only when the caller commits.
pub struct DecryptedRawMutationResult {
	previous_live_value: Option<SecretBytes>,
	observed_at: StateTimestampMicros,
	outcome: AtomicMutationOutcome,
}
impl DecryptedRawMutationResult {
	pub(crate) fn new(
		previous_live_value: Option<SecretBytes>,
		observed_at: StateTimestampMicros,
		outcome: AtomicMutationOutcome,
	) -> Self {
		Self {
			previous_live_value,
			observed_at,
			outcome,
		}
	}
	/// The decrypted value that the decision received, or `None` when the record was absent or
	/// expired. After a successful `mutate_live`, the value is present.
	pub fn previous_live_value(&self) -> Option<&SecretBytes> {
		self.previous_live_value.as_ref()
	}
	/// The database time at which the decision ran.
	pub fn observed_at(&self) -> StateTimestampMicros {
		self.observed_at
	}
	/// The change that the decision made.
	pub fn outcome(&self) -> &AtomicMutationOutcome {
		&self.outcome
	}
	/// The previous value, the database time, and the change.
	pub fn into_parts(
		self,
	) -> (
		Option<SecretBytes>,
		StateTimestampMicros,
		AtomicMutationOutcome,
	) {
		(self.previous_live_value, self.observed_at, self.outcome)
	}
}
impl fmt::Debug for DecryptedRawMutationResult {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("DecryptedRawMutationResult")
			.finish_non_exhaustive()
	}
}

/// The result of an encrypted `mutate_live_or_init`. A change inside the caller's transaction
/// becomes durable only when the caller commits.
pub struct DecryptedRawLiveOrInitMutationResult {
	value_seen: RawLiveOrInitValue<SecretBytes>,
	observed_at: StateTimestampMicros,
	outcome: AtomicMutationOutcome,
}
impl DecryptedRawLiveOrInitMutationResult {
	pub(crate) fn new(
		value_seen: RawLiveOrInitValue<SecretBytes>,
		observed_at: StateTimestampMicros,
		outcome: AtomicMutationOutcome,
	) -> Self {
		Self {
			value_seen,
			observed_at,
			outcome,
		}
	}
	/// The value that the decision received: the decrypted live value or the initialized value.
	pub fn value_seen(&self) -> &RawLiveOrInitValue<SecretBytes> {
		&self.value_seen
	}
	/// The database time at which the initializer, if it ran, and the decision ran.
	pub fn observed_at(&self) -> StateTimestampMicros {
		self.observed_at
	}
	/// The change that the decision made.
	pub fn outcome(&self) -> &AtomicMutationOutcome {
		&self.outcome
	}
	/// The value, the database time, and the change.
	pub fn into_parts(
		self,
	) -> (
		RawLiveOrInitValue<SecretBytes>,
		StateTimestampMicros,
		AtomicMutationOutcome,
	) {
		(self.value_seen, self.observed_at, self.outcome)
	}
}
impl fmt::Debug for DecryptedRawLiveOrInitMutationResult {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("DecryptedRawLiveOrInitMutationResult")
			.finish_non_exhaustive()
	}
}
