//! Pure Queue group preparation under exact immutable codec and semantic bindings.
//!
//! Prepared actions are not admission, held-lock, source-retirement, or commit proofs.
//! An owning adapter must acquire the whole identity set and apply every action atomically.

use super::auxiliary_state::{
	ActiveDedupeState, AuxiliaryRepresentationError, AuxiliarySchemaBinding, AuxiliaryStateError,
	DeadLetterOriginalState, NativeDeadLetterState, QueueAuxiliarySchemas, QueuePauseScope,
	QueuePauseState,
};
use super::record_transitions::{OwnedJobMutation, StaleClaimRecoveryKind};
use super::state::{
	BoundJobTransitionError, JobLifecycle, NativeJob, NativeJobMetadata, NativeJobParts,
	NativeJobPayload, NativeJobPayloadRecord, QueueJobBindingError, QueueJobComponent,
	QueueJobSchemaBinding, QueueRepresentationError,
};
use super::{ClaimedJob, DeadLetterJob, DeadLetterReason, Job, JobId, JobRunAtOrAfter, JobStatus};
use crate::installation::contract::{
	BoundComponentContract, BoundStateSchema, ComponentDeclaration, ComponentDeclarationFields,
	ComponentPolicy, ComponentPolicyFields, ComponentSettings, ComponentSettingsDeclaration,
	ComponentSettingsRequirement, SemanticContractDef, StateContractError, StateStorageEnvelope,
};
use crate::records::{
	InvalidGroupMutationError, RecordGroupRead, RecordIdentity, RecordKey, RecordMetadata,
	RecordMutation, RecordMutationPlan, RecordNamespace, StateTimestampMicros,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

mod claim;
mod claim_check;
pub(in crate::queue) use claim_check::NativeClaimCheckRequest;
mod dedupe;
pub(in crate::queue) use claim::{ClaimPauses, NativeClaimRequest};
mod enqueue;
pub(crate) use enqueue::NativeEnqueueRequest;
pub(crate) use enqueue::{EnqueuePauses, NativeEnqueueRejection};
mod dead_letter;
mod operator;
mod pause;
mod relocation;
pub(crate) use relocation::{QueueRelocationRecord, QueueSourceRelocationOutcome};
mod snapshot;
pub(in crate::queue) use pause::PauseAction;
pub(super) use snapshot::QueueGroupSnapshot;
pub(super) use snapshot::{QueueSourceFacts, SourceProbe, SourceRecord};

const COMPONENT: &str = "paranoid.queue.auxiliary";
const SEMANTICS: &str = "paranoid.queue.auxiliary.reconcile-dead-letter-pause.v1";

/// Actual implementation and the four immutable registries that it uses.
pub(crate) struct QueueAuxiliaryComponent {
	schemas: QueueAuxiliarySchemas,
}

/// Explicit mappings; physical container association belongs to installation binding.
pub(super) struct QueueGroupNamespaceFields {
	pub(super) jobs: RecordNamespace,
	pub(super) dedupe: RecordNamespace,
	pub(super) dead_letters: RecordNamespace,
	pub(super) originals: RecordNamespace,
	pub(super) pauses: RecordNamespace,
}
pub(super) struct QueueGroupNamespaces {
	fields: QueueGroupNamespaceFields,
}
impl QueueGroupNamespaces {
	pub(super) fn new(fields: QueueGroupNamespaceFields) -> Result<Self, QueueGroupError> {
		let values = [
			&fields.jobs,
			&fields.dedupe,
			&fields.dead_letters,
			&fields.originals,
			&fields.pauses,
		];
		for (index, value) in values.iter().enumerate() {
			if values[..index].contains(value) {
				return Err(QueueGroupError::NamespaceOverlap);
			}
		}
		Ok(Self { fields })
	}
}

/// Actual component instances and selected schema/semantic policies for one Queue placement.
pub(super) struct BoundQueueGroup<'a, 'd, 's> {
	component: &'a QueueAuxiliaryComponent,
	auxiliary_contract: &'a BoundComponentContract<'d, 's, QueueAuxiliaryComponent>,
	jobs: &'a QueueJobComponent,
	job_contract: &'a BoundComponentContract<'d, 's, QueueJobComponent>,
	namespaces: &'a QueueGroupNamespaces,
}
impl QueueAuxiliaryComponent {
	pub(crate) fn component_identifier() -> &'static str {
		COMPONENT
	}

	pub(crate) fn new() -> Self {
		Self {
			schemas: QueueAuxiliarySchemas::new(),
		}
	}
	pub(in crate::queue) fn schemas(&self) -> &QueueAuxiliarySchemas {
		&self.schemas
	}
	pub(crate) fn declaration(&self) -> ComponentDeclaration<'_, Self> {
		ComponentDeclaration::new(ComponentDeclarationFields {
			identifier: Self::component_identifier(),
			fixed_schemas: vec![
				self.schemas.active_dedupe_schema().capabilities(),
				self.schemas.dead_letter_schema().capabilities(),
				self.schemas.original_schema().capabilities(),
				self.schemas.pause_schema().capabilities(),
			],
			settings: ComponentSettingsDeclaration::Parameterless,
			semantics: vec![SemanticContractDef {
				identifier: SEMANTICS,
				transitions: self,
				settings: ComponentSettingsRequirement::Parameterless,
			}],
		})
		.expect("Queue auxiliary semantics use four distinct actual registries")
	}

	/// Selects the actual group semantics and every initial native auxiliary writer.
	pub(crate) fn initial_policy(&self) -> Result<ComponentPolicy, QueueGroupError> {
		let policy = ComponentPolicy::new(ComponentPolicyFields {
			component_identifier: Self::component_identifier().to_owned(),
			semantic_contract: SEMANTICS.to_owned(),
			schemas: self.schemas.initial_schema_policies()?,
			settings: ComponentSettings::Parameterless,
		})?;
		let declaration = self.declaration();
		self.require_bound_policy(&declaration.bind(&policy)?)?;
		Ok(policy)
	}
	pub(super) fn bind<'a, 'd, 's>(
		&'a self,
		auxiliary_contract: &'a BoundComponentContract<'d, 's, Self>,
		jobs: &'a QueueJobComponent,
		job_contract: &'a BoundComponentContract<'d, 's, QueueJobComponent>,
		namespaces: &'a QueueGroupNamespaces,
	) -> Result<BoundQueueGroup<'a, 'd, 's>, QueueGroupError> {
		self.require_bound_policy(auxiliary_contract)?;
		jobs.require_bound_policy(job_contract)?;
		Ok(BoundQueueGroup {
			component: self,
			auxiliary_contract,
			jobs,
			job_contract,
			namespaces,
		})
	}
	/// Checks this exact implementation and every selected auxiliary writer without record keys.
	pub(crate) fn require_bound_policy(
		&self,
		auxiliary_contract: &BoundComponentContract<'_, '_, Self>,
	) -> Result<(), QueueGroupError> {
		if !std::ptr::eq(self, auxiliary_contract.transitions())
			|| auxiliary_contract.policy().semantic_contract() != SEMANTICS
		{
			return Err(QueueGroupError::ComponentInstanceMismatch);
		}
		require_native_writer(
			&auxiliary_contract.bind_schema(self.schemas.active_dedupe_schema())?,
		)?;
		require_native_writer(&auxiliary_contract.bind_schema(self.schemas.dead_letter_schema())?)?;
		require_native_writer(&auxiliary_contract.bind_schema(self.schemas.original_schema())?)?;
		require_native_writer(&auxiliary_contract.bind_schema(self.schemas.pause_schema())?)?;
		Ok(())
	}
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum QueueGroupError {
	#[error("Queue group record preparation failed at {identity:?}")]
	RecordPreparation {
		identity: RecordIdentity,
		#[source]
		error: Box<Self>,
	},
	#[error("Queue group namespaces must be distinct")]
	NamespaceOverlap,
	#[error("Queue group implementation belongs to a different component declaration")]
	ComponentInstanceMismatch,
	#[error("Queue group schema policy cannot bind")]
	Contract(#[from] StateContractError),
	#[error("Queue group job policy cannot bind")]
	JobBinding(#[from] QueueJobBindingError),
	#[error("Queue group job representation failed")]
	JobRepresentation(#[from] QueueRepresentationError),
	#[error("Queue group job transition failed")]
	JobTransition(#[from] BoundJobTransitionError),
	#[error("Queue group auxiliary representation failed")]
	AuxiliaryRepresentation(#[from] AuxiliaryRepresentationError),
	#[error("Queue group auxiliary state failed")]
	AuxiliaryState(#[from] AuxiliaryStateError),
	#[error("Queue group action plan failed")]
	Plan(#[from] InvalidGroupMutationError),
	#[error("Queue group requires another identity {identity:?}")]
	RequiredIdentity { identity: RecordIdentity },
	#[error("Queue group source coverage is unknown for {identity:?}")]
	SourceUnknown { identity: RecordIdentity },
	#[error("Queue group state is inconsistent: {reason}")]
	Inconsistent { reason: &'static str },
	#[error("Queue group new identity already contains a physical value")]
	IdentityCollision,
	#[error("Queue group active dedupe conflicts with another job")]
	DedupeConflict,
	#[error("Queue group original job already has a current dead letter")]
	OriginalConflict,
}

impl QueueGroupError {
	pub(in crate::queue) fn record_identity(&self) -> Option<&RecordIdentity> {
		match self {
			Self::RecordPreparation { identity, .. }
			| Self::RequiredIdentity { identity }
			| Self::SourceUnknown { identity } => Some(identity),
			_ => None,
		}
	}
}
fn at_record(identity: RecordIdentity, error: impl Into<QueueGroupError>) -> QueueGroupError {
	QueueGroupError::RecordPreparation {
		identity,
		error: Box::new(error.into()),
	}
}

/// A source deletion names only an observed held logical record, never a raw SQL target.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum QueueSourceRole {
	Job,
	DeadLetter,
	Pause,
}
pub(super) struct QueueSourceDeleteIntent {
	pub(super) role: QueueSourceRole,
	pub(super) identity: RecordIdentity,
}
impl fmt::Debug for QueueSourceDeleteIntent {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("QueueSourceDeleteIntent")
			.field("role", &self.role)
			.field("identity", &self.identity)
			.finish()
	}
}

/// Diagnostic results contain no usable claim or private working state.
pub(super) enum PreparedQueueGroupOutcome {
	JobOperator(super::state::QueueJobOperatorOutcome),
	SourceRelocation(QueueSourceRelocationOutcome),
	Reconciled,
	JobDeleted,
	DeadLetterMoved { dead_letter_id: JobId },
	DeadLetterDeleted,
	DeadLetterRequeued { job_id: JobId },
	PauseChanged,
	Enqueued { job_id: JobId, deduplicated: bool },
	EnqueueRejected(NativeEnqueueRejection),
	Claimed { job: Box<Job> },
	ClaimSkipped,
	ClaimNotReturned,
	ClaimReturned,
}
impl fmt::Debug for PreparedQueueGroupOutcome {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(match self {
			Self::JobOperator(_) => "JobOperator { .. }",
			Self::SourceRelocation(QueueSourceRelocationOutcome::Moved) => {
				"SourceRelocation::Moved"
			}
			Self::SourceRelocation(QueueSourceRelocationOutcome::AlreadyNative) => {
				"SourceRelocation::AlreadyNative"
			}
			Self::SourceRelocation(QueueSourceRelocationOutcome::AbsentNow) => {
				"SourceRelocation::AbsentNow"
			}
			Self::Reconciled => "Reconciled",
			Self::JobDeleted => "JobDeleted",
			Self::DeadLetterMoved { .. } => "DeadLetterMoved { .. }",
			Self::DeadLetterDeleted => "DeadLetterDeleted",
			Self::DeadLetterRequeued { .. } => "DeadLetterRequeued { .. }",
			Self::PauseChanged => "PauseChanged",
			Self::Enqueued { .. } => "Enqueued { .. }",
			Self::EnqueueRejected(_) => "EnqueueRejected { .. }",
			Self::Claimed { .. } => "Claimed { .. }",
			Self::ClaimSkipped => "ClaimSkipped",
			Self::ClaimNotReturned => "ClaimNotReturned",
			Self::ClaimReturned => "ClaimReturned",
		})
	}
}
pub(super) struct PreparedQueueGroupPlan {
	pub(super) native: RecordMutationPlan,
	pub(super) source_deletes: Vec<QueueSourceDeleteIntent>,
	pub(super) result: PreparedQueueGroupOutcome,
}
pub(super) enum QueueGroupPreparation {
	Prepared(PreparedQueueGroupPlan),
	Unchanged,
	RequiresReacquisition(BTreeSet<RecordIdentity>),
}

/// An explicit operator requeue schedule; pauses do not prohibit this operation.
#[derive(Clone, Copy)]
pub(in crate::queue) enum QueueRequeueSchedule {
	ObservedNow,
	At(JobRunAtOrAfter),
}
pub(super) struct DeadLetterTarget {
	pub(super) dead_letter_id: JobId,
	pub(super) reason: DeadLetterReason,
}
pub(super) struct DeadLetterRequeueTarget {
	pub(super) job_id: JobId,
	pub(super) schedule: QueueRequeueSchedule,
}
pub(super) struct StaleClaimRecoveryRequest {
	pub(super) job_id: JobId,
	pub(super) dead_letter_id: JobId,
}
struct DeadLetterMove {
	before: NativeJob,
	origin: snapshot::Origin,
	failed: NativeJob,
	target: DeadLetterTarget,
}

struct PlanBuilder {
	actions: Vec<(RecordIdentity, RecordMutation)>,
	deletes: Vec<QueueSourceDeleteIntent>,
}
impl PlanBuilder {
	fn new() -> Self {
		Self {
			actions: Vec::new(),
			deletes: Vec::new(),
		}
	}
	fn action(&mut self, identity: RecordIdentity, action: RecordMutation) {
		self.actions.push((identity, action));
	}
	fn remove_source(
		&mut self,
		identity: RecordIdentity,
		origin: snapshot::Origin,
		role: QueueSourceRole,
	) {
		if origin == snapshot::Origin::Source {
			self.deletes
				.push(QueueSourceDeleteIntent { role, identity });
		}
	}
	fn finish(
		self,
		snapshot: &QueueGroupSnapshot<'_>,
		result: PreparedQueueGroupOutcome,
	) -> Result<QueueGroupPreparation, QueueGroupError> {
		let native = RecordMutationPlan::prepare(snapshot.native, self.actions)?;
		Ok(QueueGroupPreparation::Prepared(PreparedQueueGroupPlan {
			native,
			source_deletes: self.deletes,
			result,
		}))
	}
}
fn preparation(
	result: Result<QueueGroupPreparation, QueueGroupError>,
) -> Result<QueueGroupPreparation, QueueGroupError> {
	match result {
		Err(QueueGroupError::RequiredIdentity { identity }) => Ok(
			QueueGroupPreparation::RequiresReacquisition(BTreeSet::from([identity])),
		),
		other => other,
	}
}
fn inconsistent(reason: &'static str) -> QueueGroupError {
	QueueGroupError::Inconsistent { reason }
}
fn require_native_writer<T>(
	schema: &BoundStateSchema<'_, T, AuxiliaryStateError>,
) -> Result<(), QueueGroupError> {
	if schema.policy().writer().envelope != StateStorageEnvelope::NativeJson {
		return Err(AuxiliaryRepresentationError::NativeWriterRequired.into());
	}
	Ok(())
}
fn encode_auxiliary<T: super::auxiliary_state::AuxiliaryRecord>(
	binding: &AuxiliarySchemaBinding<'_, T>,
	state: &T,
	metadata: RecordMetadata,
) -> Result<crate::records::EncodedRecord, QueueGroupError> {
	binding
		.encode_record(state, metadata)
		.map_err(|error| at_record(binding.identity().clone(), error))
}
/// Whether the record time `at` lies more than `retention_micros` before the database time
/// `observed_at`, which cleanup requires before it deletes a record.
fn older_than(at: i64, observed_at: StateTimestampMicros, retention_micros: i64) -> bool {
	at < observed_at.as_i64().saturating_sub(retention_micros)
}

fn metadata(now: StateTimestampMicros) -> RecordMetadata {
	RecordMetadata {
		expires_at: None,
		updated_at: now,
	}
}

impl BoundQueueGroup<'_, '_, '_> {
	fn payload_identity(&self, id: JobId) -> RecordIdentity {
		RecordIdentity::new(
			self.namespaces.fields.jobs.clone(),
			QueueJobSchemaBinding::payload_record_key(id),
		)
	}

	fn public_job(
		&self,
		snapshot: &QueueGroupSnapshot<'_>,
		state: &NativeJob,
	) -> Result<Job, QueueGroupError> {
		let id = state.job().id;
		let payload = match state.payload() {
			NativeJobPayload::InlineJson(_) => None,
			NativeJobPayload::SeparateRecord => snapshot.held(&self.payload_identity(id))?.record(),
		};
		self.job_binding(id)?
			.public_job(state, payload)
			.map_err(|error| at_record(self.job_identity(id), error))
	}

	fn plan_job_write(
		&self,
		snapshot: &QueueGroupSnapshot<'_>,
		state: &NativeJob,
		plan: &mut PlanBuilder,
	) -> Result<(), QueueGroupError> {
		let id = state.job().id;
		let binding = self.job_binding(id)?;
		if binding.writes_separate_payload()
			&& let NativeJobPayload::InlineJson(json) = state.payload()
		{
			let identity = self.payload_identity(id);
			if snapshot.held(&identity)?.record().is_some() {
				return Err(inconsistent(
					"an inline job already has a separate payload record",
				));
			}
			let encoded = binding
				.encode_payload_record(NativeJobPayloadRecord {
					job_id: id,
					created_at_unix_micros: state.job().created_at_unix_micros,
					payload_json: json.clone(),
				})
				.map_err(|error| at_record(identity.clone(), error))?;
			plan.action(identity, RecordMutation::Write(encoded));
		}
		plan.action(
			self.job_identity(id),
			RecordMutation::Write(self.encode_job(state)?),
		);
		Ok(())
	}

	fn plan_job_delete(
		&self,
		snapshot: &QueueGroupSnapshot<'_>,
		state: &NativeJob,
		plan: &mut PlanBuilder,
	) -> Result<(), QueueGroupError> {
		let id = state.job().id;
		if matches!(state.payload(), NativeJobPayload::SeparateRecord) {
			let identity = self.payload_identity(id);
			let record = snapshot
				.held(&identity)?
				.record()
				.ok_or_else(|| inconsistent("job payload record is missing during deletion"))?;
			self.job_binding(id)?
				.decode_payload_record(record)
				.map_err(|error| at_record(identity.clone(), error))?;
			plan.action(identity, RecordMutation::Delete);
		}
		plan.action(self.job_identity(id), RecordMutation::Delete);
		Ok(())
	}

	fn job_identity(&self, id: JobId) -> RecordIdentity {
		RecordIdentity::new(
			self.namespaces.fields.jobs.clone(),
			RecordKey::new(id.as_bytes().to_vec()),
		)
	}
	fn encode_job(
		&self,
		state: &NativeJob,
	) -> Result<crate::records::EncodedRecord, QueueGroupError> {
		let id = state.job().id;
		self.job_binding(id)?
			.encode_record(state)
			.map_err(|error| at_record(self.job_identity(id), error))
	}
	fn job_binding(&self, id: JobId) -> Result<QueueJobSchemaBinding<'_>, QueueGroupError> {
		Ok(self.jobs.bind_schema(self.job_contract, id)?)
	}
	fn dedupe_binding(
		&self,
		task_name: &super::TaskName,
		dedupe_key: &super::DedupeKey,
	) -> Result<AuxiliarySchemaBinding<'_, ActiveDedupeState>, QueueGroupError> {
		Ok(self.component.schemas.bind_active_dedupe(
			self.auxiliary_contract,
			self.namespaces.fields.dedupe.clone(),
			task_name,
			dedupe_key,
		)?)
	}
	fn dead_letter_binding(
		&self,
		id: JobId,
	) -> Result<AuxiliarySchemaBinding<'_, NativeDeadLetterState>, QueueGroupError> {
		Ok(self.component.schemas.bind_dead_letter(
			self.auxiliary_contract,
			self.namespaces.fields.dead_letters.clone(),
			id,
		)?)
	}
	fn original_binding(
		&self,
		id: JobId,
	) -> Result<AuxiliarySchemaBinding<'_, DeadLetterOriginalState>, QueueGroupError> {
		Ok(self.component.schemas.bind_original(
			self.auxiliary_contract,
			self.namespaces.fields.originals.clone(),
			id,
		)?)
	}
	fn pause_binding(
		&self,
		scope: &QueuePauseScope,
	) -> Result<AuxiliarySchemaBinding<'_, QueuePauseState>, QueueGroupError> {
		Ok(self.component.schemas.bind_pause(
			self.auxiliary_contract,
			self.namespaces.fields.pauses.clone(),
			scope,
		)?)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn cleanup_keeps_a_record_exactly_as_old_as_its_retention() {
		let observed_at = StateTimestampMicros::new(1_000_000);
		assert!(!older_than(999_000, observed_at, 1_000));
		assert!(older_than(998_999, observed_at, 1_000));
		assert!(!older_than(999_001, observed_at, 1_000));
		assert!(!older_than(1_000_000, observed_at, 0));
		assert!(older_than(999_999, observed_at, 0));
	}
}
