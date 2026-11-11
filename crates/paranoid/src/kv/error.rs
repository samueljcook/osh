//! The error types of the KV primitive.
//!
//! Each operation returns an error type that lists exactly the failures that it can produce,
//! and operations with the same failures share one type. A database failure is the
//! `Database` variant. A protocol state that this release cannot run under is the `Protocol`
//! variant, so a caller can tell it apart from a database failure without inspecting one.
//! An operation that owns its transaction keeps both an operation failure and a rollback failure
//! in its `RollbackFailed` variant.

use std::time::Duration;

/// A time to live is invalid.
#[derive(Debug, thiserror::Error)]
pub enum TtlError {
	/// A positive TTL must be supplied through `Ttl::expires_after`.
	#[error("KV TTL cannot be zero")]
	Zero,
	/// A positive TTL was below [`crate::kv::MIN_TTL`].
	#[error("KV TTL is below the minimum of {minimum:?}")]
	BelowMinimum {
		/// Minimum accepted positive TTL, [`crate::kv::MIN_TTL`].
		minimum: Duration,
	},
	/// A TTL was too large to bind safely as microseconds.
	#[error("KV TTL is too large")]
	TooLarge,
}

/// A page limit exceeds the supported maximum.
#[derive(Debug, thiserror::Error)]
pub enum PageLimitError {
	/// The page limit exceeded [`crate::kv::MAX_PAGE_LIMIT`].
	#[error("KV page limit is {actual}, maximum is {max}")]
	TooLarge {
		/// Requested scan limit.
		actual: u32,
		/// Maximum accepted scan limit, [`crate::kv::MAX_PAGE_LIMIT`].
		max: u32,
	},
}
