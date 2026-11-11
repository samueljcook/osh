//! Byte identities for records, independent of storage and transaction mechanisms.

use std::fmt;

/// Nonempty bytes that distinguish one record namespace within a store.
#[derive(Clone, Eq, Ord, PartialEq, PartialOrd)]
pub struct RecordNamespace(Vec<u8>);

/// A record's key within its namespace. A key can hold any bytes, including none.
#[derive(Clone, Eq, Ord, PartialEq, PartialOrd)]
pub struct RecordKey(Vec<u8>);

/// A record's namespace and key. An absent record also has an identity.
#[derive(Clone, Eq, Ord, PartialEq, PartialOrd)]
pub struct RecordIdentity {
	namespace: RecordNamespace,
	key: RecordKey,
}

impl fmt::Debug for RecordNamespace {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("RecordNamespace")
			.field("byte_len", &self.0.len())
			.finish()
	}
}

impl fmt::Debug for RecordKey {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("RecordKey")
			.field("byte_len", &self.0.len())
			.finish()
	}
}

impl fmt::Debug for RecordIdentity {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("RecordIdentity")
			.field("namespace", &self.namespace)
			.field("key", &self.key)
			.finish()
	}
}

/// An invalid record identity.
#[derive(Debug, thiserror::Error)]
pub enum RecordIdentityError {
	/// No namespace bytes were supplied.
	#[error("a record namespace must not be empty")]
	EmptyNamespace,
}

impl RecordNamespace {
	/// Builds a namespace from nonempty bytes.
	pub fn new(bytes: Vec<u8>) -> Result<Self, RecordIdentityError> {
		if bytes.is_empty() {
			return Err(RecordIdentityError::EmptyNamespace);
		}
		Ok(Self(bytes))
	}

	/// The namespace bytes.
	pub fn as_bytes(&self) -> &[u8] {
		&self.0
	}
}

impl RecordKey {
	/// Builds a key from any bytes, including none.
	pub fn new(bytes: Vec<u8>) -> Self {
		Self(bytes)
	}

	/// The key bytes.
	pub fn as_bytes(&self) -> &[u8] {
		&self.0
	}
}

impl RecordIdentity {
	/// Combines a namespace and a key.
	pub fn new(namespace: RecordNamespace, key: RecordKey) -> Self {
		Self { namespace, key }
	}

	/// Borrows the record's namespace.
	pub fn namespace(&self) -> &RecordNamespace {
		&self.namespace
	}

	/// Borrows the record's key.
	pub fn key(&self) -> &RecordKey {
		&self.key
	}
}
