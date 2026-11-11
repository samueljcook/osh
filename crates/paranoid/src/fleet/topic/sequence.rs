use crate::fleet::{InvalidTopicEventSequenceSuffixError, NegativeTopicSequenceError};

pub(super) fn validate_nonnegative_topic_sequence(
	sequence: i64,
) -> Result<(), NegativeTopicSequenceError> {
	if sequence < 0 {
		return Err(NegativeTopicSequenceError);
	}
	Ok(())
}

pub fn parse_topic_sequence_key_suffix(
	key_suffix: &str,
) -> Result<i64, InvalidTopicEventSequenceSuffixError> {
	let invalid = || InvalidTopicEventSequenceSuffixError {
		key_suffix: key_suffix.to_owned(),
	};
	if key_suffix.len() != 20 || !key_suffix.bytes().all(|byte| byte.is_ascii_digit()) {
		return Err(invalid());
	}
	key_suffix.parse::<i64>().map_err(|_| invalid())
}
