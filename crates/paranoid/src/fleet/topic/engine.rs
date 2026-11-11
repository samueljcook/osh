//! One admitted native Topic publication with actual retained core and scoped payload code.

use std::sync::Arc;

use crate::db::{WriteDatabase, run_transaction_write};
use crate::fleet::TopicKey;
use crate::fleet::payload::point::topic_handle::PgNativeTopicEvent;
use crate::fleet::payload::scope::topic::TopicEventScopeDeclaration;
use crate::fleet::scalar::FleetScalarCode;
use crate::installation::creation::PgNativeFleetScalarStorage;
use crate::kv::Ttl;
use crate::records::postgres::group_attempt::PgRecordGroupAttemptBudget;
use crate::runtime::admission::bindings::PgInstallationAdmissionBinding;
use crate::runtime::admission::payload::PayloadAdmissionFields;
use crate::runtime::admission::{NativeOperationError, PgNativeAdmission};
use crate::scope::payload::PayloadSchemaDeclaration;

mod error;
mod publication;
mod publication_lock;
mod subscription;
pub(in crate::fleet::topic) use error::NativeTopicCause;
pub(crate) use error::{
	NativeTopicConstructionCause, NativeTopicConstructionError, NativeTopicError,
};
use publication::PublicationFields;
pub(in crate::fleet::topic) use publication::{TopicPublication, TopicPublicationError};
use publication_lock::{TopicEventTarget, TopicSequenceTarget, TopicSequenceTargetFields};
pub(crate) use subscription::NativeTopicReadFields;

pub(crate) struct NativeTopicConstructionFields<'scope, 'code> {
	pub(crate) key: TopicKey,
	pub(crate) declaration: TopicEventScopeDeclaration<'scope, 'code>,
	pub(crate) event_ttl: Ttl,
}
pub(crate) struct NativeTopicFields<'scope, 'code> {
	pub(crate) key: TopicKey,
	pub(crate) event: PgNativeTopicEvent<'scope, 'code>,
	pub(crate) code: Arc<FleetScalarCode>,
	pub(crate) admission: Arc<PgNativeAdmission>,
	pub(crate) event_ttl: Ttl,
	pub(crate) budget: PgRecordGroupAttemptBudget,
}
pub(crate) struct NativeTopicPublishFields<'a, 'scope, 'code, T, C> {
	pub(crate) payload: &'a PayloadSchemaDeclaration<'scope, 'code, T, C>,
	pub(crate) value: &'a T,
}
/// Retained handles contain no runtime lifecycle owner, current phase or private acquisition.
pub(crate) struct PgNativeTopic<'scope, 'code> {
	key: TopicKey,
	event: PgNativeTopicEvent<'scope, 'code>,
	code: Arc<FleetScalarCode>,
	placement: PgNativeFleetScalarStorage,
	admission: Arc<PgNativeAdmission>,
	event_ttl: Ttl,
	budget: PgRecordGroupAttemptBudget,
}
impl<'scope, 'code> PgNativeTopic<'scope, 'code> {
	pub(crate) fn new(
		fields: NativeTopicFields<'scope, 'code>,
	) -> Result<Self, NativeTopicConstructionError> {
		let scope = fields.event.scope();
		if !fields.event.shares_admission(&fields.admission) {
			return Err(NativeTopicConstructionError::new(
				NativeTopicConstructionCause::Association,
			));
		}
		let native = match fields.admission.installation_binding().as_ref() {
			PgInstallationAdmissionBinding::Fresh { native }
			| PgInstallationAdmissionBinding::Imported { native, .. } => native,
		};
		let placement = native.fleet_scalar();
		if placement.storage().shares_container(fields.event.storage())
			&& placement.topic_sequence() == scope.namespace()
		{
			return Err(NativeTopicConstructionError::new(
				NativeTopicConstructionCause::OverlappingPlacement,
			));
		}
		Ok(Self {
			key: fields.key,
			event: fields.event,
			code: fields.code,
			placement: placement.clone(),
			admission: fields.admission,
			event_ttl: fields.event_ttl,
			budget: fields.budget,
		})
	}
	pub(crate) fn key(&self) -> &TopicKey {
		&self.key
	}
	fn failure<C>(&self, source: NativeTopicCause<C>) -> NativeTopicError<C> {
		NativeTopicError::new(self.event.scope(), &self.key, source)
	}

	/// Pool-owned success follows known commit; a joined transaction's result remains tentative.
	pub(crate) async fn publish_value<T, C>(
		&self,
		database: impl WriteDatabase,
		fields: NativeTopicPublishFields<'_, '_, '_, T, C>,
	) -> Result<TopicPublication, NativeTopicError<C>> {
		run_transaction_write(database, "fleet.native_topic.publish_value", async |tx| {
			self.admission
				.execute_write_with_contracts(tx, async |contracts, tx| {
					let writer = contracts
						.bind_topic_event_writer(self.event.declaration())
						.map_err(NativeOperationError::Admission)?;
					let declaration = self.code.topic.declaration();
					let component = contracts
						.bind(&declaration)
						.map_err(NativeOperationError::Admission)?;
					let sequence = TopicSequenceTarget::new(TopicSequenceTargetFields {
						fleet_scalar: &self.placement,
						component: &self.code.topic,
						contract: &component,
					})
					.map_err(|error| {
						NativeOperationError::Operation(
							self.failure(NativeTopicCause::Sequence(error)),
						)
					})?;
					let schema = self.code.topic.bind_sequence(&component).map_err(|error| {
						NativeOperationError::Operation(
							self.failure(NativeTopicCause::Sequence(error)),
						)
					})?;
					let payload = {
						let NativeTopicPublishFields {
							payload: declaration,
							value,
						} = fields;
						self.event
							.require_payload_declaration(declaration)
							.map_err(|error| {
								NativeOperationError::Operation(
									self.failure(NativeTopicCause::Scope(error)),
								)
							})?;
						let producer = contracts
							.bind_payload_producer(PayloadAdmissionFields {
								actual_scope: self.event.scope(),
								declaration,
							})
							.map_err(NativeOperationError::Admission)?;
						producer.require_declaration(declaration).map_err(|error| {
							NativeOperationError::Operation(
								self.failure(NativeTopicCause::Scope(error)),
							)
						})?;
						match writer.prepare_payload_with_producer(&producer, value) {
							Ok(payload) => payload,
							Err(source) => {
								return Ok(Err(self.failure(NativeTopicCause::Payload(source))));
							}
						}
					};
					let result = publication::publish(PublicationFields {
						tx,
						sequence,
						sequence_schema: &schema,
						events: TopicEventTarget {
							storage: self.event.storage(),
							namespace: self.event.scope().namespace(),
						},
						topic: &self.key,
						budget: self.budget,
						ttl: self.event_ttl,
						payload,
					})
					.await;
					Ok(result.map_err(|error| self.failure(NativeTopicCause::Publication(error))))
				})
				.await
				.map_err(NativeTopicError::from_operation)?
		})
		.await
		.map_err(|error: NativeTopicError<C>| {
			error.with_context_if_absent(self.event.scope(), &self.key)
		})
	}
}
