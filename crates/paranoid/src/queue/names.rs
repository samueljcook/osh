//! Parsed names of Queue: task names, dedupe keys, and worker names.
//!
//! Each constructor checks its text once, so a value of the type is valid wherever it goes.

use super::{
	DedupeKeyError, MAX_DEDUPE_KEY_LEN, MAX_QUEUE_WORKER_NAME_LEN, MAX_TASK_NAME_LEN,
	MAX_WORKER_OWNER_ID_LEN, TaskNameError, WorkerNameError, WorkerOwnerIdError, fmt,
};
use std::borrow::Cow;

const fn is_task_name_first_byte(byte: u8) -> bool {
	byte == b'_' || byte.is_ascii_alphanumeric()
}

const fn is_task_name_trailing_byte(byte: u8) -> bool {
	is_task_name_first_byte(byte) || byte == b'.' || byte == b'-'
}

const fn check_task_name(text: &str) -> Result<(), TaskNameError> {
	let bytes = text.as_bytes();
	if bytes.is_empty() {
		return Err(TaskNameError::Empty);
	}
	if bytes.len() > MAX_TASK_NAME_LEN {
		return Err(TaskNameError::TooLong {
			actual: bytes.len(),
			max: MAX_TASK_NAME_LEN,
		});
	}
	if !is_task_name_first_byte(bytes[0]) {
		return Err(TaskNameError::InvalidByte);
	}
	let mut index = 1;
	while index < bytes.len() {
		if !is_task_name_trailing_byte(bytes[index]) {
			return Err(TaskNameError::InvalidByte);
		}
		index += 1;
	}
	Ok(())
}

const fn check_worker_name(text: &str) -> Result<(), WorkerNameError> {
	let bytes = text.as_bytes();
	if bytes.is_empty() {
		return Err(WorkerNameError::Empty);
	}
	if bytes.len() > MAX_QUEUE_WORKER_NAME_LEN {
		return Err(WorkerNameError::TooLong {
			actual: bytes.len(),
			max: MAX_QUEUE_WORKER_NAME_LEN,
		});
	}
	let mut index = 0;
	while index < bytes.len() {
		if bytes[index] == 0 {
			return Err(WorkerNameError::ContainsNullByte);
		}
		index += 1;
	}
	Ok(())
}

fn check_dedupe_key(text: &str) -> Result<(), DedupeKeyError> {
	if text.is_empty() {
		return Err(DedupeKeyError::Empty);
	}
	if text.len() > MAX_DEDUPE_KEY_LEN {
		return Err(DedupeKeyError::TooLong {
			actual: text.len(),
			max: MAX_DEDUPE_KEY_LEN,
		});
	}
	if text.as_bytes().contains(&0) {
		return Err(DedupeKeyError::ContainsNullByte);
	}
	Ok(())
}

pub(super) fn check_worker_owner_id(text: &str) -> Result<(), WorkerOwnerIdError> {
	if text.is_empty() {
		return Err(WorkerOwnerIdError::Empty);
	}
	if text.len() > MAX_WORKER_OWNER_ID_LEN {
		return Err(WorkerOwnerIdError::TooLong {
			actual: text.len(),
			max: MAX_WORKER_OWNER_ID_LEN,
		});
	}
	if text.as_bytes().contains(&0) {
		return Err(WorkerOwnerIdError::ContainsNullByte);
	}
	Ok(())
}

/// The name of a queue task.
///
/// A task name has 1 to [`MAX_TASK_NAME_LEN`] bytes. It consists of ASCII letters, digits,
/// underscores, hyphens, and dots, and it starts with an ASCII letter, a digit, or an
/// underscore. Producers and workers agree on a task through its name, so the name is a stable
/// protocol string.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TaskName(Cow<'static, str>);

impl TaskName {
	/// Parses a task name from text that the program builds at run time.
	pub fn new(text: impl Into<String>) -> Result<Self, TaskNameError> {
		let text = text.into();
		check_task_name(&text)?;
		Ok(Self(Cow::Owned(text)))
	}

	/// Parses a task name from a literal.
	///
	/// An invalid literal panics. In a `const`, the panic fails the build.
	pub const fn from_static(text: &'static str) -> Self {
		match check_task_name(text) {
			Ok(()) => Self(Cow::Borrowed(text)),
			Err(TaskNameError::Empty) => panic!("queue task name is required"),
			Err(TaskNameError::TooLong { .. }) => panic!("queue task name is too long"),
			Err(TaskNameError::InvalidByte) => panic!(
				"queue task name must contain only ASCII letters, digits, underscores, hyphens, or dots, and must start with an ASCII letter, digit, or underscore"
			),
		}
	}

	/// Returns the task name text.
	pub fn as_str(&self) -> &str {
		&self.0
	}
}

impl AsRef<str> for TaskName {
	fn as_ref(&self) -> &str {
		self.as_str()
	}
}

impl fmt::Display for TaskName {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str(self.as_str())
	}
}

/// The key that makes one active job of a task unique.
///
/// A dedupe key has 1 to [`MAX_DEDUPE_KEY_LEN`] bytes and contains no null byte.
///
/// A dedupe key prevents a second pending or running job of the same task with the same key. It
/// does not prevent a repeated external effect. After the job completes, fails, or moves to the
/// dead letters, the application can enqueue another job with the same key, and the handler of
/// one job can still run more than once; see [`TaskRegistry`](crate::queue::TaskRegistry).
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DedupeKey(String);

impl DedupeKey {
	/// Parses a dedupe key.
	pub fn new(text: impl Into<String>) -> Result<Self, DedupeKeyError> {
		let text = text.into();
		check_dedupe_key(&text)?;
		Ok(Self(text))
	}

	/// Returns the dedupe key text.
	pub fn as_str(&self) -> &str {
		&self.0
	}
}

impl AsRef<str> for DedupeKey {
	fn as_ref(&self) -> &str {
		self.as_str()
	}
}

impl fmt::Display for DedupeKey {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str(self.as_str())
	}
}

/// The logical name of a queue worker.
///
/// A worker name has 1 to [`MAX_QUEUE_WORKER_NAME_LEN`] bytes and contains no null byte.
/// [`WorkerOwnerId::generate`](crate::queue::WorkerOwnerId::generate) derives from it the label of
/// each batch of claims that a worker makes.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WorkerName(Cow<'static, str>);

impl WorkerName {
	/// Parses a worker name from text that the program builds at run time.
	pub fn new(text: impl Into<String>) -> Result<Self, WorkerNameError> {
		let text = text.into();
		check_worker_name(&text)?;
		Ok(Self(Cow::Owned(text)))
	}

	/// Parses a worker name from a literal.
	///
	/// An invalid literal panics. In a `const`, the panic fails the build.
	pub const fn from_static(text: &'static str) -> Self {
		match check_worker_name(text) {
			Ok(()) => Self(Cow::Borrowed(text)),
			Err(WorkerNameError::Empty) => panic!("queue worker name is required"),
			Err(WorkerNameError::TooLong { .. }) => panic!("queue worker name is too long"),
			Err(WorkerNameError::ContainsNullByte) => {
				panic!("queue worker name must not contain null bytes")
			}
		}
	}

	/// Returns the worker name text.
	pub fn as_str(&self) -> &str {
		&self.0
	}
}

impl AsRef<str> for WorkerName {
	fn as_ref(&self) -> &str {
		self.as_str()
	}
}

impl fmt::Display for WorkerName {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str(self.as_str())
	}
}
