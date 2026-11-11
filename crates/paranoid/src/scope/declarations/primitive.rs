//! Semaphore, Throttler, cache, and Topic scope declarations.

use std::sync::Arc;

use super::{
	AppDeclarationError, AppDeclarationInnerError, InitialUse, PayloadUseIntent,
	PrimitiveUseIntent, ScopeIdentity,
};
use crate::fleet::cache::entry_state::{CacheEntryComponent, CacheEntryComponentFields};
use crate::fleet::payload::scope::retained::{
	RetainedCacheEntryScopeCode, RetainedCacheEntryScopeCodeFields, RetainedPayloadCode,
	RetainedTopicEventScopeCode, RetainedTopicEventScopeCodeFields,
};
use crate::fleet::primitive_scope::{
	ManagedPrimitiveRegistration, NativePrimitiveCodeAccess, PrimitiveScopeDeclaration,
	RetainedSemaphoreScopeCode, RetainedSemaphoreScopeCodeFields, RetainedThrottlerScopeCode,
	RetainedThrottlerScopeCodeFields,
};
use crate::fleet::semaphore::state::{SemaphoreComponent, SemaphoreComponentFields};
use crate::fleet::throttler::state::{ThrottlerComponent, ThrottlerComponentFields};
use crate::fleet::topic::event_state::{TopicEventComponent, TopicEventComponentFields};
use crate::records::state_schema::StateSchema;
use crate::runtime::participation::state::catalog::ParticipantUseReference;
use crate::scope::payload::managed::{
	ManagedPayloadRegistration, TypedManagedPayloadSchema, TypedManagedPayloadSchemaFields,
};
use crate::scope::payload::{PayloadRole, PayloadRoleSupport};
use crate::scope::{CacheScope, PrimitiveRole, PrimitiveScopeSupport, TopicScope};

/// The inputs of a Semaphore or Throttler scope declaration.
pub struct AppPrimitiveDeclarationFields {
	/// The scope's identity.
	pub scope: ScopeIdentity,
	/// The scope's roles that the code supports, and whether the runtime uses each one when it
	/// starts.
	pub intent: PrimitiveUseIntent,
}

/// The inputs of a cache scope declaration.
pub struct AppCacheDeclarationFields<T, C> {
	/// The scope and the codecs of its values.
	pub scope: CacheScope<T, C>,
	/// The roles of the cache entries that the code supports.
	pub intent: PrimitiveUseIntent,
	/// The roles of the cached values that the code supports.
	pub payload_intent: PayloadUseIntent,
}

/// The inputs of a Topic scope declaration.
pub struct AppTopicDeclarationFields<T, C> {
	/// The scope and the codecs of its event payloads.
	pub scope: TopicScope<T, C>,
	/// The roles of the Topic events that the code supports.
	pub intent: PrimitiveUseIntent,
	/// The roles of the event payloads that the code supports.
	pub payload_intent: PayloadUseIntent,
}

/// One Semaphore, Throttler, cache, or Topic scope that the app declares.
pub struct AppPrimitiveDeclaration {
	pub(super) scope: ScopeIdentity,
	pub(super) support: Option<PrimitiveScopeSupport>,
	pub(super) payload_support: Vec<PayloadRoleSupport>,
	pub(super) initial_uses: Vec<ParticipantUseReference>,
	pub(super) managed: ManagedPrimitiveRegistration,
}

impl AppPrimitiveDeclaration {
	/// Declares a Semaphore scope without database work. It fails when the code cannot provide a
	/// role that the intent marks as supported.
	pub fn semaphore(fields: AppPrimitiveDeclarationFields) -> Result<Self, AppDeclarationError> {
		let code_access = code_access(&fields.intent);
		let code = RetainedSemaphoreScopeCode::new(RetainedSemaphoreScopeCodeFields {
			identity: fields.scope,
			component: Arc::new(SemaphoreComponent::new(SemaphoreComponentFields {
				code_access,
			})),
		});
		Self::build(
			ManagedPrimitiveRegistration::Semaphore(Arc::new(code)),
			&fields.intent,
			None,
		)
	}

	/// Declares a Throttler scope without database work. It fails when the code cannot provide a
	/// role that the intent marks as supported.
	pub fn throttler(fields: AppPrimitiveDeclarationFields) -> Result<Self, AppDeclarationError> {
		let code_access = code_access(&fields.intent);
		let code = RetainedThrottlerScopeCode::new(RetainedThrottlerScopeCodeFields {
			identity: fields.scope,
			component: Arc::new(ThrottlerComponent::new(ThrottlerComponentFields {
				code_access,
			})),
		});
		Self::build(
			ManagedPrimitiveRegistration::Throttler(Arc::new(code)),
			&fields.intent,
			None,
		)
	}

	/// Declares a cache scope without database work. It fails when the code cannot provide a
	/// role that the intent marks as supported.
	pub fn cache<T, C>(fields: AppCacheDeclarationFields<T, C>) -> Result<Self, AppDeclarationError>
	where
		StateSchema<T, C>: Send + Sync,
		T: 'static,
		C: Send + Sync + 'static,
	{
		let code_access = code_access(&fields.intent);
		let code = RetainedCacheEntryScopeCode::new(RetainedCacheEntryScopeCodeFields {
			identity: fields.scope.identity().clone(),
			component: Arc::new(CacheEntryComponent::new(CacheEntryComponentFields {
				code_access,
			})),
			payload: RetainedPayloadCode::Schema(payload_code(fields.scope.schema())),
		});
		Self::build(
			ManagedPrimitiveRegistration::CacheEntry(Arc::new(code)),
			&fields.intent,
			Some(&fields.payload_intent),
		)
	}

	/// Declares a Topic scope without database work. It fails when the code cannot provide a
	/// role that the intent marks as supported.
	pub fn topic<T, C>(fields: AppTopicDeclarationFields<T, C>) -> Result<Self, AppDeclarationError>
	where
		StateSchema<T, C>: Send + Sync,
		T: 'static,
		C: Send + Sync + 'static,
	{
		let code_access = code_access(&fields.intent);
		let code = RetainedTopicEventScopeCode::new(RetainedTopicEventScopeCodeFields {
			identity: fields.scope.identity().clone(),
			component: Arc::new(TopicEventComponent::new(TopicEventComponentFields {
				code_access,
			})),
			payload: RetainedPayloadCode::Schema(payload_code(fields.scope.schema())),
		});
		Self::build(
			ManagedPrimitiveRegistration::TopicEvent(Arc::new(code)),
			&fields.intent,
			Some(&fields.payload_intent),
		)
	}

	fn build(
		managed: ManagedPrimitiveRegistration,
		intent: &PrimitiveUseIntent,
		payload_intent: Option<&PayloadUseIntent>,
	) -> Result<Self, AppDeclarationError> {
		let (scope, support, payload_support, initial_uses) = {
			let declaration = managed.declaration();
			let (support, mut initial_uses) = derive_primitive_support(&declaration, intent)?;
			let (payload_support, payload_uses) = match (managed.payload_code(), payload_intent) {
				(Some(RetainedPayloadCode::Schema(code)), Some(payload_intent)) => {
					derive_payload_support(&declaration, code, payload_intent)?
				}
				_ => (Vec::new(), Vec::new()),
			};
			initial_uses.extend(payload_uses);
			(
				declaration.identity().clone(),
				support,
				payload_support,
				initial_uses,
			)
		};
		Ok(Self {
			scope,
			support,
			payload_support,
			initial_uses,
			managed,
		})
	}
}

/// The code registers encoders only when it supports the writer role.
fn code_access(intent: &PrimitiveUseIntent) -> NativePrimitiveCodeAccess {
	if intent.writer.is_supported() {
		NativePrimitiveCodeAccess::ReaderWriter
	} else {
		NativePrimitiveCodeAccess::ReaderOnly
	}
}

fn payload_code<T, C>(schema: &Arc<StateSchema<T, C>>) -> ManagedPayloadRegistration
where
	StateSchema<T, C>: Send + Sync,
	T: 'static,
	C: Send + Sync + 'static,
{
	TypedManagedPayloadSchema::new(TypedManagedPayloadSchemaFields {
		schema: Arc::clone(schema),
	})
	.into_registration()
}

fn derive_primitive_support(
	declaration: &PrimitiveScopeDeclaration<'_, '_>,
	intent: &PrimitiveUseIntent,
) -> Result<(Option<PrimitiveScopeSupport>, Vec<ParticipantUseReference>), AppDeclarationError> {
	let mut roles = Vec::new();
	let mut initial_uses = Vec::new();
	for (role, requested) in [
		(PrimitiveRole::Reader, intent.reader.initial_use()),
		(PrimitiveRole::Writer, intent.writer.initial_use()),
	] {
		if let Some(initial_use) = requested {
			roles.push(role);
			if initial_use == InitialUse::Active {
				initial_uses.push(ParticipantUseReference::Primitive {
					scope: declaration.identity().clone(),
					role,
				});
			}
		}
	}
	let support = if roles.is_empty() {
		None
	} else {
		Some(
			PrimitiveScopeSupport::from_declaration(declaration, roles).map_err(|error| {
				AppDeclarationError::at_scope(
					declaration.identity(),
					AppDeclarationInnerError::Scope(error),
				)
			})?,
		)
	};
	Ok((support, initial_uses))
}

fn derive_payload_support(
	declaration: &PrimitiveScopeDeclaration<'_, '_>,
	code: &ManagedPayloadRegistration,
	intent: &PayloadUseIntent,
) -> Result<(Vec<PayloadRoleSupport>, Vec<ParticipantUseReference>), AppDeclarationError> {
	let fail = |error| AppDeclarationError::at_scope(declaration.identity(), error);
	let mut support = Vec::new();
	let mut uses = Vec::new();
	for (role, requested) in [
		(PayloadRole::Reader, intent.reader.initial_use()),
		(PayloadRole::Producer, intent.producer.initial_use()),
	] {
		if let Some(initial) = requested {
			support.push(
				code.support(declaration.identity(), role)
					.map_err(|error| fail(AppDeclarationInnerError::Payload(Box::new(error))))?,
			);
			if initial == InitialUse::Active {
				uses.push(ParticipantUseReference::Payload {
					scope: declaration.identity().clone(),
					role,
				});
			}
		}
	}
	Ok((support, uses))
}
