//! Fixed published-key encryption consumes the final selected raw slot without exposing its lock.
use primitives_rs::crypto::{encryption::EncryptError, keysets::Keyset};
use std::{error::Error, fmt};

use super::encryption::{
	PublishedItemEncryption, PublishedItemEncryptionFields, PublishedItemKeyError,
	prepare_published_item_key,
};
use super::slot::{
	RawSlotResult, RawSlotSelectionRequest, RawWriteExpirationOverflowError,
	RawWritePreparationFields, prepare_raw_write,
};
use crate::kv::raw::engine::slot::RawSlotFailure;
use crate::kv::raw::postgres::slot::{
	PgRawSlotSelected, RawRejectionSettlement, RawSlotStorageError,
};
use crate::kv::{Key, KeyError, Ttl};
use crate::records::{RecordIdentity, RecordKey};
use crate::scope::ScopeIdentity;

/// The associated data format that an encrypted slot value uses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RawSlotAssociatedData {
	/// `PublishedItemV1` selects the associated data format of
	/// [`PgEncryptedRaw`](crate::kv::PgEncryptedRaw), which contains the domain header
	/// `paranoid.kv.raw.v1\0`, the handle's Raw domain (either the core Raw namespace or the
	/// declared [`ScopeIdentity`]), and the length-prefixed selected key, which must be a valid
	/// v10 key.
	PublishedItemV1,
}
/// The inputs of an encrypted slot request.
pub struct EncryptedRawSlotRequestFields<'a> {
	/// The candidate keys and the TTL.
	pub selection: RawSlotSelectionRequest,
	/// The keyset that encrypts the value.
	pub keyset: &'a Keyset,
	/// The value to encrypt.
	pub plaintext: &'a [u8],
	/// The associated data that the encrypted value is bound to.
	pub associated_data: RawSlotAssociatedData,
}
/// A validated encrypted slot request. Paranoid encrypts the value once, for the selected key.
pub struct EncryptedRawSlotRequest<'a> {
	selection: RawSlotSelectionRequest,
	preparation: PublishedItemSlotPreparation<'a>,
}
#[derive(Debug, thiserror::Error)]
/// A candidate key that the requested associated data cannot use.
pub enum EncryptedRawSlotInputError {
	#[error("published Item associated data requires a UTF-8 candidate")]
	/// The candidate key is not UTF-8.
	Utf8 {
		/// The candidate key.
		key: RecordKey,
		/// The candidate's position in the request.
		input_index: usize,
		#[source]
		/// The UTF-8 failure.
		source: std::str::Utf8Error,
	},
	#[error("published Item associated data requires a canonical complete Key")]
	/// The candidate key is not a valid v10 key.
	Key {
		/// The candidate key.
		key: RecordKey,
		/// The candidate's position in the request.
		input_index: usize,
		#[source]
		/// Why the key is invalid.
		source: KeyError,
	},
}
impl EncryptedRawSlotInputError {
	/// The candidate key.
	pub fn key(&self) -> &RecordKey {
		match self {
			Self::Utf8 { key, .. } | Self::Key { key, .. } => key,
		}
	}
	/// The candidate's position in the request.
	pub fn input_index(&self) -> usize {
		match self {
			Self::Utf8 { input_index, .. } | Self::Key { input_index, .. } => *input_index,
		}
	}
}
impl<'a> EncryptedRawSlotRequest<'a> {
	/// Validates every candidate key for the associated data without database work or
	/// encryption.
	pub fn new(
		fields: EncryptedRawSlotRequestFields<'a>,
	) -> Result<Self, EncryptedRawSlotInputError> {
		let keys = match fields.associated_data {
			RawSlotAssociatedData::PublishedItemV1 => {
				let candidates = fields.selection.candidates();
				let mut keys = Vec::with_capacity(candidates.keys().len());
				for key in candidates.keys() {
					let input_index = candidates
						.input_index(key)
						.expect("canonical candidate retains its input position");
					keys.push(
						prepare_published_item_key(key).map_err(|source| match source {
							PublishedItemKeyError::Utf8(source) => {
								EncryptedRawSlotInputError::Utf8 {
									key: key.clone(),
									input_index,
									source,
								}
							}
							PublishedItemKeyError::Key(source) => EncryptedRawSlotInputError::Key {
								key: key.clone(),
								input_index,
								source,
							},
						})?,
					);
				}
				keys
			}
		};
		Ok(Self {
			selection: fields.selection,
			preparation: PublishedItemSlotPreparation {
				keyset: fields.keyset,
				plaintext: fields.plaintext,
				keys,
			},
		})
	}
	pub(in crate::kv) fn into_parts(self) -> (RawSlotSelectionRequest, SlotPayload<'a>) {
		(self.selection, SlotPayload::PublishedItem(self.preparation))
	}
}
/// Only these actual library preparations enter the admitted runner.
pub(in crate::kv) enum SlotPayload<'a> {
	Bytes(Vec<u8>),
	PublishedItem(PublishedItemSlotPreparation<'a>),
}
pub(in crate::kv) struct PublishedItemSlotPreparation<'a> {
	keyset: &'a Keyset,
	plaintext: &'a [u8],
	keys: Vec<Key>,
}
#[derive(Debug, thiserror::Error)]
pub(in crate::kv) enum RawSlotPreparationError {
	#[error("the selected raw slot does not belong to its parsed encrypted candidates")]
	SelectedCandidateMismatch,
	#[error("the selected raw slot expiration is unrepresentable")]
	Expiration(#[source] RawWriteExpirationOverflowError),
	#[error("the selected raw slot could not be encrypted")]
	Encryption(#[source] EncryptError),
}
/// A rejected preparation retains its own cause independently of actual rejection cleanup.
pub(in crate::kv) struct RawSlotPreparationRejectionError {
	identity: RecordIdentity,
	input_index: Option<usize>,
	preparation_error: RawSlotPreparationError,
	rejection: Result<RawRejectionSettlement, RawSlotStorageError>,
}
impl RawSlotPreparationRejectionError {
	pub(crate) fn identity(&self) -> &RecordIdentity {
		&self.identity
	}
	pub(crate) fn input_index(&self) -> Option<usize> {
		self.input_index
	}
	pub(crate) fn preparation_error(&self) -> &RawSlotPreparationError {
		&self.preparation_error
	}
	pub(crate) fn settlement(&self) -> Option<RawRejectionSettlement> {
		self.rejection.as_ref().ok().copied()
	}
	pub(crate) fn cleanup_error(&self) -> Option<&RawSlotStorageError> {
		self.rejection.as_ref().err()
	}
}
impl fmt::Debug for RawSlotPreparationRejectionError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("RawSlotPreparationRejectionError")
			.finish_non_exhaustive()
	}
}
impl fmt::Display for RawSlotPreparationRejectionError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("preparing the value for the selected slot key failed")
	}
}
impl Error for RawSlotPreparationRejectionError {
	fn source(&self) -> Option<&(dyn Error + 'static)> {
		Some(&self.preparation_error)
	}
}
pub(in crate::kv) struct EncryptedSlotFinishFields<'a, 'tx, 'input> {
	pub(in crate::kv) selected: PgRawSlotSelected<'a, 'tx>,
	pub(in crate::kv) scope: Option<&'a ScopeIdentity>,
	pub(in crate::kv) ttl: Ttl,
	pub(in crate::kv) preparation: PublishedItemSlotPreparation<'input>,
	pub(in crate::kv) input_index: Option<usize>,
}
pub(in crate::kv) async fn finish_encrypted(
	fields: EncryptedSlotFinishFields<'_, '_, '_>,
) -> Result<RawSlotResult, RawSlotFailure> {
	let EncryptedSlotFinishFields {
		selected,
		scope,
		ttl,
		preparation,
		input_index,
	} = fields;
	let identity = selected.identity();
	let prepared = (|| {
		let position = preparation
			.keys
			.binary_search_by(|key| key.as_str().as_bytes().cmp(identity.key().as_bytes()))
			.map_err(|_| RawSlotPreparationError::SelectedCandidateMismatch)?;
		if input_index.is_none() {
			return Err(RawSlotPreparationError::SelectedCandidateMismatch);
		}
		let outcome = prepare_raw_write(RawWritePreparationFields {
			observed_at: selected.observed_at(),
			ttl,
		})
		.map_err(RawSlotPreparationError::Expiration)?;
		let body = PublishedItemEncryption::new(PublishedItemEncryptionFields {
			keyset: preparation.keyset,
			scope,
			key: &preparation.keys[position],
		})
		.encrypt(preparation.plaintext)
		.map_err(RawSlotPreparationError::Encryption)?;
		Ok((outcome, body))
	})();
	let (outcome, body) = match prepared {
		Ok(prepared) => prepared,
		Err(preparation_error) => {
			let rejection = selected.reject().await;
			return Err(RawSlotFailure::Preparation(Box::new(
				RawSlotPreparationRejectionError {
					identity,
					input_index,
					preparation_error,
					rejection,
				},
			)));
		}
	};
	selected.apply(outcome.encode(body)).await?;
	Ok(RawSlotResult::Acquired {
		key: identity.key().clone(),
		outcome,
	})
}
