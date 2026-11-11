//! The token proving one worker's claim on one queue job, and its size.

use primitives_rs::random::secure_bytes as random;
use primitives_rs::secret_bytes::SecretToken32;
use std::fmt;
use subtle::ConstantTimeEq;

/// Size, in bytes, of the random token proving a worker's claim on a job.
pub const WORKER_CLAIM_TOKEN_LEN: usize = 32;

/// Unforgeable authority for one claim of one queue job.
#[derive(Clone)]
pub struct WorkerClaimToken(SecretToken32);

impl PartialEq for WorkerClaimToken {
	fn eq(&self, other: &Self) -> bool {
		self.0.ct_eq(&other.0).into()
	}
}

impl Eq for WorkerClaimToken {}

impl WorkerClaimToken {
	/// Restores authority from an already parsed, fixed-size persisted token.
	pub(in crate::queue) fn from_persisted_bytes(bytes: [u8; WORKER_CLAIM_TOKEN_LEN]) -> Self {
		Self(SecretToken32::from_array(bytes))
	}

	pub(in crate::queue) fn generate() -> Result<Self, random::RandomError> {
		SecretToken32::generate().map(Self)
	}

	pub(in crate::queue) fn as_bytes(&self) -> &[u8; WORKER_CLAIM_TOKEN_LEN] {
		self.0.expose_secret()
	}
}

impl fmt::Debug for WorkerClaimToken {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("WorkerClaimToken")
			.field("len", &WORKER_CLAIM_TOKEN_LEN)
			.finish()
	}
}
