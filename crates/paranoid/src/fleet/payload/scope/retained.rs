//! One retained actual intrinsic family owner and explicit application payload code per scope.

use std::sync::Arc;

use super::cache::{CacheEntryScopeDeclaration, CacheEntryScopeDeclarationFields};
use super::topic::{TopicEventScopeDeclaration, TopicEventScopeDeclarationFields};
use crate::fleet::cache::entry_state::CacheEntryComponent;
use crate::fleet::primitive_scope::PrimitiveScopeDeclaration;
use crate::fleet::topic::event_state::TopicEventComponent;
use crate::scope::payload::managed::{ManagedPayloadError, ManagedPayloadRegistration};
use crate::scope::{PrimitivePayloadPolicy, ScopeError, ScopeIdentity, ScopeOwner, ScopePolicy};
use crate::upgrade::policy::scope::{CandidateScopePolicy, ScopedUpgradeCompatibilityError};

/// Explicit application codec retention, separate from installed payload policy and active use.
pub(crate) enum RetainedPayloadCode {
	/// Retains the actual typed schema for declared payload roles and complete Schema targets.
	Schema(ManagedPayloadRegistration),
}
impl RetainedPayloadCode {
	fn bind_final_candidate(
		&self,
		intrinsic: &PrimitiveScopeDeclaration<'_, '_>,
		policy: ScopePolicy,
	) -> Result<CandidateScopePolicy, RetainedPayloadError> {
		let ScopeOwner::Primitive(primitive) = policy.owner() else {
			return Err(ScopeError::WrongOwner.into());
		};
		match (self, primitive.payload()) {
			(Self::Schema(code), PrimitivePayloadPolicy::Schema(_)) => {
				Ok(code.bind_final_candidate(intrinsic, policy)?)
			}
			_ => Err(RetainedPayloadError::ModeMismatch),
		}
	}
}
/// Owned CacheEntry scope identity, intrinsic code and explicit business-code retention.
pub(crate) struct RetainedCacheEntryScopeCodeFields {
	/// Explicit logical storage and namespace identity.
	pub(crate) identity: ScopeIdentity,
	/// The actual shared intrinsic envelope owner.
	pub(crate) component: Arc<CacheEntryComponent>,
	/// Application codec retention, without selecting installed payload policy.
	pub(crate) payload: RetainedPayloadCode,
}
/// Retains actual CacheEntry scope code independently from advertised or active roles.
pub(crate) struct RetainedCacheEntryScopeCode {
	identity: ScopeIdentity,
	component: Arc<CacheEntryComponent>,
	payload: RetainedPayloadCode,
}
impl RetainedCacheEntryScopeCode {
	/// Retains the supplied actual owners without binding installed state.
	pub(crate) fn new(fields: RetainedCacheEntryScopeCodeFields) -> Self {
		Self {
			identity: fields.identity,
			component: fields.component,
			payload: fields.payload,
		}
	}
	/// Borrows the exact retained scope identity.
	pub(crate) fn identity(&self) -> &ScopeIdentity {
		&self.identity
	}
	/// Borrows the intrinsic declaration from the same retained owner.
	pub(crate) fn declaration(&self) -> CacheEntryScopeDeclaration<'_, '_> {
		CacheEntryScopeDeclaration::new(CacheEntryScopeDeclarationFields {
			identity: &self.identity,
			component: &self.component,
		})
	}
	pub(crate) fn payload_code(&self) -> &RetainedPayloadCode {
		&self.payload
	}
	pub(crate) fn bind_final_candidate(
		&self,
		policy: ScopePolicy,
	) -> Result<CandidateScopePolicy, RetainedPayloadError> {
		self.payload.bind_final_candidate(
			&PrimitiveScopeDeclaration::CacheEntry(self.declaration()),
			policy,
		)
	}
}
/// Owned TopicEvent scope identity, intrinsic code and explicit business-code retention.
pub(crate) struct RetainedTopicEventScopeCodeFields {
	/// Explicit logical storage and namespace identity.
	pub(crate) identity: ScopeIdentity,
	/// The actual shared intrinsic envelope owner.
	pub(crate) component: Arc<TopicEventComponent>,
	/// Application codec retention, without selecting installed payload policy.
	pub(crate) payload: RetainedPayloadCode,
}
/// Retains actual TopicEvent scope code independently from advertised or active roles.
pub(crate) struct RetainedTopicEventScopeCode {
	identity: ScopeIdentity,
	component: Arc<TopicEventComponent>,
	payload: RetainedPayloadCode,
}
impl RetainedTopicEventScopeCode {
	/// Retains the supplied actual owners without binding installed state.
	pub(crate) fn new(fields: RetainedTopicEventScopeCodeFields) -> Self {
		Self {
			identity: fields.identity,
			component: fields.component,
			payload: fields.payload,
		}
	}
	/// Borrows the exact retained scope identity.
	pub(crate) fn identity(&self) -> &ScopeIdentity {
		&self.identity
	}
	/// Borrows the intrinsic declaration from the same retained owner.
	pub(crate) fn declaration(&self) -> TopicEventScopeDeclaration<'_, '_> {
		TopicEventScopeDeclaration::new(TopicEventScopeDeclarationFields {
			identity: &self.identity,
			component: &self.component,
		})
	}
	pub(crate) fn payload_code(&self) -> &RetainedPayloadCode {
		&self.payload
	}
	pub(crate) fn bind_final_candidate(
		&self,
		policy: ScopePolicy,
	) -> Result<CandidateScopePolicy, RetainedPayloadError> {
		self.payload.bind_final_candidate(
			&PrimitiveScopeDeclaration::TopicEvent(self.declaration()),
			policy,
		)
	}
}
#[derive(Debug, thiserror::Error)]
pub(crate) enum RetainedPayloadError {
	#[error("payload mode differs from the complete target meaning")]
	ModeMismatch,
	#[error("family does not bind its scoped policy")]
	Scope(#[from] ScopeError),
	#[error("intrinsic family does not bind its full target")]
	Candidate(#[from] ScopedUpgradeCompatibilityError),
	#[error("application payload does not bind its full target")]
	Payload(#[from] ManagedPayloadError),
}
