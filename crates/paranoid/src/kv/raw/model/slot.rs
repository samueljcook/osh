//! Bounded canonical slot candidates and owned opaque input carry no lock authority.
pub(crate) use crate::kv::raw::state::{
	RawSlotSeedIntent, RawSlotSeedOverflowError, RawWriteExpirationOverflowError,
	RawWritePreparationFields, RawWriteResult, prepare_raw_slot_seed, prepare_raw_write,
};
use crate::kv::{Key, KeyError, MAX_ACQUIRE_SLOT_CANDIDATES, Ttl};
use crate::records::{
	ManyRecordRequestError, RecordKey, StateTimestampMicros, check_many_record_keys,
};
#[derive(Debug, thiserror::Error)]
/// A slot request that Paranoid rejected.
pub enum SlotRequestError {
	#[error("raw slot candidates are invalid")]
	/// The candidate keys repeat a key or exceed the largest supported number of candidates.
	Candidates(#[from] ManyRecordRequestError),
	#[error("raw slot TTL must be positive and expiring")]
	/// The TTL is not a positive expiring TTL.
	TtlMustExpire,
	#[error("raw slot source candidate is not UTF-8")]
	/// While the namespace is still in v10 storage, a candidate key is not UTF-8.
	SourceUtf8 {
		/// The candidate's position in the request.
		input_index: usize,
		#[source]
		/// The UTF-8 failure.
		source: std::str::Utf8Error,
	},
	#[error("raw slot source candidate is not canonical")]
	/// While the namespace is still in v10 storage, a candidate key is not a valid v10
	/// key.
	SourceKey {
		/// The candidate's position in the request.
		input_index: usize,
		#[source]
		/// Why the key is invalid.
		source: KeyError,
	},
}
/// The inputs of a slot's candidates and TTL.
pub struct RawSlotSelectionRequestFields {
	/// The candidate keys, without duplicates.
	pub candidates: Vec<RecordKey>,
	/// The TTL of the written record, which must be positive and expiring.
	pub ttl: Ttl,
}
/// The validated candidates and TTL of a slot request.
pub struct RawSlotSelectionRequest {
	candidates: RawSlotCandidates,
	ttl: Ttl,
}
pub(in crate::kv) struct RawSlotSelectionRequestParts {
	pub(in crate::kv) candidates: RawSlotCandidates,
	pub(in crate::kv) ttl: Ttl,
}
/// The inputs of a slot request.
pub struct RawSlotRequestFields {
	/// The candidate keys, without duplicates.
	pub candidates: Vec<RecordKey>,
	/// The bytes to write under the selected key.
	pub value: Vec<u8>,
	/// The TTL of the written record, which must be positive and expiring.
	pub ttl: Ttl,
}
/// A validated slot request.
pub struct RawSlotRequest {
	selection: RawSlotSelectionRequest,
	value: Vec<u8>,
}
pub(in crate::kv) struct RawSlotRequestParts {
	pub(in crate::kv) selection: RawSlotSelectionRequest,
	pub(in crate::kv) value: Vec<u8>,
}
pub(crate) struct RawSlotCandidates {
	keys: Vec<RecordKey>,
	input_indexes: Vec<usize>,
}
impl RawSlotSelectionRequest {
	/// Validates the candidates and the TTL without database work.
	pub fn new(fields: RawSlotSelectionRequestFields) -> Result<Self, SlotRequestError> {
		check_many_record_keys(&fields.candidates, MAX_ACQUIRE_SLOT_CANDIDATES)?;
		fields
			.ttl
			.positive_micros()
			.ok_or(SlotRequestError::TtlMustExpire)?;
		let mut indexed = fields
			.candidates
			.into_iter()
			.enumerate()
			.collect::<Vec<_>>();
		indexed.sort_by(|a, b| a.1.cmp(&b.1));
		let (input_indexes, keys) = indexed.into_iter().unzip();
		Ok(Self {
			candidates: RawSlotCandidates {
				keys,
				input_indexes,
			},
			ttl: fields.ttl,
		})
	}
	/// Whether the request has no candidates.
	pub fn is_empty(&self) -> bool {
		self.candidates.keys.is_empty()
	}
	pub(in crate::kv) fn candidates(&self) -> &RawSlotCandidates {
		&self.candidates
	}
	pub(in crate::kv) fn into_parts(self) -> RawSlotSelectionRequestParts {
		RawSlotSelectionRequestParts {
			candidates: self.candidates,
			ttl: self.ttl,
		}
	}
}
impl RawSlotRequest {
	/// Validates the request without database work.
	pub fn new(fields: RawSlotRequestFields) -> Result<Self, SlotRequestError> {
		Ok(Self {
			selection: RawSlotSelectionRequest::new(RawSlotSelectionRequestFields {
				candidates: fields.candidates,
				ttl: fields.ttl,
			})?,
			value: fields.value,
		})
	}
	/// Whether the request has no candidates.
	pub fn is_empty(&self) -> bool {
		self.selection.is_empty()
	}
	pub(in crate::kv) fn into_parts(self) -> RawSlotRequestParts {
		RawSlotRequestParts {
			selection: self.selection,
			value: self.value,
		}
	}
}
impl RawSlotCandidates {
	pub(crate) fn keys(&self) -> &[RecordKey] {
		&self.keys
	}
	pub(in crate::kv) fn keys_in_input_order(&self) -> Vec<RecordKey> {
		let mut indexed = self
			.keys
			.iter()
			.zip(&self.input_indexes)
			.collect::<Vec<_>>();
		indexed.sort_unstable_by_key(|(_, position)| **position);
		indexed.into_iter().map(|(key, _)| key.clone()).collect()
	}
	pub(crate) fn input_index(&self, key: &RecordKey) -> Option<usize> {
		self.keys
			.binary_search(key)
			.ok()
			.map(|i| self.input_indexes[i])
	}
	pub(in crate::kv) fn source_keys(&self) -> Result<RawSlotSourceKeys<'_>, SlotRequestError> {
		let mut texts = Vec::with_capacity(self.keys.len());
		for (index, key) in self.keys.iter().enumerate() {
			let input_index = self.input_indexes[index];
			let text = std::str::from_utf8(key.as_bytes()).map_err(|source| {
				SlotRequestError::SourceUtf8 {
					input_index,
					source,
				}
			})?;
			let key = Key::from_persisted_text(text.to_owned()).map_err(|source| {
				SlotRequestError::SourceKey {
					input_index,
					source,
				}
			})?;
			texts.push(key.as_str().to_owned());
		}
		Ok(RawSlotSourceKeys {
			candidates: self,
			texts,
		})
	}
}
/// Canonical source keys are derived only from the exact parsed candidate owner.
pub(crate) struct RawSlotSourceKeys<'a> {
	candidates: &'a RawSlotCandidates,
	texts: Vec<String>,
}
impl RawSlotSourceKeys<'_> {
	pub(crate) fn candidates(&self) -> &RawSlotCandidates {
		self.candidates
	}
	pub(crate) fn texts(&self) -> &[String] {
		&self.texts
	}
}
#[derive(Debug)]
/// The result of a slot request.
pub enum RawSlotResult {
	/// The request had no candidates, so Paranoid did no database work.
	Empty,
	/// Every candidate held a live record or was locked by another transaction, so Paranoid
	/// wrote nothing.
	Unavailable {
		/// The database time of the attempt.
		observed_at: StateTimestampMicros,
	},
	/// Paranoid wrote the value under one candidate key.
	Acquired {
		/// The selected key.
		key: RecordKey,
		/// The database time of the write and the stored metadata.
		outcome: RawWriteResult,
	},
}
