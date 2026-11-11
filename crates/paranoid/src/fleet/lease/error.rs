use super::{MAX_HOLDER_ID_LEN, MAX_LEASE_KEY_LEN, MIN_CLAIM_DURATION, StdDuration};

/// A coordination key or one of its parts is invalid.
#[derive(Debug, thiserror::Error)]
pub enum CoordinationKeyError {
	/// A key needed at least one part.
	#[error("coordination key must contain at least one part")]
	Empty,
	/// A key part was empty.
	#[error("coordination key part must not be empty")]
	EmptyPart,
	/// A key part contained the key separator byte.
	#[error("coordination key part must not contain ':'")]
	PartContainsSeparatorByte,
	/// A key part contained a null byte.
	#[error("coordination key part must not contain null bytes")]
	PartContainsNullByte,
	/// A composed key exceeded the maximum accepted byte length.
	#[error("coordination key is {actual} bytes, maximum is {max}")]
	TooLong {
		/// The supplied key's length in bytes.
		actual: usize,
		/// Maximum accepted key byte length.
		max: usize,
	},
}

impl CoordinationKeyError {
	pub(super) fn too_long(actual: usize) -> Self {
		Self::TooLong {
			actual,
			max: MAX_LEASE_KEY_LEN,
		}
	}
}

/// A holder prefix is invalid.
#[derive(Debug, thiserror::Error)]
pub enum HolderPrefixError {
	/// Holder prefixes must be non-empty.
	#[error("Fleet holder prefix must not be empty")]
	Empty,
	/// A holder prefix contained a null byte.
	#[error("Fleet holder prefix must not contain null bytes")]
	ContainsNullByte,
	/// A holder prefix exceeded the maximum accepted byte length.
	#[error("Fleet holder prefix is {actual} bytes, maximum is {max}")]
	TooLong {
		/// The supplied holder prefix's length in bytes.
		actual: usize,
		/// Maximum accepted holder prefix byte length.
		max: usize,
	},
}

/// A holder identifier is invalid.
#[derive(Debug, thiserror::Error)]
pub enum HolderIdError {
	/// Holder identifiers must be non-empty.
	#[error("coordination holder identifier must not be empty")]
	Empty,
	/// A holder identifier contained a null byte.
	#[error("coordination holder identifier must not contain null bytes")]
	ContainsNullByte,
	/// A holder identifier exceeded the maximum accepted byte length.
	#[error("coordination holder identifier is {actual} bytes, maximum is {max}")]
	TooLong {
		/// The supplied holder identifier's length in bytes.
		actual: usize,
		/// Maximum accepted holder identifier byte length.
		max: usize,
	},
}

impl HolderIdError {
	pub(super) fn too_long(actual: usize) -> Self {
		Self::TooLong {
			actual,
			max: MAX_HOLDER_ID_LEN,
		}
	}
}

/// A claim duration is invalid.
#[derive(Debug, thiserror::Error)]
pub enum ClaimDurationError {
	/// A positive claim duration must be supplied through `ClaimDuration::new`.
	#[error("coordination claim duration cannot be zero")]
	Zero,
	/// A positive duration was below the configured minimum.
	#[error("coordination claim duration is below the minimum of {minimum:?}")]
	BelowMinimum {
		/// Minimum accepted positive duration.
		minimum: StdDuration,
	},
	/// A duration was too large to bind safely as microseconds.
	#[error("coordination claim duration is too large")]
	TooLarge,
}

impl ClaimDurationError {
	pub(super) fn below_minimum() -> Self {
		Self::BelowMinimum {
			minimum: MIN_CLAIM_DURATION,
		}
	}
}

/// A claim row in the database holds a value that Fleet cannot read.
#[derive(Debug, thiserror::Error)]
pub enum PersistedLeaseError {
	/// The persisted holder identifier is invalid.
	#[error("persisted coordination holder identifier is invalid")]
	HolderId(#[source] HolderIdError),
	/// The persisted fencing token was not positive.
	#[error("persisted coordination fencing token must be positive, got {value}")]
	FencingToken {
		/// The persisted fencing token.
		value: i64,
	},
	/// The persisted claim token bytes had an impossible length.
	#[error("persisted coordination claim token is {actual} bytes, expected {expected}")]
	TokenLength {
		/// The stored token's length in bytes.
		actual: usize,
		/// Expected token byte length.
		expected: usize,
	},
}
