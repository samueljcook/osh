use std::fmt;

/// An SSE data event. Empty data still produces an event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
	/// Event type. An empty type is interpreted as `message`.
	pub event_type: String,
	/// Text payload. CRLF and CR become LF on the wire.
	pub data: String,
	/// The event's cursor.
	///
	/// When encoding, `Some` sets the client's last event ID, an empty string
	/// resets it, and `None` leaves it unchanged. [`Parser`](super::Parser)
	/// always produces `Some` with the stream's last event ID when it dispatched
	/// the event. That ID can come from an earlier event, and it is empty when no
	/// `id` field and no initial cursor has set one.
	pub id: Option<String>,
}

/// Invalid event fields or an encoded frame exceeding its configured size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncodeError {
	/// Event types and IDs cannot contain line breaks; IDs also cannot contain NUL.
	InvalidField,
	/// The complete encoded frame exceeds the byte limit.
	FrameTooLarge,
}
impl fmt::Display for EncodeError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(match self {
			Self::InvalidField => "invalid SSE field",
			Self::FrameTooLarge => "SSE frame exceeds its size limit",
		})
	}
}
impl std::error::Error for EncodeError {}

fn append(out: &mut Vec<u8>, value: &[u8], max_size: usize) -> Result<(), EncodeError> {
	if value.len() > max_size.saturating_sub(out.len()) {
		return Err(EncodeError::FrameTooLarge);
	}
	out.extend_from_slice(value);
	Ok(())
}
fn lines(value: &str) -> impl Iterator<Item = &str> {
	enum LineState<'a> {
		Reading(&'a str),
		Finished,
	}
	let mut state = LineState::Reading(value);
	std::iter::from_fn(move || {
		let LineState::Reading(value) = state else {
			return None;
		};
		if let Some(index) = value.find(['\r', '\n']) {
			let delimiter_len = if value[index..].starts_with("\r\n") {
				2
			} else {
				1
			};
			state = LineState::Reading(&value[index + delimiter_len..]);
			Some(&value[..index])
		} else {
			state = LineState::Finished;
			Some(value)
		}
	})
}

/// Encodes an event using LF delimiters, including a final blank line.
/// Size checks precede each allocation-producing append.
pub fn encode_event(event: &Event, max_size: usize) -> Result<Vec<u8>, EncodeError> {
	if event.event_type.contains(['\r', '\n'])
		|| event
			.id
			.as_ref()
			.is_some_and(|id| id.contains(['\r', '\n', '\0']))
	{
		return Err(EncodeError::InvalidField);
	}
	let mut out = Vec::new();
	if !event.event_type.is_empty() {
		append(&mut out, b"event: ", max_size)?;
		append(&mut out, event.event_type.as_bytes(), max_size)?;
		append(&mut out, b"\n", max_size)?;
	}
	if let Some(id) = &event.id {
		append(&mut out, b"id: ", max_size)?;
		append(&mut out, id.as_bytes(), max_size)?;
		append(&mut out, b"\n", max_size)?;
	}
	for line in lines(&event.data) {
		append(&mut out, b"data: ", max_size)?;
		append(&mut out, line.as_bytes(), max_size)?;
		append(&mut out, b"\n", max_size)?;
	}
	append(&mut out, b"\n", max_size)?;
	Ok(out)
}

/// Encodes a comment, including multiline comments and a final blank line.
pub fn encode_comment(comment: &str, max_size: usize) -> Result<Vec<u8>, EncodeError> {
	let mut out = Vec::new();
	for line in lines(comment) {
		append(&mut out, b":", max_size)?;
		append(&mut out, line.as_bytes(), max_size)?;
		append(&mut out, b"\n", max_size)?;
	}
	append(&mut out, b"\n", max_size)?;
	Ok(out)
}
