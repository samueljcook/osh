//! Validated persisted text keys and prefixes for v10 point records and encrypted Raw records.

use crate::records::RecordKey;

/// Separator used when composing persisted KV keys from key parts.
pub const KEY_SEPARATOR: &str = "::";

/// Maximum accepted persisted KV key length, in bytes.
pub const MAX_KEY_LEN: usize = 2048;

/// A key, key part, or key prefix is invalid.
#[derive(Debug, thiserror::Error)]
pub enum KeyError {
	/// A key needed at least one part.
	#[error("KV key must contain at least one part")]
	Empty,
	/// A key part was empty.
	#[error("KV key part must not be empty")]
	EmptyPart,
	/// A key part contained the key separator byte.
	#[error("KV key part must not contain ':'")]
	PartContainsSeparatorByte,
	/// A key part contained a null byte.
	#[error("KV key part must not contain null bytes")]
	PartContainsNullByte,
	/// Stored key text did not end with the key separator.
	#[error("stored KV key does not end with the key separator")]
	MissingFinalSeparator,
	/// A composed key exceeded [`MAX_KEY_LEN`].
	#[error("KV key is {actual} bytes, maximum is {max}")]
	TooLong {
		/// The supplied key's length in bytes.
		actual: usize,
		/// Maximum accepted key byte length, [`MAX_KEY_LEN`].
		max: usize,
	},
}

/// Validated persisted KV key.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Key(String);

/// Validated persisted KV key prefix.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct KeyPrefix(String);

impl Key {
	/// Validates and joins key parts into a persisted KV key.
	pub fn from_parts<S, I>(parts: I) -> Result<Self, KeyError>
	where
		S: AsRef<str>,
		I: IntoIterator<Item = S>,
	{
		Ok(Self(build_key_from_parts(parts)?))
	}

	/// Validates and joins suffix parts onto an already validated key prefix.
	pub fn from_prefix_and_parts<S, I>(
		prefix: &KeyPrefix,
		suffix_parts: I,
	) -> Result<Self, KeyError>
	where
		S: AsRef<str>,
		I: IntoIterator<Item = S>,
	{
		let key = build_key_from_prefix_and_parts(prefix, suffix_parts)?;
		Ok(Self(key))
	}

	/// Returns the persisted key text.
	pub fn as_str(&self) -> &str {
		&self.0
	}

	pub(crate) fn into_string(self) -> String {
		self.0
	}

	/// Reads key text from a stored row, which is canonical only when it consists of the parts that
	/// `from_parts` would join: each part nonempty, with no colon and no NUL byte, and the
	/// separator after every part including the last.
	pub(crate) fn from_persisted_text(text: String) -> Result<Self, KeyError> {
		let parts = text
			.strip_suffix(KEY_SEPARATOR)
			.ok_or(KeyError::MissingFinalSeparator)?;
		Self::from_parts(parts.split(KEY_SEPARATOR))
	}
}

impl From<&Key> for RecordKey {
	/// Converts the key's persisted text into bytes. A v10 import moves each v10 KV value into the
	/// core Raw namespace under this record key.
	fn from(key: &Key) -> Self {
		Self::new(key.as_str().as_bytes().to_vec())
	}
}

impl KeyPrefix {
	/// Parses the exact persisted parts grammar through the published complete-key parser.
	pub(crate) fn from_persisted_text(text: String) -> Result<Self, KeyError> {
		Key::from_persisted_text(text).map(|key| Self(key.0))
	}

	/// Validates and joins key parts into a persisted KV key prefix.
	pub fn from_parts<S, I>(parts: I) -> Result<Self, KeyError>
	where
		S: AsRef<str>,
		I: IntoIterator<Item = S>,
	{
		let key = build_key_from_parts(parts)?;
		Ok(Self(key))
	}

	/// Returns the persisted prefix text.
	pub fn as_str(&self) -> &str {
		&self.0
	}

	/// Reports whether a full persisted key belongs to this prefix.
	pub fn contains_key(&self, key: &Key) -> bool {
		key.as_str().starts_with(self.as_str())
	}
}

fn build_key_from_prefix_and_parts<S, I>(
	prefix: &KeyPrefix,
	suffix_parts: I,
) -> Result<String, KeyError>
where
	S: AsRef<str>,
	I: IntoIterator<Item = S>,
{
	append_key_parts(prefix.as_str().to_owned(), suffix_parts)
}

fn build_key_from_parts<S, I>(parts: I) -> Result<String, KeyError>
where
	S: AsRef<str>,
	I: IntoIterator<Item = S>,
{
	let key = append_key_parts(String::new(), parts)?;
	if key.is_empty() {
		return Err(KeyError::Empty);
	}
	Ok(key)
}

fn append_key_parts<S, I>(mut key: String, parts: I) -> Result<String, KeyError>
where
	S: AsRef<str>,
	I: IntoIterator<Item = S>,
{
	for part in parts {
		let part = part.as_ref();
		validate_key_part(part)?;
		key.push_str(part);
		key.push_str(KEY_SEPARATOR);
	}
	validate_key_length(&key)?;
	Ok(key)
}

fn validate_key_part(part: &str) -> Result<(), KeyError> {
	if part.is_empty() {
		return Err(KeyError::EmptyPart);
	}
	if part.as_bytes().contains(&b':') {
		return Err(KeyError::PartContainsSeparatorByte);
	}
	if part.as_bytes().contains(&0) {
		return Err(KeyError::PartContainsNullByte);
	}
	Ok(())
}

fn validate_key_length(key: &str) -> Result<(), KeyError> {
	let actual = key.len();
	if actual > MAX_KEY_LEN {
		return Err(KeyError::TooLong {
			actual,
			max: MAX_KEY_LEN,
		});
	}
	Ok(())
}
