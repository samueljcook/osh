//! Retained actual application payload codecs with sealed synchronous preparations.

use std::{any::Any, fmt, sync::Arc};

use super::{
	PayloadRole, PayloadRoleSupport, PayloadSchemaDeclaration, PayloadSchemaDeclarationFields,
};
use crate::fleet::payload::state::FleetPayload;
use crate::fleet::primitive_scope::PrimitiveScopeDeclaration;
use crate::installation::contract::RepresentationPolicy;
use crate::records::RecordIdentity;
use crate::records::state_schema::{StateSchema, StateSchemaCapabilities};
use crate::scope::{ScopeError, ScopeIdentity, ScopeOwner, ScopePolicy};
use crate::upgrade::policy::scope::{CandidateScopePolicy, ScopedUpgradeCompatibilityError};

/// Actual shared application schema retained for fixed synchronous payload operations.
pub(crate) struct TypedManagedPayloadSchemaFields<T, C> {
	/// The actual schema registry shared with ordinary typed declarations.
	pub(crate) schema: Arc<StateSchema<T, C>>,
}
/// Retains actual application codec functions without retaining a working payload value.
pub(crate) struct TypedManagedPayloadSchema<T, C> {
	schema: Arc<StateSchema<T, C>>,
}
impl<T, C> TypedManagedPayloadSchema<T, C> {
	/// Retains the supplied actual registry without invoking a codec.
	pub(crate) fn new(fields: TypedManagedPayloadSchemaFields<T, C>) -> Self {
		Self {
			schema: fields.schema,
		}
	}
	/// Erases this actual owner for heterogeneous fixed operations.
	///
	/// Working values are created and consumed synchronously. Only the retained owner and
	/// its erased causes require the bounds stated here; codec causes need not implement Error.
	pub(crate) fn into_registration(self) -> ManagedPayloadRegistration
	where
		Self: Send + Sync + 'static,
		T: 'static,
		C: Send + Sync + 'static,
	{
		ManagedPayloadRegistration {
			code: Arc::new(self),
		}
	}
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ManagedPayloadOperation {
	DeclareRole(PayloadRole),
	BindFinalCandidate,
	SelectedWriterConversion,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ManagedPayloadFailureStage {
	RecordIdentity,
	Binding,
	RetainedDecode,
	SelectedEncoding,
}
enum ManagedPayloadCause {
	Scope(ScopeError),
	Candidate(ScopedUpgradeCompatibilityError),
	App,
}
struct ManagedPayloadFailureContext {
	operation: ManagedPayloadOperation,
	stage: ManagedPayloadFailureStage,
	selected_writer: Option<RepresentationPolicy>,
}
pub(crate) struct ManagedPayloadError {
	context: Box<ManagedPayloadFailureContext>,
	source: ManagedPayloadCause,
}
impl fmt::Debug for ManagedPayloadError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("ManagedPayloadError")
			.field("operation", &self.context.operation)
			.field("stage", &self.context.stage)
			.field("selected_writer", &self.context.selected_writer)
			.finish_non_exhaustive()
	}
}
impl fmt::Display for ManagedPayloadError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(
			"the scope's payload code could not declare a role, bind the scope's target, or \
			 convert a payload to the writer version",
		)
	}
}
impl std::error::Error for ManagedPayloadError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match &self.source {
			ManagedPayloadCause::Scope(source) => Some(source),
			ManagedPayloadCause::Candidate(source) => Some(source),
			ManagedPayloadCause::App => None,
		}
	}
}
#[derive(Clone, Copy)]
pub(crate) struct ManagedPayloadRecord<'a> {
	pub(crate) identity: &'a RecordIdentity,
	pub(crate) payload: &'a FleetPayload,
}
trait ErasedManagedPayload: Send + Sync {
	fn as_any(&self) -> &dyn Any;
	fn capabilities(&self) -> StateSchemaCapabilities<'_>;
	fn support(
		&self,
		scope: &ScopeIdentity,
		role: PayloadRole,
	) -> Result<PayloadRoleSupport, ManagedPayloadError>;
	fn bind_final_candidate(
		&self,
		intrinsic: &PrimitiveScopeDeclaration<'_, '_>,
		policy: ScopePolicy,
	) -> Result<CandidateScopePolicy, ManagedPayloadError>;
	fn prepare_selected_writer_conversion(
		&self,
		scope: &ScopeIdentity,
		policy: &ScopePolicy,
		record: ManagedPayloadRecord<'_>,
	) -> Result<FleetPayload, ManagedPayloadError>;
}
impl<T, C> TypedManagedPayloadSchema<T, C> {
	fn declaration<'a>(
		&'a self,
		scope: &'a ScopeIdentity,
	) -> PayloadSchemaDeclaration<'a, 'a, T, C> {
		PayloadSchemaDeclaration::new(PayloadSchemaDeclarationFields {
			scope,
			schema: &self.schema,
		})
	}
	fn failure_context(
		&self,
		policy: Option<&ScopePolicy>,
		operation: ManagedPayloadOperation,
		stage: ManagedPayloadFailureStage,
	) -> Box<ManagedPayloadFailureContext> {
		let selected_writer = policy.and_then(|policy| match policy.owner() {
			ScopeOwner::Primitive(primitive) => primitive
				.payload()
				.schema()
				.ok()
				.map(|schema| schema.writer()),
			_ => None,
		});
		Box::new(ManagedPayloadFailureContext {
			operation,
			stage,
			selected_writer,
		})
	}
	fn failure(
		&self,
		policy: Option<&ScopePolicy>,
		operation: ManagedPayloadOperation,
		stage: ManagedPayloadFailureStage,
		source: ManagedPayloadCause,
	) -> ManagedPayloadError {
		ManagedPayloadError {
			context: self.failure_context(policy, operation, stage),
			source,
		}
	}
}
impl<T, C> ErasedManagedPayload for TypedManagedPayloadSchema<T, C>
where
	Self: Send + Sync,
	T: 'static,
	C: Send + Sync + 'static,
{
	fn as_any(&self) -> &dyn Any {
		self
	}
	fn capabilities(&self) -> StateSchemaCapabilities<'_> {
		self.schema.capabilities()
	}
	fn support(
		&self,
		scope: &ScopeIdentity,
		role: PayloadRole,
	) -> Result<PayloadRoleSupport, ManagedPayloadError> {
		self.declaration(scope).support(role).map_err(|error| {
			self.failure(
				None,
				ManagedPayloadOperation::DeclareRole(role),
				ManagedPayloadFailureStage::Binding,
				ManagedPayloadCause::Scope(error),
			)
		})
	}
	fn bind_final_candidate(
		&self,
		intrinsic: &PrimitiveScopeDeclaration<'_, '_>,
		policy: ScopePolicy,
	) -> Result<CandidateScopePolicy, ManagedPayloadError> {
		let scope = intrinsic.identity();
		let payload = self.declaration(scope);
		let context = self.failure_context(
			Some(&policy),
			ManagedPayloadOperation::BindFinalCandidate,
			ManagedPayloadFailureStage::Binding,
		);
		CandidateScopePolicy::from_payload_primitive(intrinsic, &payload, policy).map_err(|error| {
			ManagedPayloadError {
				context,
				source: ManagedPayloadCause::Candidate(error),
			}
		})
	}
	fn prepare_selected_writer_conversion(
		&self,
		scope: &ScopeIdentity,
		policy: &ScopePolicy,
		record: ManagedPayloadRecord<'_>,
	) -> Result<FleetPayload, ManagedPayloadError> {
		let operation = ManagedPayloadOperation::SelectedWriterConversion;
		let fail = |stage, source| self.failure(Some(policy), operation, stage, source);
		if record.identity.namespace() != scope.namespace() {
			return Err(fail(
				ManagedPayloadFailureStage::RecordIdentity,
				ManagedPayloadCause::Scope(ScopeError::ScopeMismatch),
			));
		}
		let declaration = self.declaration(scope);
		let reader = declaration.bind_reader(scope, policy).map_err(|error| {
			fail(
				ManagedPayloadFailureStage::Binding,
				ManagedPayloadCause::Scope(error),
			)
		})?;
		let producer = declaration.bind_producer(scope, policy).map_err(|error| {
			fail(
				ManagedPayloadFailureStage::Binding,
				ManagedPayloadCause::Scope(error),
			)
		})?;
		let value = reader.decode(record.payload).map_err(|_| {
			fail(
				ManagedPayloadFailureStage::RetainedDecode,
				ManagedPayloadCause::App,
			)
		})?;
		producer.encode(&value).map_err(|_| {
			fail(
				ManagedPayloadFailureStage::SelectedEncoding,
				ManagedPayloadCause::App,
			)
		})
	}
}
/// Sealed retained application schema code; construction grants no installed policy or role.
pub(crate) struct ManagedPayloadRegistration {
	code: Arc<dyn ErasedManagedPayload>,
}
impl ManagedPayloadRegistration {
	/// The typed schema, when this code holds payloads of type `T` with codec failures `C`.
	pub(crate) fn typed_schema<T: 'static, C: 'static>(&self) -> Option<&Arc<StateSchema<T, C>>> {
		self.code
			.as_any()
			.downcast_ref::<TypedManagedPayloadSchema<T, C>>()
			.map(|code| &code.schema)
	}
	pub(crate) fn capabilities(&self) -> StateSchemaCapabilities<'_> {
		self.code.capabilities()
	}
	pub(crate) fn matches_capabilities(&self, actual: StateSchemaCapabilities<'_>) -> bool {
		self.capabilities().is_same_declaration(actual)
	}
	pub(crate) fn support(
		&self,
		scope: &ScopeIdentity,
		role: PayloadRole,
	) -> Result<PayloadRoleSupport, ManagedPayloadError> {
		self.code.support(scope, role)
	}
	pub(crate) fn bind_final_candidate(
		&self,
		intrinsic: &PrimitiveScopeDeclaration<'_, '_>,
		policy: ScopePolicy,
	) -> Result<CandidateScopePolicy, ManagedPayloadError> {
		self.code.bind_final_candidate(intrinsic, policy)
	}
	pub(crate) fn prepare_selected_writer_conversion(
		&self,
		scope: &ScopeIdentity,
		policy: &ScopePolicy,
		record: ManagedPayloadRecord<'_>,
	) -> Result<FleetPayload, ManagedPayloadError> {
		self.code
			.prepare_selected_writer_conversion(scope, policy, record)
	}
}
impl fmt::Debug for ManagedPayloadRegistration {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("ManagedPayloadRegistration { .. }")
	}
}
