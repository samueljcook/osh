//! Shared finite native key-set parsing; it grants no record or transaction authority.

use std::collections::BTreeMap;

use super::RecordKey;

/// The largest number of records in one many-record read or write.
pub const MAX_MANY_RECORD_ENTRIES: usize = 10_000;

/// A many-record request that Paranoid rejected.
#[derive(Debug, thiserror::Error)]
pub enum ManyRecordRequestError {
	/// The request has more entries than its limit.
	#[error("record batch carries {actual} entries, maximum is {max}")]
	TooMany {
		/// Number of supplied entries.
		actual: usize,
		/// Maximum accepted number of entries.
		max: usize,
	},
	/// The request repeats a key.
	#[error("record batch repeats a key at input position {input_index}")]
	DuplicateKey {
		/// The repeated key.
		key: RecordKey,
		/// The position of the key's first occurrence.
		first_index: usize,
		/// The position of the key's second occurrence.
		input_index: usize,
	},
}

pub(crate) fn check_many_record_keys(
	keys: &[RecordKey],
	max: usize,
) -> Result<(), ManyRecordRequestError> {
	if keys.len() > max {
		return Err(ManyRecordRequestError::TooMany {
			actual: keys.len(),
			max,
		});
	}
	let mut positions = BTreeMap::new();
	for (input_index, key) in keys.iter().enumerate() {
		if let Some(first_index) = positions.insert(key, input_index) {
			return Err(ManyRecordRequestError::DuplicateKey {
				key: key.clone(),
				first_index,
				input_index,
			});
		}
	}
	Ok(())
}
