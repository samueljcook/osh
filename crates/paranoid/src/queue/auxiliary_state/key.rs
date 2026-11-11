//! Strict inverse parsing of the actual versioned auxiliary logical keys.

use super::{
	AuxiliaryStateError, DedupeKey, QueuePauseScope, RecordKey, TaskName, active_dedupe, invalid,
};

pub(in crate::queue) fn parse_active_dedupe_key(
	key: &RecordKey,
) -> Result<(TaskName, DedupeKey), AuxiliaryStateError> {
	let mut bytes = key.as_bytes();
	if bytes.first() != Some(&1) {
		return Err(invalid("active dedupe key version is invalid"));
	}
	bytes = &bytes[1..];
	let task = take_frame(&mut bytes)?;
	let dedupe = take_frame(&mut bytes)?;
	if !bytes.is_empty() {
		return Err(invalid("active dedupe key has trailing bytes"));
	}
	let task = TaskName::new(
		std::str::from_utf8(task)
			.map_err(|_| invalid("active dedupe task is not UTF8"))?
			.to_owned(),
	)
	.map_err(|_| invalid("active dedupe task is invalid"))?;
	let dedupe = DedupeKey::new(
		std::str::from_utf8(dedupe)
			.map_err(|_| invalid("active dedupe value is not UTF8"))?
			.to_owned(),
	)
	.map_err(|_| invalid("active dedupe value is invalid"))?;
	if active_dedupe::key(&task, &dedupe)? != *key {
		return Err(invalid("active dedupe key is not canonical"));
	}
	Ok((task, dedupe))
}
fn take_frame<'a>(bytes: &mut &'a [u8]) -> Result<&'a [u8], AuxiliaryStateError> {
	let (length, rest) = bytes
		.split_first_chunk::<4>()
		.ok_or_else(|| invalid("active dedupe key frame is incomplete"))?;
	let length = usize::try_from(u32::from_be_bytes(*length))
		.map_err(|_| invalid("active dedupe frame exceeds the platform domain"))?;
	let (value, rest) = rest
		.split_at_checked(length)
		.ok_or_else(|| invalid("active dedupe key frame is incomplete"))?;
	*bytes = rest;
	Ok(value)
}
pub(in crate::queue) fn parse_pause_key(
	key: &RecordKey,
) -> Result<QueuePauseScope, AuxiliaryStateError> {
	let scope = match key.as_bytes() {
		[1, 0] => QueuePauseScope::Global,
		[1, 1, task @ ..] => QueuePauseScope::Task(
			TaskName::new(
				std::str::from_utf8(task)
					.map_err(|_| invalid("pause task is not UTF8"))?
					.to_owned(),
			)
			.map_err(|_| invalid("pause task is invalid"))?,
		),
		_ => return Err(invalid("pause key is invalid")),
	};
	if scope.key() != *key {
		return Err(invalid("pause key is not canonical"));
	}
	Ok(scope)
}
