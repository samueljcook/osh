//! Borrowed actual Topic event reads; publication and subscription sequencing are separate operations.

use super::factory::{
	NativePayloadConstructionError, require_retained_payload, retained_point_code,
};
use crate::fleet::payload::scope::retained::RetainedTopicEventScopeCode;
use crate::fleet::payload::scope::topic::TopicEventScopeDeclaration;
use crate::records::postgres::PgPointStorage;
use crate::runtime::admission::PgNativeAdmission;
use crate::scope::managed::ManagedScopeRegistry;
use crate::scope::payload::PayloadSchemaDeclaration;
use std::sync::Arc;

pub(crate) struct NativeTopicEventFields<'scope, 'code, 'registry> {
	pub(crate) declaration: TopicEventScopeDeclaration<'scope, 'code>,
	pub(crate) registry: &'registry ManagedScopeRegistry,
	pub(crate) admission: Arc<PgNativeAdmission>,
}
pub(crate) struct PgNativeTopicEvent<'scope, 'code> {
	declaration: TopicEventScopeDeclaration<'scope, 'code>,
	retained: Option<Arc<RetainedTopicEventScopeCode>>,
	storage: PgPointStorage,
	admission: Arc<PgNativeAdmission>,
}
impl<'scope, 'code> PgNativeTopicEvent<'scope, 'code> {
	pub(crate) fn new(
		fields: NativeTopicEventFields<'scope, 'code, '_>,
	) -> Result<Self, NativePayloadConstructionError> {
		let retained = retained_point_code(fields.registry, &fields.declaration)?;
		let storage = fields
			.admission
			.app_storage(fields.declaration.identity())
			.map_err(NativePayloadConstructionError::admission)?;
		Ok(Self {
			declaration: fields.declaration,
			retained,
			storage,
			admission: fields.admission,
		})
	}
	pub(in crate::fleet) fn scope(&self) -> &crate::scope::ScopeIdentity {
		self.declaration.identity()
	}
	pub(in crate::fleet) fn declaration(&self) -> &TopicEventScopeDeclaration<'scope, 'code> {
		&self.declaration
	}
	pub(in crate::fleet) fn storage(&self) -> &PgPointStorage {
		&self.storage
	}
	pub(in crate::fleet) fn shares_admission(&self, admission: &PgNativeAdmission) -> bool {
		std::ptr::eq(self.admission.as_ref(), admission)
	}
	pub(in crate::fleet) fn require_payload_declaration<T, C>(
		&self,
		declaration: &PayloadSchemaDeclaration<'_, '_, T, C>,
	) -> Result<(), crate::scope::ScopeError> {
		require_retained_payload(
			self.retained.as_ref().map(|code| code.payload_code()),
			declaration,
		)
	}
}
