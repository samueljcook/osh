use super::Event;
use std::{fmt, num::NonZeroUsize};

/// A complete SSE frame, including comments and cursor-only frames.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
	/// An event exists only when at least one `data` field was present.
	pub event: Option<Event>,
}
/// An input frame exceeded its configured size. Parsing cannot resume afterward.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParseError;
impl fmt::Display for ParseError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("SSE frame exceeds its size limit")
	}
}
impl std::error::Error for ParseError {}

/// An initial cursor contains NUL or a line break, or exceeds the decoded frame bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InitialEventIdError;
impl fmt::Display for InitialEventIdError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("invalid initial SSE event ID")
	}
}
impl std::error::Error for InitialEventIdError {}

/// An incremental WHATWG SSE parser with bounded frame storage.
///
/// The frame limit counts input bytes, with one byte for each line break (CRLF
/// counts as one). It applies to the current line together with the lines that
/// supply fields retained in the current frame: every `data` line, the last `event` line,
/// and the last `id` line without NUL since the previous blank line, each with
/// its line break. A comment line or another field stops counting when its line
/// ends, so comments without blank lines do not accumulate toward the limit.
/// Parsing fails when one more input byte, including a line break, would exceed
/// the limit. UTF-8 replacement can use at most three output bytes per input
/// byte. Retained cursor state is also bounded by this limit. EOF discards an
/// incomplete frame. Create a new parser for each connection, restoring the
/// committed cursor when resuming a prior connection.
pub struct Parser {
	max_size: usize,
	state: ParserState,
	last_event_id: String,
	retry_millis: Option<u64>,
}

enum ParserState {
	Reading(FrameBuffer),
	Failed,
}

struct FrameBuffer {
	/// The input bytes of this frame's `data` lines, including line breaks.
	data_size: usize,
	/// The input bytes of this frame's last `event` line, including its line break.
	event_type_size: usize,
	/// The input bytes of this frame's last accepted `id` line, including its line break.
	id_size: usize,
	line: Vec<u8>,
	data: String,
	event_type: String,
	id_buffer: String,
	line_position: LinePosition,
}

enum LinePosition {
	InitialLine,
	SubsequentLine,
	AfterCarriageReturn,
}
impl Parser {
	/// Creates a parser with an explicit frame bound and no cursor.
	pub fn new(max_frame_size: NonZeroUsize) -> Self {
		Self {
			max_size: max_frame_size.get(),
			state: ParserState::Reading(FrameBuffer {
				data_size: 0,
				event_type_size: 0,
				id_size: 0,
				line: Vec::new(),
				data: String::new(),
				event_type: String::new(),
				id_buffer: String::new(),
				line_position: LinePosition::InitialLine,
			}),
			last_event_id: String::new(),
			retry_millis: None,
		}
	}
	/// Creates a parser for a resumed connection with its previously committed cursor.
	/// The cursor must contain no NUL or line break and at most three times the
	/// frame limit in UTF-8 bytes, which bounds replacement-decoded cursor storage.
	/// An empty cursor starts without an ID; a later empty `id` field resets it.
	pub fn new_with_event_id(
		max_frame_size: NonZeroUsize,
		initial_event_id: &str,
	) -> Result<Self, InitialEventIdError> {
		if initial_event_id.contains(['\0', '\r', '\n'])
			|| initial_event_id.len() > max_frame_size.get().saturating_mul(3)
		{
			return Err(InitialEventIdError);
		}
		let mut parser = Self::new(max_frame_size);
		let ParserState::Reading(frame) = &mut parser.state else {
			unreachable!("a new parser is reading");
		};
		frame.id_buffer.push_str(initial_event_id);
		parser.last_event_id.push_str(initial_event_id);
		Ok(parser)
	}
	/// Returns the last cursor committed by a blank line, including an empty reset.
	pub fn last_event_id(&self) -> &str {
		&self.last_event_id
	}
	/// Returns the most recent valid retry delay. Overflowing numbers are ignored.
	pub fn retry_millis(&self) -> Option<u64> {
		self.retry_millis
	}
	/// Consumes bytes until a complete frame or the end of this slice.
	/// Unconsumed bytes remain in `input`, so callers can apply backpressure
	/// between frames without allocating a queue.
	pub fn next_frame(&mut self, input: &mut &[u8]) -> Result<Option<Frame>, ParseError> {
		let ParserState::Reading(frame) = &mut self.state else {
			return Err(ParseError);
		};
		let result = frame.next_frame(
			input,
			self.max_size,
			&mut self.last_event_id,
			&mut self.retry_millis,
		);
		if result.is_err() {
			self.state = ParserState::Failed;
		}
		result
	}
}

impl FrameBuffer {
	fn next_frame(
		&mut self,
		input: &mut &[u8],
		max_size: usize,
		last_event_id: &mut String,
		retry_millis: &mut Option<u64>,
	) -> Result<Option<Frame>, ParseError> {
		while let Some((&byte, rest)) = input.split_first() {
			*input = rest;
			if matches!(self.line_position, LinePosition::AfterCarriageReturn) {
				self.line_position = LinePosition::SubsequentLine;
				if byte == b'\n' {
					continue;
				}
			}
			if self.size() == max_size {
				return Err(ParseError);
			}
			if byte == b'\r' || byte == b'\n' {
				let initial_line = matches!(self.line_position, LinePosition::InitialLine);
				self.line_position = if byte == b'\r' {
					LinePosition::AfterCarriageReturn
				} else {
					LinePosition::SubsequentLine
				};
				if let Some(frame) = self.finish_line(initial_line, last_event_id, retry_millis) {
					return Ok(Some(frame));
				}
			} else {
				self.line.push(byte);
			}
		}
		Ok(None)
	}
	fn finish_line(
		&mut self,
		initial_line: bool,
		last_event_id: &mut String,
		retry_millis: &mut Option<u64>,
	) -> Option<Frame> {
		let bytes = std::mem::take(&mut self.line);
		let line_size = bytes.len() + 1;
		let mut line = String::from_utf8_lossy(&bytes);
		if initial_line && line.starts_with('\u{feff}') {
			line = std::borrow::Cow::Owned(line['\u{feff}'.len_utf8()..].to_owned());
		}
		if line.is_empty() {
			self.data_size = 0;
			self.event_type_size = 0;
			self.id_size = 0;
			last_event_id.clone_from(&self.id_buffer);
			let event = if self.data.is_empty() {
				None
			} else {
				self.data.pop();
				Some(Event {
					event_type: if self.event_type.is_empty() {
						"message".into()
					} else {
						std::mem::take(&mut self.event_type)
					},
					data: std::mem::take(&mut self.data),
					id: Some(last_event_id.clone()),
				})
			};
			self.event_type.clear();
			return Some(Frame { event });
		}
		if !line.starts_with(':') {
			let (field, value) = line
				.split_once(':')
				.map_or((line.as_ref(), ""), |(field, value)| {
					(field, value.strip_prefix(' ').unwrap_or(value))
				});
			match field {
				"data" => {
					self.data.push_str(value);
					self.data.push('\n');
					self.data_size += line_size;
				}
				"event" => {
					self.event_type.clear();
					self.event_type.push_str(value);
					self.event_type_size = line_size;
				}
				"id" if !value.contains('\0') => {
					self.id_buffer.clear();
					self.id_buffer.push_str(value);
					self.id_size = line_size;
				}
				"retry" if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) => {
					if let Ok(delay) = value.parse() {
						*retry_millis = Some(delay);
					}
				}
				_ => {}
			}
		}
		None
	}
	/// The input bytes that count toward the frame limit: the retained lines
	/// and the current line without its line break.
	fn size(&self) -> usize {
		self.data_size + self.event_type_size + self.id_size + self.line.len()
	}
}
