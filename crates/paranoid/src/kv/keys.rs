use crate::kv::{KeyPrefix, RecordKeyPrefix};

impl From<&KeyPrefix> for RecordKeyPrefix {
	/// Converts the prefix's persisted text into bytes, which selects the record keys of v10 KV
	/// values with keys that start with the prefix.
	fn from(prefix: &KeyPrefix) -> Self {
		Self::new(prefix.as_str().as_bytes().to_vec()).expect("a key prefix is not empty")
	}
}
