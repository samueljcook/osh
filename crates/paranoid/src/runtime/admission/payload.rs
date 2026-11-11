//! Current active intrinsic and payload roles for fixed native Cache/Topic operations.
//!
//! Native cache and Topic scopes run on every storage route: v11 starts them empty and never
//! shares their records with v10.

use crate::fleet::payload::scope::PayloadScopeError;
use crate::fleet::payload::scope::cache::{
	BoundCacheEntryScopeReader, BoundCacheEntryScopeWriter, CacheEntryScopeDeclaration,
};
use crate::fleet::payload::scope::topic::{
	BoundTopicEventScopeReader, BoundTopicEventScopeWriter, TopicEventScopeDeclaration,
};
use crate::fleet::primitive_scope::PrimitiveScopeBindingError;
use crate::scope::payload::{
	BoundPayloadProducer, BoundPayloadReader, PayloadRole, PayloadSchemaDeclaration,
};
use crate::scope::{PrimitiveRole, ScopeIdentity, ScopePolicy};

use super::{NativeAdmissionError, NativeAdmittedContracts};

pub(crate) struct PayloadAdmissionFields<'a, 'scope, 'code, T, C> {
	pub(crate) actual_scope: &'a ScopeIdentity,
	pub(crate) declaration: &'a PayloadSchemaDeclaration<'scope, 'code, T, C>,
}

impl NativeAdmittedContracts {
	pub(crate) fn bind_cache_entry_reader<'a, 'scope, 'code>(
		&'a self,
		declaration: &'a CacheEntryScopeDeclaration<'scope, 'code>,
	) -> Result<BoundCacheEntryScopeReader<'a, 'code>, NativeAdmissionError> {
		let policy = self.primitive_policy(declaration.identity(), PrimitiveRole::Reader)?;
		declaration
			.bind_reader(declaration.identity(), policy)
			.map_err(payload_binding_error)
	}
	pub(crate) fn bind_cache_entry_writer<'a, 'scope, 'code>(
		&'a self,
		declaration: &'a CacheEntryScopeDeclaration<'scope, 'code>,
	) -> Result<BoundCacheEntryScopeWriter<'a, 'code>, NativeAdmissionError> {
		let policy = self.primitive_policy(declaration.identity(), PrimitiveRole::Writer)?;
		declaration
			.bind_writer(declaration.identity(), policy)
			.map_err(payload_binding_error)
	}
	pub(crate) fn bind_topic_event_reader<'a, 'scope, 'code>(
		&'a self,
		declaration: &'a TopicEventScopeDeclaration<'scope, 'code>,
	) -> Result<BoundTopicEventScopeReader<'a, 'code>, NativeAdmissionError> {
		let policy = self.primitive_policy(declaration.identity(), PrimitiveRole::Reader)?;
		declaration
			.bind_reader(declaration.identity(), policy)
			.map_err(payload_binding_error)
	}
	pub(crate) fn bind_topic_event_writer<'a, 'scope, 'code>(
		&'a self,
		declaration: &'a TopicEventScopeDeclaration<'scope, 'code>,
	) -> Result<BoundTopicEventScopeWriter<'a, 'code>, NativeAdmissionError> {
		let policy = self.primitive_policy(declaration.identity(), PrimitiveRole::Writer)?;
		declaration
			.bind_writer(declaration.identity(), policy)
			.map_err(payload_binding_error)
	}
	fn payload_policy(
		&self,
		scope: &ScopeIdentity,
		role: PayloadRole,
	) -> Result<&ScopePolicy, NativeAdmissionError> {
		let policy = self
			.observation
			.control
			.state()
			.scope_policies()
			.lookup(scope)?;
		self.observation
			.participant()?
			.capabilities()
			.payload_roles()
			.lookup(scope, role)?
			.check_policy(policy)?;
		Ok(policy)
	}
	pub(crate) fn bind_payload_reader<'a, 'scope: 'a, 'code: 'a, T, C>(
		&'a self,
		fields: PayloadAdmissionFields<'a, 'scope, 'code, T, C>,
	) -> Result<BoundPayloadReader<'a, T, C>, NativeAdmissionError> {
		let policy = self.payload_policy(fields.actual_scope, PayloadRole::Reader)?;
		Ok(fields
			.declaration
			.bind_reader(fields.actual_scope, policy)?)
	}
	pub(crate) fn bind_payload_producer<'a, 'scope: 'a, 'code: 'a, T, C>(
		&'a self,
		fields: PayloadAdmissionFields<'a, 'scope, 'code, T, C>,
	) -> Result<BoundPayloadProducer<'a, T, C>, NativeAdmissionError> {
		let policy = self.payload_policy(fields.actual_scope, PayloadRole::Producer)?;
		Ok(fields
			.declaration
			.bind_producer(fields.actual_scope, policy)?)
	}
}

fn payload_binding_error(source: PayloadScopeError) -> NativeAdmissionError {
	NativeAdmissionError::Primitive(PrimitiveScopeBindingError::Payload(Box::new(source)))
}
