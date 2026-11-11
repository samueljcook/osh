//! Sealed actual Semaphore/Throttler role bindings and retained callable family code.
//!
//! These pure bindings provide no participant admission, physical placement, source authority,
//! acquisition delivery, or transaction permission.

use super::payload::scope::cache::CacheEntryScopeDeclaration;
use super::payload::scope::retained::{
	RetainedCacheEntryScopeCode, RetainedPayloadCode, RetainedPayloadError,
	RetainedTopicEventScopeCode,
};
use super::payload::scope::topic::TopicEventScopeDeclaration;
use super::resource::NativeFleetResource;
use super::semaphore::state::{
	NativeSemaphoreState, SemaphoreBindingFields, SemaphoreComponent, SemaphoreSchemaBinding,
	SemaphoreSettings, SemaphoreStateError, SemaphoreUndeliveredReleaseInput,
	SemaphoreUndeliveredTarget,
};
use super::throttler::state::{
	NativeThrottlerState, ThrottlerBindingFields, ThrottlerComponent, ThrottlerSchemaBinding,
	ThrottlerSettings, ThrottlerStateError,
};
use super::{SemaphoreKey, SemaphoreStatus, ThrottlerKey, ThrottlerStatus};
use crate::installation::contract::{
	BoundComponentContract, BoundComponentReader, ComponentDeclaration, ComponentPolicy,
};
use crate::records::{RecordIdentity, StateTimestampMicros, StoredRecord};
use crate::runtime::participation::state::ParticipantComponentSupport;
use crate::scope::{
	ComponentScopeDeclaration, ComponentScopeKind, ScopeError, ScopeIdentity, ScopePolicy,
};
use crate::upgrade::policy::scope::CandidateScopePolicy;
use std::sync::Arc;

/// Selects actual local codec functions; this does not choose an installed writer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativePrimitiveCodeAccess {
	/// Registers actual readers without intrinsic encoders.
	ReaderOnly,
	/// Registers actual readers and intrinsic encoders.
	ReaderWriter,
}

/// The actual scope identity and borrowed Semaphore codec owner.
pub struct SemaphoreScopeDeclarationFields<'scope, 'code> {
	/// Explicit logical storage and namespace identity.
	pub identity: &'scope ScopeIdentity,
	/// The actual library codec owner.
	pub component: &'code SemaphoreComponent,
}
/// A borrowed Semaphore declaration without installed policy or admission.
pub struct SemaphoreScopeDeclaration<'scope, 'code> {
	identity: &'scope ScopeIdentity,
	component: &'code SemaphoreComponent,
	declaration: ComponentDeclaration<'code, SemaphoreComponent>,
}
impl<'scope, 'code> SemaphoreScopeDeclaration<'scope, 'code> {
	/// Borrows the actual identity and codec owner without accessing stored state.
	pub fn new(fields: SemaphoreScopeDeclarationFields<'scope, 'code>) -> Self {
		Self {
			identity: fields.identity,
			component: fields.component,
			declaration: fields.component.declaration(),
		}
	}
	/// Borrows the exact supplied scope identity.
	pub fn identity(&self) -> &'scope ScopeIdentity {
		self.identity
	}
	pub(crate) fn declaration(&self) -> &ComponentDeclaration<'code, SemaphoreComponent> {
		&self.declaration
	}
	pub(crate) fn record_identity(&self, key: &SemaphoreKey) -> RecordIdentity {
		resource_identity(self.identity, NativeFleetResource::Semaphore(key))
	}
	pub(crate) fn bind_reader<'a>(
		&'a self,
		actual_scope: &ScopeIdentity,
		policy: &'a ScopePolicy,
	) -> Result<BoundSemaphoreScopeReader<'a, 'code>, PrimitiveScopeBindingError> {
		crate::scope::payload_policy::require_no_primitive_payload(policy)?;
		let contract = ComponentScopeDeclaration::new(
			self.identity,
			&self.declaration,
			ComponentScopeKind::Primitive,
		)
		.bind_reader(actual_scope, policy)?;
		contract
			.bind_schema(self.component.schema())
			.map_err(ScopeError::from)?;
		Ok(BoundSemaphoreScopeReader {
			identity: self.identity,
			component: self.component,
			contract,
		})
	}
	pub(crate) fn bind_writer<'a>(
		&'a self,
		actual_scope: &ScopeIdentity,
		policy: &'a ScopePolicy,
	) -> Result<BoundSemaphoreScopeWriter<'a, 'code>, PrimitiveScopeBindingError> {
		crate::scope::payload_policy::require_no_primitive_payload(policy)?;
		let contract = ComponentScopeDeclaration::new(
			self.identity,
			&self.declaration,
			ComponentScopeKind::Primitive,
		)
		.bind(actual_scope, policy)?;
		self.component.require_bound_native_writer(&contract)?;
		Ok(BoundSemaphoreScopeWriter {
			identity: self.identity,
			component: self.component,
			contract,
		})
	}
}
pub(crate) struct SemaphoreResourceReadFields<'a> {
	pub(crate) identity: &'a RecordIdentity,
	pub(crate) key: &'a SemaphoreKey,
	/// The settings that the stored record must have.
	pub(crate) settings: &'a SemaphoreSettings,
	pub(crate) record: &'a StoredRecord,
}
pub(crate) struct SemaphoreResourceWriteFields<'a> {
	pub(crate) identity: &'a RecordIdentity,
	pub(crate) key: &'a SemaphoreKey,
	pub(crate) settings: &'a SemaphoreSettings,
}
/// Actual scope/resource binding precedes the release-only private predicate call.
pub(crate) struct SemaphoreUndeliveredScopeReleaseFields<'a> {
	pub(crate) resource: SemaphoreResourceWriteFields<'a>,
	pub(crate) state: &'a mut NativeSemaphoreState,
	pub(crate) target: &'a SemaphoreUndeliveredTarget,
	pub(crate) observed_at: crate::records::StateTimestampMicros,
}
pub(crate) struct BoundSemaphoreScopeReader<'a, 'code> {
	identity: &'a ScopeIdentity,
	component: &'code SemaphoreComponent,
	contract: BoundComponentReader<'a, 'code, SemaphoreComponent>,
}
impl BoundSemaphoreScopeReader<'_, '_> {
	pub(crate) fn decode_record(
		&self,
		fields: SemaphoreResourceReadFields<'_>,
	) -> Result<NativeSemaphoreState, PrimitiveScopeBindingError> {
		require_identity(
			self.identity,
			fields.identity,
			NativeFleetResource::Semaphore(fields.key),
		)?;
		let reader = self
			.contract
			.bind_schema(self.component.schema())
			.map_err(ScopeError::from)?;
		Ok(super::semaphore::state::read_resource(
			&reader,
			fields.record,
			fields.key,
			fields.settings,
		)?)
	}
	pub(crate) fn status(
		&self,
		fields: SemaphoreResourceReadFields<'_>,
		observed_at: StateTimestampMicros,
	) -> Result<SemaphoreStatus, PrimitiveScopeBindingError> {
		let state = self.decode_record(fields)?;
		Ok(super::semaphore::state::resource_status(
			&state,
			observed_at,
		))
	}
}
pub(crate) struct BoundSemaphoreScopeWriter<'a, 'code> {
	identity: &'a ScopeIdentity,
	component: &'code SemaphoreComponent,
	contract: BoundComponentContract<'a, 'code, SemaphoreComponent>,
}
impl BoundSemaphoreScopeWriter<'_, '_> {
	pub(crate) fn prepare_undelivered_release(
		&self,
		fields: SemaphoreUndeliveredScopeReleaseFields<'_>,
	) -> Result<bool, PrimitiveScopeBindingError> {
		let resource = self.bind_resource(fields.resource)?;
		Ok(resource.release_undelivered(
			fields.state,
			SemaphoreUndeliveredReleaseInput {
				target: fields.target,
				observed_at: fields.observed_at,
			},
		)?)
	}

	pub(crate) fn bind_resource<'a>(
		&'a self,
		fields: SemaphoreResourceWriteFields<'a>,
	) -> Result<SemaphoreSchemaBinding<'a>, PrimitiveScopeBindingError> {
		require_identity(
			self.identity,
			fields.identity,
			NativeFleetResource::Semaphore(fields.key),
		)?;
		Ok(self.component.bind_schema(SemaphoreBindingFields {
			contract: &self.contract,
			key: fields.key,
			settings: fields.settings,
		})?)
	}
}

/// Owned identity and actual Semaphore codecs retained for scoped operations.
pub struct RetainedSemaphoreScopeCodeFields {
	/// Explicit logical storage and namespace identity.
	pub identity: ScopeIdentity,
	/// The actual shared library codec owner.
	pub component: Arc<SemaphoreComponent>,
}
/// Retains actual Semaphore code independently from active participant roles.
pub struct RetainedSemaphoreScopeCode {
	identity: ScopeIdentity,
	component: Arc<SemaphoreComponent>,
}
impl RetainedSemaphoreScopeCode {
	/// Retains the supplied identity and actual shared codec owner.
	pub fn new(fields: RetainedSemaphoreScopeCodeFields) -> Self {
		Self {
			identity: fields.identity,
			component: fields.component,
		}
	}
	/// Borrows a declaration from this retained owner.
	pub fn declaration(&self) -> SemaphoreScopeDeclaration<'_, '_> {
		SemaphoreScopeDeclaration::new(SemaphoreScopeDeclarationFields {
			identity: &self.identity,
			component: &self.component,
		})
	}
}

/// The actual scope identity and borrowed Throttler codec owner.
pub struct ThrottlerScopeDeclarationFields<'scope, 'code> {
	/// Explicit logical storage and namespace identity.
	pub identity: &'scope ScopeIdentity,
	/// The actual library codec owner.
	pub component: &'code ThrottlerComponent,
}
/// A borrowed Throttler declaration without installed policy or admission.
pub struct ThrottlerScopeDeclaration<'scope, 'code> {
	identity: &'scope ScopeIdentity,
	component: &'code ThrottlerComponent,
	declaration: ComponentDeclaration<'code, ThrottlerComponent>,
}
impl<'scope, 'code> ThrottlerScopeDeclaration<'scope, 'code> {
	/// Borrows the actual identity and codec owner without accessing stored state.
	pub fn new(fields: ThrottlerScopeDeclarationFields<'scope, 'code>) -> Self {
		Self {
			identity: fields.identity,
			component: fields.component,
			declaration: fields.component.declaration(),
		}
	}
	/// Borrows the exact supplied scope identity.
	pub fn identity(&self) -> &'scope ScopeIdentity {
		self.identity
	}
	pub(crate) fn declaration(&self) -> &ComponentDeclaration<'code, ThrottlerComponent> {
		&self.declaration
	}
	/// Resolves only this scope's actual closed Throttler accounting key.
	pub(crate) fn outcome_record_identity(
		&self,
		key: &ThrottlerKey,
		outcome_id: &super::throttler::state::outcome::ThrottlerOutcomeId,
	) -> RecordIdentity {
		RecordIdentity::new(
			self.identity.namespace().clone(),
			NativeFleetResource::throttler_outcome_key(key, outcome_id),
		)
	}
	pub(crate) fn bind_reader<'a>(
		&'a self,
		actual_scope: &ScopeIdentity,
		policy: &'a ScopePolicy,
	) -> Result<BoundThrottlerScopeReader<'a, 'code>, PrimitiveScopeBindingError> {
		crate::scope::payload_policy::require_no_primitive_payload(policy)?;
		let contract = ComponentScopeDeclaration::new(
			self.identity,
			&self.declaration,
			ComponentScopeKind::Primitive,
		)
		.bind_reader(actual_scope, policy)?;
		contract
			.bind_schema(self.component.schema())
			.map_err(ScopeError::from)?;
		Ok(BoundThrottlerScopeReader {
			identity: self.identity,
			component: self.component,
			contract,
		})
	}
	pub(crate) fn bind_writer<'a>(
		&'a self,
		actual_scope: &ScopeIdentity,
		policy: &'a ScopePolicy,
	) -> Result<BoundThrottlerScopeWriter<'a, 'code>, PrimitiveScopeBindingError> {
		crate::scope::payload_policy::require_no_primitive_payload(policy)?;
		let contract = ComponentScopeDeclaration::new(
			self.identity,
			&self.declaration,
			ComponentScopeKind::Primitive,
		)
		.bind(actual_scope, policy)?;
		self.component.require_bound_native_writers(&contract)?;
		Ok(BoundThrottlerScopeWriter {
			identity: self.identity,
			component: self.component,
			contract,
		})
	}
}
pub(crate) struct ThrottlerResourceReadFields<'a> {
	pub(crate) identity: &'a RecordIdentity,
	pub(crate) key: &'a ThrottlerKey,
	/// The settings that the stored record must have.
	pub(crate) settings: &'a ThrottlerSettings,
	pub(crate) record: &'a StoredRecord,
}
pub(crate) struct ThrottlerResourceWriteFields<'a> {
	pub(crate) identity: &'a RecordIdentity,
	pub(crate) key: &'a ThrottlerKey,
	pub(crate) settings: &'a ThrottlerSettings,
}
pub(crate) struct BoundThrottlerScopeReader<'a, 'code> {
	identity: &'a ScopeIdentity,
	component: &'code ThrottlerComponent,
	contract: BoundComponentReader<'a, 'code, ThrottlerComponent>,
}
impl BoundThrottlerScopeReader<'_, '_> {
	pub(crate) fn decode_record(
		&self,
		fields: ThrottlerResourceReadFields<'_>,
	) -> Result<NativeThrottlerState, PrimitiveScopeBindingError> {
		require_identity(
			self.identity,
			fields.identity,
			NativeFleetResource::Throttler(fields.key),
		)?;
		let reader = self
			.contract
			.bind_schema(self.component.schema())
			.map_err(ScopeError::from)?;
		Ok(super::throttler::state::read_resource(
			&reader,
			fields.record,
			fields.key,
			fields.settings,
		)?)
	}
	pub(crate) fn status(
		&self,
		fields: ThrottlerResourceReadFields<'_>,
		observed_at: StateTimestampMicros,
	) -> Result<ThrottlerStatus, PrimitiveScopeBindingError> {
		let state = self.decode_record(fields)?;
		Ok(super::throttler::state::resource_status(
			&state,
			observed_at,
		))
	}
}
pub(crate) struct BoundThrottlerScopeWriter<'a, 'code> {
	identity: &'a ScopeIdentity,
	component: &'code ThrottlerComponent,
	contract: BoundComponentContract<'a, 'code, ThrottlerComponent>,
}
impl BoundThrottlerScopeWriter<'_, '_> {
	pub(crate) fn prepare_acquisition_group(
		&self,
		fields: super::throttler::state::outcome::ThrottlerAcquireScopeGroupFields<'_>,
	) -> Result<
		super::throttler::state::outcome::ThrottlerAcquisitionGroupPreparation,
		PrimitiveScopeBindingError,
	> {
		let binding = self.bind_resource(ThrottlerResourceWriteFields {
			identity: fields.group.resource_identity,
			key: fields.key,
			settings: fields.settings,
		})?;
		Ok(binding
			.prepare_acquisition_group(fields.group)
			.map_err(Box::new)?)
	}

	pub(crate) fn prepare_pending_outcome_group(
		&self,
		fields: super::throttler::state::outcome::ThrottlerPendingOutcomeScopeGroupFields<'_>,
	) -> Result<
		super::throttler::state::outcome::ThrottlerOutcomeGroupPreparation,
		PrimitiveScopeBindingError,
	> {
		let binding = self.bind_resource(ThrottlerResourceWriteFields {
			identity: fields.group.resource_identity,
			key: fields.key,
			settings: fields.settings,
		})?;
		Ok(binding
			.prepare_pending_outcome_group(fields.group)
			.map_err(Box::new)?)
	}

	pub(crate) fn prepare_undelivered_group(
		&self,
		fields: super::throttler::state::outcome::ThrottlerUndeliveredScopeGroupFields<'_>,
	) -> Result<
		super::throttler::state::outcome::ThrottlerOutcomeGroupPreparation,
		PrimitiveScopeBindingError,
	> {
		let binding = self.bind_resource(ThrottlerResourceWriteFields {
			identity: fields.group.resource_identity,
			key: fields.key,
			settings: fields.settings,
		})?;
		Ok(binding
			.prepare_undelivered_group(fields.group)
			.map_err(Box::new)?)
	}

	pub(crate) fn bind_resource<'a>(
		&'a self,
		fields: ThrottlerResourceWriteFields<'a>,
	) -> Result<ThrottlerSchemaBinding<'a>, PrimitiveScopeBindingError> {
		require_identity(
			self.identity,
			fields.identity,
			NativeFleetResource::Throttler(fields.key),
		)?;
		Ok(self.component.bind_schema(ThrottlerBindingFields {
			contract: &self.contract,
			key: fields.key,
			settings: fields.settings,
		})?)
	}
}

/// Owned identity and actual Throttler codecs retained for scoped operations.
pub struct RetainedThrottlerScopeCodeFields {
	/// Explicit logical storage and namespace identity.
	pub identity: ScopeIdentity,
	/// The actual shared library codec owner.
	pub component: Arc<ThrottlerComponent>,
}
/// Retains actual Throttler code independently from active participant roles.
pub struct RetainedThrottlerScopeCode {
	identity: ScopeIdentity,
	component: Arc<ThrottlerComponent>,
}
impl RetainedThrottlerScopeCode {
	/// Retains the supplied identity and actual shared codec owner.
	pub fn new(fields: RetainedThrottlerScopeCodeFields) -> Self {
		Self {
			identity: fields.identity,
			component: fields.component,
		}
	}
	/// Borrows a declaration from this retained owner.
	pub fn declaration(&self) -> ThrottlerScopeDeclaration<'_, '_> {
		ThrottlerScopeDeclaration::new(ThrottlerScopeDeclarationFields {
			identity: &self.identity,
			component: &self.component,
		})
	}
}

/// Borrowed actual intrinsic family code for one scope.
pub enum PrimitiveScopeDeclaration<'scope, 'code> {
	/// Actual Semaphore scope code.
	Semaphore(SemaphoreScopeDeclaration<'scope, 'code>),
	/// Actual Throttler scope code.
	Throttler(ThrottlerScopeDeclaration<'scope, 'code>),
	/// Actual CacheEntry scope code.
	CacheEntry(CacheEntryScopeDeclaration<'scope, 'code>),
	/// Actual TopicEvent scope code.
	TopicEvent(TopicEventScopeDeclaration<'scope, 'code>),
}
impl PrimitiveScopeDeclaration<'_, '_> {
	/// Borrows this declaration's exact scope identity.
	pub fn identity(&self) -> &ScopeIdentity {
		match self {
			Self::Semaphore(view) => view.identity(),
			Self::Throttler(view) => view.identity(),
			Self::CacheEntry(view) => view.identity(),
			Self::TopicEvent(view) => view.identity(),
		}
	}
	/// Selects fixed intrinsic policy through the actual family owner, without payload choices.
	pub(crate) fn initial_intrinsic_policy(
		&self,
	) -> Result<ComponentPolicy, PrimitiveScopeBindingError> {
		match self {
			Self::Semaphore(view) => Ok(view.component.initial_policy()?),
			Self::Throttler(view) => Ok(view.component.initial_policy()?),
			Self::CacheEntry(view) => {
				Ok(view.initial_intrinsic_policy().map_err(ScopeError::from)?)
			}
			Self::TopicEvent(view) => {
				Ok(view.initial_intrinsic_policy().map_err(ScopeError::from)?)
			}
		}
	}
	pub(crate) fn component_support(&self) -> ParticipantComponentSupport {
		match self {
			Self::Semaphore(view) => {
				ParticipantComponentSupport::from_declaration(view.declaration())
			}
			Self::Throttler(view) => {
				ParticipantComponentSupport::from_declaration(view.declaration())
			}
			Self::CacheEntry(view) => {
				ParticipantComponentSupport::from_declaration(view.declaration())
			}
			Self::TopicEvent(view) => {
				ParticipantComponentSupport::from_declaration(view.declaration())
			}
		}
	}
	pub(crate) fn supports_writer(&self) -> bool {
		match self {
			Self::Semaphore(view) => supports_native_writer(view.declaration()),
			Self::Throttler(view) => supports_native_writer(view.declaration()),
			Self::CacheEntry(view) => supports_native_writer(view.declaration()),
			Self::TopicEvent(view) => supports_native_writer(view.declaration()),
		}
	}
	pub(crate) fn require_writer(
		&self,
		policy: &ScopePolicy,
	) -> Result<(), PrimitiveScopeBindingError> {
		match self {
			Self::Semaphore(view) => {
				view.bind_writer(view.identity(), policy)?;
			}
			Self::Throttler(view) => {
				view.bind_writer(view.identity(), policy)?;
			}
			Self::CacheEntry(view) => {
				view.bind_writer(view.identity(), policy)
					.map_err(|error| PrimitiveScopeBindingError::Payload(Box::new(error)))?;
			}
			Self::TopicEvent(view) => {
				view.bind_writer(view.identity(), policy)
					.map_err(|error| PrimitiveScopeBindingError::Payload(Box::new(error)))?;
			}
		}
		Ok(())
	}
}
fn supports_native_writer<T>(declaration: &ComponentDeclaration<'_, T>) -> bool {
	declaration.schemas().iter().all(|schema| {
		let mut writable = false;
		schema.for_each_representation(|_, capability| {
			writable |= capability.writable
				&& capability.format
					== crate::records::state_schema::StateRepresentationFormat::Json;
		});
		writable
	})
}
/// One retained actual family owner for a scope, independent from active use.
#[derive(Clone)]
pub(crate) enum ManagedPrimitiveRegistration {
	/// Actual Semaphore scope code.
	Semaphore(Arc<RetainedSemaphoreScopeCode>),
	/// Actual Throttler scope code.
	Throttler(Arc<RetainedThrottlerScopeCode>),
	/// Actual CacheEntry scope code.
	CacheEntry(Arc<RetainedCacheEntryScopeCode>),
	/// Actual TopicEvent scope code.
	TopicEvent(Arc<RetainedTopicEventScopeCode>),
}
impl ManagedPrimitiveRegistration {
	/// Borrows this declaration's exact scope identity.
	pub fn identity(&self) -> &ScopeIdentity {
		match self {
			Self::Semaphore(code) => &code.identity,
			Self::Throttler(code) => &code.identity,
			Self::CacheEntry(code) => code.identity(),
			Self::TopicEvent(code) => code.identity(),
		}
	}
	/// Borrows the actual retained family declaration.
	pub fn declaration(&self) -> PrimitiveScopeDeclaration<'_, '_> {
		match self {
			Self::Semaphore(code) => PrimitiveScopeDeclaration::Semaphore(code.declaration()),
			Self::Throttler(code) => PrimitiveScopeDeclaration::Throttler(code.declaration()),
			Self::CacheEntry(code) => PrimitiveScopeDeclaration::CacheEntry(code.declaration()),
			Self::TopicEvent(code) => PrimitiveScopeDeclaration::TopicEvent(code.declaration()),
		}
	}
	pub(crate) fn payload_code(&self) -> Option<&RetainedPayloadCode> {
		match self {
			Self::Semaphore(_) | Self::Throttler(_) => None,
			Self::CacheEntry(code) => Some(code.payload_code()),
			Self::TopicEvent(code) => Some(code.payload_code()),
		}
	}
	pub(crate) fn bind_final_candidate(
		&self,
		policy: ScopePolicy,
	) -> Result<CandidateScopePolicy, RetainedPayloadError> {
		match self {
			Self::Semaphore(_) | Self::Throttler(_) => Ok(CandidateScopePolicy::from_primitive(
				&self.declaration(),
				policy,
			)?),
			Self::CacheEntry(code) => code.bind_final_candidate(policy),
			Self::TopicEvent(code) => code.bind_final_candidate(policy),
		}
	}
}

fn resource_identity(scope: &ScopeIdentity, resource: NativeFleetResource<'_>) -> RecordIdentity {
	RecordIdentity::new(scope.namespace().clone(), resource.record_key())
}
fn require_identity(
	scope: &ScopeIdentity,
	actual: &RecordIdentity,
	resource: NativeFleetResource<'_>,
) -> Result<(), PrimitiveScopeBindingError> {
	if *actual != resource_identity(scope, resource) {
		return Err(PrimitiveScopeBindingError::RecordIdentityMismatch);
	}
	Ok(())
}
#[derive(Debug, thiserror::Error)]
pub(crate) enum PrimitiveScopeBindingError {
	#[error("payload family does not bind its intrinsic scoped code")]
	Payload(#[from] Box<super::payload::scope::PayloadScopeError>),
	#[error("primitive scope does not bind its code and policy")]
	Scope(#[from] ScopeError),
	#[error("primitive record lookup differs from its declared namespace and resource key")]
	RecordIdentityMismatch,
	#[error("Semaphore resource state was rejected")]
	Semaphore(#[from] SemaphoreStateError),
	#[error("Throttler resource state was rejected")]
	Throttler(#[from] ThrottlerStateError),
	#[error("Throttler anchored group preparation was rejected")]
	ThrottlerGroup(#[from] Box<super::throttler::state::outcome::ThrottlerGroupPreparationError>),
}
