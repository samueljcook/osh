//! Actual Cache/Topic intrinsic role bindings with separately bound application payload roles.
//!
//! These pure code bindings provide no installed placement, participant admission or mutation.

use crate::fleet::cache::entry_state::CacheEntryStateError;
use crate::fleet::payload::identity::NativeFleetPayloadIdentity;
use crate::fleet::payload::state::{FleetPayloadPolicyError, FleetPayloadStateError};
use crate::fleet::topic::event_state::TopicEventStateError;
use crate::records::{RecordIdentity, StoredRecord};
use crate::scope::{PrimitivePayloadPolicy, ScopeError, ScopeIdentity, ScopeOwner, ScopePolicy};

pub(crate) mod cache;
pub(crate) mod conversion;
pub(crate) mod retained;
pub(crate) mod topic;

pub(crate) struct PayloadScopeRecordFields<'a> {
	pub(crate) identity: &'a RecordIdentity,
	pub(crate) record: &'a StoredRecord,
}

fn payload_policy(policy: &ScopePolicy) -> Result<&PrimitivePayloadPolicy, PayloadScopeError> {
	let ScopeOwner::Primitive(primitive) = policy.owner() else {
		return Err(ScopeError::WrongOwner.into());
	};
	if matches!(primitive.payload(), PrimitivePayloadPolicy::None) {
		return Err(PayloadScopeError::PayloadRequired);
	}
	Ok(primitive.payload())
}

fn require_record_identity(
	scope: &ScopeIdentity,
	key: NativeFleetPayloadIdentity<'_>,
	actual: &RecordIdentity,
) -> Result<(), PayloadScopeError> {
	let expected = RecordIdentity::new(scope.namespace().clone(), key.record_key());
	if &expected != actual {
		return Err(PayloadScopeError::RecordIdentityMismatch {
			expected,
			actual: actual.clone(),
		});
	}
	Ok(())
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum PayloadScopeError {
	#[error("CacheValue and TopicEvent require explicit opaque or structured payload ownership")]
	PayloadRequired,
	#[error("payload envelope lookup differs from its complete scoped identity")]
	RecordIdentityMismatch {
		expected: RecordIdentity,
		actual: RecordIdentity,
	},
	#[error("payload envelope scope does not bind its intrinsic declaration")]
	Scope(#[from] ScopeError),
	#[error("cache envelope does not match its identity and selected codec")]
	Cache(#[from] CacheEntryStateError),
	#[error("topic envelope does not match its identity and selected codec")]
	Topic(#[from] TopicEventStateError),
	#[error("payload transport does not match the installed scoped payload policy")]
	PayloadPolicy(#[from] FleetPayloadPolicyError),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum PayloadScopeValueError<C> {
	#[error("payload operation does not bind its intrinsic scope")]
	Scope(#[from] PayloadScopeError),
	#[error("payload operation failed under its application codec")]
	Payload(#[from] FleetPayloadStateError<C>),
}
