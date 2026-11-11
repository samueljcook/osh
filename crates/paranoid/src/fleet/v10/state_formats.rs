//! Decoders for the published Postcard state formats of v10 Fleet families.

use serde::{Deserialize, Serialize};

use super::untagged_postcard::{
	decode_untagged_cache_epoch, decode_untagged_counter, decode_untagged_once_completion,
	decode_untagged_subscription_cursor, decode_untagged_topic_sequence,
};
use crate::fleet::HolderId;
use crate::fleet::scalar_codec::{RepresentationDecodeError, ValueError};

/// Durable completion marker for a v10 Fleet run-once task.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(in crate::fleet::v10) struct OnceCompletion {
	pub(in crate::fleet::v10) finished_at_unix_micros: i64,
	pub(in crate::fleet::v10) holder_id: String,
	pub(in crate::fleet::v10) fencing_token: i64,
}

fn reject_state(reason: impl Into<String>) -> ValueError {
	ValueError::Representation {
		reason: reason.into(),
	}
}

fn untagged_failure(source: impl std::error::Error + Send + Sync + 'static) -> ValueError {
	ValueError::RepresentationDecode(RepresentationDecodeError::new(source))
}

fn require_nonnegative(value: i64, what: &str) -> Result<i64, ValueError> {
	if value < 0 {
		return Err(reject_state(format!("stored {what} is below its minimum")));
	}
	Ok(value)
}

fn require_valid_completion(completion: &OnceCompletion) -> Result<(), ValueError> {
	if HolderId::new(&completion.holder_id).is_err() {
		return Err(reject_state("stored completion holder is invalid"));
	}
	if completion.fencing_token <= 0 {
		return Err(reject_state("stored completion fencing token is invalid"));
	}
	Ok(())
}

pub(in crate::fleet) fn decode_source_counter(bytes: &[u8]) -> Result<i64, ValueError> {
	decode_untagged_counter(bytes).map_err(untagged_failure)
}

pub(super) fn decode_source_cache_epoch(bytes: &[u8]) -> Result<i64, ValueError> {
	decode_untagged_cache_epoch(bytes).map_err(untagged_failure)
}

pub(super) fn decode_source_topic_sequence(bytes: &[u8]) -> Result<i64, ValueError> {
	let value = decode_untagged_topic_sequence(bytes).map_err(untagged_failure)?;
	require_nonnegative(value, "topic sequence")
}

pub(super) fn decode_source_subscription_cursor(bytes: &[u8]) -> Result<i64, ValueError> {
	let value = decode_untagged_subscription_cursor(bytes).map_err(untagged_failure)?;
	require_nonnegative(value, "topic cursor")
}

pub(super) fn decode_source_once_completion(bytes: &[u8]) -> Result<OnceCompletion, ValueError> {
	let completion = decode_untagged_once_completion(bytes).map_err(untagged_failure)?;
	require_valid_completion(&completion)?;
	Ok(completion)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn source_counters_accept_raw_postcard_and_reject_truncated_varints() {
		for value in [0_i64, -5, 42, i64::MIN, i64::MAX] {
			let encoded = postcard::to_allocvec(&value).expect("encode postcard counter");
			assert_eq!(
				decode_source_counter(&encoded).expect("decode postcard counter"),
				value
			);
		}
		assert!(decode_source_counter(&[]).is_err());
		assert!(decode_source_counter(&[0x80]).is_err());
		assert!(decode_source_counter(&[0xff, 0xfe, 0xfd]).is_err());
	}

	#[test]
	fn source_topic_sequences_and_cursors_reject_negative_values() {
		let valid = postcard::to_allocvec(&7_i64).expect("encode nonnegative integer");
		assert_eq!(
			decode_source_topic_sequence(&valid).expect("decode sequence"),
			7
		);
		assert_eq!(
			decode_source_subscription_cursor(&valid).expect("decode cursor"),
			7
		);

		let negative = postcard::to_allocvec(&-1_i64).expect("encode negative integer");
		assert!(decode_source_topic_sequence(&negative).is_err());
		assert!(decode_source_subscription_cursor(&negative).is_err());
	}

	#[test]
	fn source_once_completions_validate_holder_and_fencing_token() {
		let valid = OnceCompletion {
			finished_at_unix_micros: 1_000,
			holder_id: "holder.1".to_owned(),
			fencing_token: 1,
		};
		let bytes = postcard::to_allocvec(&valid).expect("encode completion");
		let decoded = decode_source_once_completion(&bytes).expect("decode completion");
		assert_eq!(decoded, valid);

		let bad_fence = OnceCompletion {
			finished_at_unix_micros: 1_000,
			holder_id: "holder.1".to_owned(),
			fencing_token: 0,
		};
		assert!(
			decode_source_once_completion(
				&postcard::to_allocvec(&bad_fence).expect("encode invalid completion")
			)
			.is_err()
		);
	}
}
