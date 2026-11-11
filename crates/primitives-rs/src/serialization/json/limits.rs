use super::{JsonError, JsonLimits};
use std::io::{self, Write};

enum StringPosition {
	Outside,
	Inside,
	Escaped,
}

pub(super) struct DepthScanner {
	max_depth: usize,
	open_containers: usize,
	string_position: StringPosition,
}
impl DepthScanner {
	pub(super) fn new(max_depth: usize) -> Self {
		Self {
			max_depth,
			open_containers: 0,
			string_position: StringPosition::Outside,
		}
	}
	pub(super) fn check(&mut self, bytes: &[u8]) -> Result<(), JsonError> {
		for &byte in bytes {
			match (&self.string_position, byte) {
				(StringPosition::Escaped, _) => self.string_position = StringPosition::Inside,
				(StringPosition::Inside, b'\\') => self.string_position = StringPosition::Escaped,
				(StringPosition::Inside, b'"') => self.string_position = StringPosition::Outside,
				(StringPosition::Inside, _) => {}
				(StringPosition::Outside, b'"') => self.string_position = StringPosition::Inside,
				(StringPosition::Outside, b'[' | b'{') => {
					if self.open_containers == self.max_depth {
						return Err(JsonError::TooDeep {
							max: self.max_depth,
						});
					}
					self.open_containers += 1;
				}
				(StringPosition::Outside, b']' | b'}') => {
					self.open_containers = self.open_containers.saturating_sub(1)
				}
				(StringPosition::Outside, _) => {}
			}
		}
		Ok(())
	}
}

struct WriterProgress {
	written: usize,
	depth: DepthScanner,
}

enum WriterState<'a, W: ?Sized> {
	Writing {
		writer: &'a mut W,
		progress: WriterProgress,
	},
	OutputUnconfirmed,
	Failed(JsonError),
}

pub(super) struct LimitedJsonWriter<'a, W: ?Sized> {
	limits: JsonLimits,
	state: WriterState<'a, W>,
}
impl<'a, W: Write + ?Sized> LimitedJsonWriter<'a, W> {
	pub(super) fn new(writer: &'a mut W, limits: JsonLimits) -> Self {
		Self {
			limits,
			state: WriterState::Writing {
				writer,
				progress: WriterProgress {
					written: 0,
					depth: DepthScanner::new(limits.max_depth),
				},
			},
		}
	}
	pub(super) fn finish(self, result: Result<(), serde_json::Error>) -> Result<(), JsonError> {
		match self.state {
			WriterState::Writing { .. } => result.map_err(JsonError::Serialize),
			WriterState::OutputUnconfirmed => Err(JsonError::Io(io::Error::other(
				"JSON output was interrupted",
			))),
			WriterState::Failed(error) => Err(error),
		}
	}
}
impl<W: Write + ?Sized> Write for LimitedJsonWriter<'_, W> {
	fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
		let (writer, mut progress) =
			match std::mem::replace(&mut self.state, WriterState::OutputUnconfirmed) {
				WriterState::Writing { writer, progress } => (writer, progress),
				state => {
					self.state = state;
					return Err(io::Error::other("JSON output previously failed"));
				}
			};
		let result = (|| {
			let next_size = progress
				.written
				.checked_add(bytes.len())
				.filter(|size| *size <= self.limits.max_size)
				.ok_or(JsonError::TooLarge {
					actual_minimum: progress.written.saturating_add(bytes.len()),
					max: self.limits.max_size,
				})?;
			progress.depth.check(bytes)?;
			writer.write_all(bytes).map_err(JsonError::Io)?;
			progress.written = next_size;
			Ok(bytes.len())
		})();
		match result {
			Ok(count) => {
				self.state = WriterState::Writing { writer, progress };
				Ok(count)
			}
			Err(error) => {
				self.state = WriterState::Failed(error);
				Err(io::Error::other("JSON output failed"))
			}
		}
	}
	fn flush(&mut self) -> io::Result<()> {
		let (writer, progress) =
			match std::mem::replace(&mut self.state, WriterState::OutputUnconfirmed) {
				WriterState::Writing { writer, progress } => (writer, progress),
				state => {
					self.state = state;
					return Err(io::Error::other("JSON output previously failed"));
				}
			};
		match writer.flush() {
			Ok(()) => {
				self.state = WriterState::Writing { writer, progress };
				Ok(())
			}
			Err(error) => {
				self.state = WriterState::Failed(JsonError::Io(error));
				Err(io::Error::other("JSON output failed"))
			}
		}
	}
}
