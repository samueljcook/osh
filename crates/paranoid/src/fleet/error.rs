//! The error types of Fleet.
//!
//! Each operation returns an error type that lists exactly the failures that it can produce,
//! and operations with the same failures share one type. A database failure is the `Database`
//! variant. A protocol state that this release cannot run under is the `Protocol` variant, so a
//! caller can tell it apart from a database failure without inspecting one. An operation that
//! owns its transaction keeps both an operation failure and a rollback failure in its
//! `RollbackFailed` variant.
//!
//! A guarded task's result types keep the task's own error separate from Paranoid's failures.

use super::lease::CoordinationKeyError;
use crate::{db::database_work_error, kv};
use primitives_rs::random::secure_bytes as random;

/// Secure random generation for a Fleet value failed.
#[derive(Debug, thiserror::Error)]
pub enum RandomnessError {
	/// Generating a holder identifier failed.
	#[error("failed to generate a Fleet holder identifier")]
	HolderId(#[source] random::RandomError),
	/// Generating a lease token failed.
	#[error("failed to generate a coordination claim token")]
	LeaseToken(#[source] random::RandomError),
	/// Generating an acquisition capability failed.
	#[error("failed to generate a Fleet acquisition capability")]
	AcquisitionCapability(#[source] random::RandomError),
	/// Generating retry jitter failed.
	#[error("failed to generate Fleet retry jitter")]
	RetryJitter(#[source] random::RandomError),
}

/// A topic sequence was negative.
#[derive(Debug, thiserror::Error)]
#[error("Fleet topic sequence must be non-negative")]
pub struct NegativeTopicSequenceError;

/// A persisted topic event key ended in a suffix that is not a topic sequence.
#[derive(Debug, thiserror::Error)]
#[error("invalid Fleet topic event sequence suffix {key_suffix:?}")]
pub struct InvalidTopicEventSequenceSuffixError {
	/// The rejected key suffix.
	pub key_suffix: String,
}

/// A key is invalid for the coordination records or for the stored records that Fleet builds
/// from it.
#[derive(Debug, thiserror::Error)]
pub enum KeyError {
	/// The key cannot form a coordination key.
	#[error("invalid Fleet key for coordination records")]
	Coordination(#[source] CoordinationKeyError),
	/// The key cannot form a stored-state key.
	#[error("invalid Fleet key for stored records")]
	Stored(#[source] kv::KeyError),
}

/// A rate limit configuration is invalid.
#[derive(Debug, thiserror::Error)]
pub enum ThrottlerRateSettingsError {
	/// The bucket capacity was zero.
	#[error("Fleet rate limit capacity cannot be zero")]
	CapacityIsZero,
	/// The number of tokens per interval was zero.
	#[error("Fleet rate limit tokens-per-interval cannot be zero")]
	TokensPerIntervalIsZero,
	/// The interval was zero or too large to bind as microseconds, or a Throttler's empty bucket
	/// would take longer than [`MAX_SETTINGS_DURATION`](crate::fleet::MAX_SETTINGS_DURATION) to
	/// refill to its capacity.
	#[error(
		"Fleet rate limit interval must be positive and fit in microseconds, and a full refill \
		 must take at most the settings maximum"
	)]
	IntervalInvalid,
}

/// A concurrency limit configuration is invalid.
#[derive(Debug, thiserror::Error)]
pub enum ThrottlerConcurrencySettingsError {
	/// The maximum number of concurrent holders was zero or above the maximum.
	#[error("invalid Fleet max-concurrent value {value}, maximum is {max}")]
	CapacityInvalid {
		/// Rejected value.
		value: u16,
		/// Maximum accepted value.
		max: u16,
	},
	/// The maximum hold duration was zero or longer than
	/// [`MAX_SETTINGS_DURATION`](crate::fleet::MAX_SETTINGS_DURATION).
	#[error("Fleet max-hold duration must be positive and at most the settings maximum")]
	MaxHoldDurationInvalid,
}

/// A circuit breaker configuration is invalid.
#[derive(Debug, thiserror::Error)]
pub enum ThrottlerCircuitSettingsError {
	/// The failure threshold was zero.
	#[error("Fleet circuit breaker failure threshold cannot be zero")]
	FailureThresholdIsZero,
	/// The recovery timeout was zero or longer than
	/// [`MAX_SETTINGS_DURATION`](crate::fleet::MAX_SETTINGS_DURATION).
	#[error(
		"Fleet circuit breaker recovery timeout must be positive and at most the settings maximum"
	)]
	RecoveryTimeoutInvalid,
	/// The probe window was zero or longer than
	/// [`MAX_SETTINGS_DURATION`](crate::fleet::MAX_SETTINGS_DURATION).
	#[error("Fleet circuit breaker probe window must be positive and at most the settings maximum")]
	ProbeWindowInvalid,
}

/// Defines an error enum for an operation that can fail with the listed causes and the
/// database work that the operation owns. Each cause is one variant. The macro appends the
/// variants `Protocol`, `Database`, and `RollbackFailed` after the causes, and implements the
/// conversions that the transaction runners need.
macro_rules! fleet_error {
	(
		$(#[$meta:meta])*
		pub enum $name:ident {
			$($source:ident),* $(,)?
		}
	) => {
		fleet_error!(@collect [$(#[$meta])*] $name [] [] $($source)*);
	};

	(@collect [$($meta:tt)*] $name:ident [$($variants:tt)*] [$($impls:tt)*]) => {
		database_work_error! {
			$($meta)*
			pub enum $name {
				$($variants)*
			}
		}
		$($impls)*
	};

	(@collect [$($meta:tt)*] $name:ident [$($variants:tt)*] [$($impls:tt)*] Randomness $($rest:ident)*) => {
		fleet_error!(@collect [$($meta)*] $name [$($variants)*
			/// Secure random generation failed.
			#[error("secure random generation failed")]
			Randomness(#[from] $crate::fleet::RandomnessError),
		] [$($impls)*

		] $($rest)*);
	};

	(@collect [$($meta:tt)*] $name:ident [$($variants:tt)*] [$($impls:tt)*] PersistedLease $($rest:ident)*) => {
		fleet_error!(@collect [$($meta)*] $name [$($variants)*
			/// A persisted coordination claim holds a value that Fleet cannot read.
			#[error("a persisted coordination claim is invalid")]
			PersistedLease(#[from] $crate::fleet::PersistedLeaseError),
		] [$($impls)*

		] $($rest)*);
	};
}

fleet_error! {
	/// An operation that can fail only through the database and the protocol.
	pub enum OperationError {}
}

fleet_error! {
	/// Claiming or renewing a coordination claim failed.
	pub enum ClaimError { Randomness, PersistedLease }
}
