//! Closed per-acquisition accounting and whole-group pure preparation.
//!
//! Prepared actions require the same admitted backend group and known owned ending. Stored
//! anchors never construct grants, and timestamp expiry never removes an accounting record.

use std::{collections::BTreeSet, fmt};

use primitives_rs::random::secure_bytes::{RandomError, generate_random_byte_array};

use crate::fleet::throttler::model::ThrottlerTaskOutcome;
use crate::fleet::{AcquisitionCapability, HolderId, ThrottlerKey};
use crate::fleet::{
	private_capability_wire::PrivateCapabilityWireError, resource::NativeFleetResource,
};
use crate::installation::contract::{
	ContractStateReadError, RepresentationPolicy, SchemaPolicy, StateContractError,
	StateStorageEnvelope,
};
use crate::records::state_schema::{
	StateRepresentationFormat, StateSchema, StateSchemaError, StateVersion,
};
use crate::records::{
	EncodedRecord, IndexFields, RecordGroupRead, RecordIdentity, RecordMetadata, RecordMutation,
	StoredRecord,
};
use primitives_rs::serialization::json::JsonDecodeFailureSummary;

use super::{
	NativeThrottlerPermit, NativeThrottlerState, NativeThrottlerStateFields,
	TentativeThrottlerAcquisition, ThrottlerAcquireInput, ThrottlerAcquisitionPredicates,
	ThrottlerAcquisitionResult, ThrottlerMutationDecision, ThrottlerOutcomeInput,
	ThrottlerSchemaBinding, ThrottlerSettings, ThrottlerStateError,
};

mod wire;

const SCHEMA: &str = "paranoid.throttler.outcome";
const NATIVE_REPRESENTATION_VERSION: u32 = 1;

/// A non-authoritative random locator; only the anchor's private predicates authenticate use.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) struct ThrottlerOutcomeId([u8; 32]);
impl ThrottlerOutcomeId {
	fn generate() -> Result<Self, RandomError> {
		generate_random_byte_array::<32>().map(Self)
	}
	pub(in crate::fleet) fn as_bytes(&self) -> &[u8; 32] {
		&self.0
	}
}
impl fmt::Debug for ThrottlerOutcomeId {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("ThrottlerOutcomeId { .. }")
	}
}

pub(super) struct NativeThrottlerOutcomeAnchor {
	key: ThrottlerKey,
	outcome_id: ThrottlerOutcomeId,
	holder: HolderId,
	capability: AcquisitionCapability,
	epoch: AcquisitionCapability,
}
impl fmt::Debug for NativeThrottlerOutcomeAnchor {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("NativeThrottlerOutcomeAnchor { .. }")
	}
}

fn require_anchor_envelope(record: &StoredRecord) -> Result<(), ThrottlerGroupPreparationError> {
	if record.metadata().expires_at.is_some() || !record.index_fields().is_empty() {
		return Err(ThrottlerGroupPreparationError::AnchorEnvelope);
	}
	Ok(())
}

pub(super) fn schema(
	access: crate::fleet::primitive_scope::NativePrimitiveCodeAccess,
) -> StateSchema<NativeThrottlerOutcomeAnchor, ThrottlerOutcomeCodecError> {
	StateSchema::new(crate::records::state_schema::StateSchemaFields {
		identifier: SCHEMA,
		versions: vec![StateVersion {
			version: NATIVE_REPRESENTATION_VERSION,
			format: StateRepresentationFormat::Json,
			decoder: wire::decode,
			encoder: match access {
				crate::fleet::primitive_scope::NativePrimitiveCodeAccess::ReaderOnly => None,
				crate::fleet::primitive_scope::NativePrimitiveCodeAccess::ReaderWriter => {
					Some(wire::encode)
				}
			},
		}],
	})
	.expect("Throttler declares its actual initial private anchor codec")
}

pub(super) fn initial_schema_policy(
	schema: &StateSchema<NativeThrottlerOutcomeAnchor, ThrottlerOutcomeCodecError>,
) -> Result<SchemaPolicy, StateContractError> {
	let writer = RepresentationPolicy {
		version: NATIVE_REPRESENTATION_VERSION,
		envelope: StateStorageEnvelope::NativeJson,
	};
	SchemaPolicy::new(schema.identifier().to_owned(), writer, vec![writer])
}

/// Proposed identity and holder retained before group IO; this creates no acquisition.
pub(crate) struct ThrottlerAcquireProposal {
	rate_limit_cost: std::num::NonZeroU32,
	key: ThrottlerKey,
	settings: ThrottlerSettings,
	holder: Option<HolderId>,
	outcome_id: Option<ThrottlerOutcomeId>,
}
impl ThrottlerAcquireProposal {
	pub(crate) fn outcome_id(&self) -> Option<&ThrottlerOutcomeId> {
		self.outcome_id.as_ref()
	}
}
impl fmt::Debug for ThrottlerAcquireProposal {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("ThrottlerAcquireProposal { .. }")
	}
}

pub(crate) struct ThrottlerAcquireGroupFields<'a> {
	pub(crate) resource_identity: &'a RecordIdentity,
	pub(crate) snapshot: &'a RecordGroupRead,
	pub(crate) proposal: ThrottlerAcquireProposal,
}
pub(crate) struct ThrottlerAcquireScopeGroupFields<'a> {
	pub(crate) key: &'a ThrottlerKey,
	pub(crate) settings: &'a ThrottlerSettings,
	pub(crate) group: ThrottlerAcquireGroupFields<'a>,
}

/// Actual consumed outcome material retained across uncertain ending, without grant access.
pub(crate) struct ThrottlerPendingOutcome {
	predicates: ThrottlerAcquisitionPredicates,
	outcome: ThrottlerTaskOutcome,
}
impl ThrottlerPendingOutcome {
	pub(crate) fn outcome_id(&self) -> Option<&ThrottlerOutcomeId> {
		self.predicates.outcome_id()
	}
}
impl fmt::Debug for ThrottlerPendingOutcome {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("ThrottlerPendingOutcome { .. }")
	}
}
impl NativeThrottlerPermit {
	/// Consumes one actual grant into a fixed outcome purpose with no extension or grant
	/// conversion.
	pub(crate) fn into_pending_outcome(
		self,
		outcome: ThrottlerTaskOutcome,
	) -> ThrottlerPendingOutcome {
		ThrottlerPendingOutcome {
			predicates: self.predicates,
			outcome,
		}
	}
}
pub(crate) struct ThrottlerPendingOutcomeGroupFields<'a> {
	pub(crate) resource_identity: &'a RecordIdentity,
	pub(crate) snapshot: &'a RecordGroupRead,
	pub(crate) pending: &'a ThrottlerPendingOutcome,
}
pub(crate) struct ThrottlerPendingOutcomeScopeGroupFields<'a> {
	pub(crate) key: &'a ThrottlerKey,
	pub(crate) settings: &'a ThrottlerSettings,
	pub(crate) group: ThrottlerPendingOutcomeGroupFields<'a>,
}

/// Exact prepared acquisition material restricted to undelivered NotExecuted cleanup.
/// This non-Clone wrapper cannot construct a permit, report an outcome, or extend a probe.
pub(crate) struct ThrottlerUndeliveredTarget {
	predicates: ThrottlerAcquisitionPredicates,
}
impl ThrottlerUndeliveredTarget {
	pub(crate) fn outcome_id(&self) -> Option<&ThrottlerOutcomeId> {
		self.predicates.outcome_id()
	}
}
impl fmt::Debug for ThrottlerUndeliveredTarget {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("ThrottlerUndeliveredTarget { .. }")
	}
}
pub(crate) struct ThrottlerUndeliveredGroupFields<'a> {
	pub(crate) resource_identity: &'a RecordIdentity,
	pub(crate) snapshot: &'a RecordGroupRead,
	pub(crate) target: &'a ThrottlerUndeliveredTarget,
}
pub(crate) struct ThrottlerUndeliveredScopeGroupFields<'a> {
	pub(crate) key: &'a ThrottlerKey,
	pub(crate) settings: &'a ThrottlerSettings,
	pub(crate) group: ThrottlerUndeliveredGroupFields<'a>,
}

enum ThrottlerReleasePurpose<'a> {
	Undelivered(&'a ThrottlerUndeliveredTarget),
	PendingOutcome(&'a ThrottlerPendingOutcome),
}
impl ThrottlerReleasePurpose<'_> {
	fn predicates(&self) -> &ThrottlerAcquisitionPredicates {
		match self {
			Self::Undelivered(target) => &target.predicates,
			Self::PendingOutcome(pending) => &pending.predicates,
		}
	}
	fn into_input(
		self,
		observed_at: crate::records::StateTimestampMicros,
	) -> ThrottlerOutcomeInput {
		match self {
			Self::PendingOutcome(pending) => ThrottlerOutcomeInput {
				predicates: pending.predicates.clone(),
				outcome: pending.outcome,
				observed_at,
			},
			Self::Undelivered(target) => ThrottlerOutcomeInput {
				predicates: target.predicates.clone(),
				outcome: ThrottlerTaskOutcome::NotExecuted,
				observed_at,
			},
		}
	}
}

/// Actions and tentative result supply no backend group or transaction permission.
pub(crate) struct ThrottlerAcquisitionGroupPreparation {
	actions: Vec<(RecordIdentity, RecordMutation)>,
	result: ThrottlerAcquisitionResult,
}
impl ThrottlerAcquisitionGroupPreparation {
	/// Derives exact undelivered cleanup material before the owning group's first apply await.
	/// The owning executor must establish origin ending/order before using that material.
	pub(crate) fn into_parts(
		self,
	) -> (
		Vec<(RecordIdentity, RecordMutation)>,
		ThrottlerAcquisitionResult,
		Option<ThrottlerUndeliveredTarget>,
	) {
		let target = match &self.result {
			ThrottlerAcquisitionResult::Acquired(acquired) => Some(ThrottlerUndeliveredTarget {
				predicates: acquired.permit.predicates.clone(),
			}),
			ThrottlerAcquisitionResult::Throttled { .. }
			| ThrottlerAcquisitionResult::CircuitOpen => None,
		};
		(self.actions, self.result, target)
	}
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ThrottlerOutcomeAccounting {
	Consumed,
	AlreadyConsumedOrAbsent,
	ResourceOwned,
}
pub(crate) struct ThrottlerOutcomeGroupPreparation {
	actions: Vec<(RecordIdentity, RecordMutation)>,
	accounting: ThrottlerOutcomeAccounting,
}
impl ThrottlerOutcomeGroupPreparation {
	pub(crate) fn into_parts(
		self,
	) -> (
		Vec<(RecordIdentity, RecordMutation)>,
		ThrottlerOutcomeAccounting,
	) {
		(self.actions, self.accounting)
	}
}

impl ThrottlerSchemaBinding<'_> {
	pub(crate) fn acquisition_proposal(
		&self,
		holder: Option<HolderId>,
		rate_limit_cost: std::num::NonZeroU32,
	) -> Result<ThrottlerAcquireProposal, ThrottlerGroupPreparationError> {
		if (self.settings.concurrency().is_some() || self.settings.circuit().is_some())
			&& holder.is_none()
		{
			return Err(ThrottlerStateError::HolderRequired.into());
		}
		Ok(ThrottlerAcquireProposal {
			rate_limit_cost,
			key: self.key.clone(),
			settings: *self.settings,
			holder,
			outcome_id: self
				.settings
				.circuit()
				.map(|_| ThrottlerOutcomeId::generate())
				.transpose()
				.map_err(ThrottlerGroupPreparationError::OutcomeIdRandomness)?,
		})
	}

	pub(crate) fn prepare_acquisition_group(
		&self,
		fields: ThrottlerAcquireGroupFields<'_>,
	) -> Result<ThrottlerAcquisitionGroupPreparation, ThrottlerGroupPreparationError> {
		if fields.proposal.key != *self.key || fields.proposal.settings != *self.settings {
			return Err(ThrottlerGroupPreparationError::ProposalMismatch);
		}
		if fields.proposal.outcome_id.is_some() != self.settings.circuit().is_some() {
			return Err(ThrottlerGroupPreparationError::ProposalMismatch);
		}
		let anchor_identity = fields
			.proposal
			.outcome_id
			.as_ref()
			.map(|id| outcome_identity(fields.resource_identity, self.key, id));
		require_group(
			self.key,
			fields.resource_identity,
			anchor_identity.as_ref(),
			fields.snapshot,
		)?;
		if let Some(identity) = &anchor_identity
			&& fields.snapshot.snapshots()[identity].record().is_some()
		{
			return Err(record_error(
				identity,
				ThrottlerGroupPreparationError::AnchorCollision,
			));
		}
		let resource = fields.snapshot.snapshots()[fields.resource_identity].record();
		let mut state = match resource {
			Some(record) => self
				.decode_record(record)
				.map_err(|error| record_error(fields.resource_identity, error.into()))?,
			None => NativeThrottlerState::new(NativeThrottlerStateFields {
				key: self.key.clone(),
				settings: *self.settings,
			}),
		};
		let decision = self
			.acquire(
				&mut state,
				ThrottlerAcquireInput {
					rate_limit_cost: fields.proposal.rate_limit_cost,
					holder: fields.proposal.holder,
					observed_at: fields.snapshot.observed_at(),
					outcome_id: fields.proposal.outcome_id,
				},
			)
			.map_err(|error| record_error(fields.resource_identity, error.into()))?;
		let mut actions = Vec::new();
		if let Some(identity) = anchor_identity {
			let action = match &decision.result {
				ThrottlerAcquisitionResult::Acquired(acquired)
					if acquired.ordinary_anchor_requirement().is_some() =>
				{
					let anchor = NativeThrottlerOutcomeAnchor::from_tentative(acquired)?;
					RecordMutation::Write(
						self.encode_anchor(&anchor, fields.snapshot.observed_at())
							.map_err(|error| record_error(&identity, error))?,
					)
				}
				_ => RecordMutation::Keep,
			};
			actions.push((identity, action));
		}
		let resource_action = match decision.mutation {
			ThrottlerMutationDecision::Keep => RecordMutation::Keep,
			ThrottlerMutationDecision::Write => RecordMutation::Write(
				self.encode_record(
					&state,
					RecordMetadata {
						expires_at: None,
						updated_at: fields.snapshot.observed_at(),
					},
				)
				.map_err(|error| record_error(fields.resource_identity, error.into()))?,
			),
		};
		actions.push((fields.resource_identity.clone(), resource_action));
		Ok(ThrottlerAcquisitionGroupPreparation {
			actions,
			result: decision.result,
		})
	}

	/// Retained exact outcome purpose supplies no ending/order evidence or automatic replay.
	pub(crate) fn prepare_pending_outcome_group(
		&self,
		fields: ThrottlerPendingOutcomeGroupFields<'_>,
	) -> Result<ThrottlerOutcomeGroupPreparation, ThrottlerGroupPreparationError> {
		self.prepare_release_group(
			fields.resource_identity,
			fields.snapshot,
			ThrottlerReleasePurpose::PendingOutcome(fields.pending),
		)
	}
	/// Origin ending and physical ordering are executor prerequisites, never supplied by this pure
	/// target.
	pub(crate) fn prepare_undelivered_group(
		&self,
		fields: ThrottlerUndeliveredGroupFields<'_>,
	) -> Result<ThrottlerOutcomeGroupPreparation, ThrottlerGroupPreparationError> {
		self.prepare_release_group(
			fields.resource_identity,
			fields.snapshot,
			ThrottlerReleasePurpose::Undelivered(fields.target),
		)
	}
	fn prepare_release_group(
		&self,
		resource_identity: &RecordIdentity,
		snapshot: &RecordGroupRead,
		purpose: ThrottlerReleasePurpose<'_>,
	) -> Result<ThrottlerOutcomeGroupPreparation, ThrottlerGroupPreparationError> {
		let predicates = purpose.predicates();
		if predicates.key != *self.key {
			return Err(ThrottlerStateError::ResourceMismatch.into());
		}
		let anchor_identity = predicates
			.outcome_id()
			.map(|id| outcome_identity(resource_identity, self.key, id));
		require_group(
			self.key,
			resource_identity,
			anchor_identity.as_ref(),
			snapshot,
		)?;
		let mut actions = Vec::new();
		let accounting = if let Some(identity) = &anchor_identity {
			let Some(record) = snapshot.snapshots()[identity].record() else {
				return Ok(ThrottlerOutcomeGroupPreparation {
					actions: vec![
						(identity.clone(), RecordMutation::Keep),
						(resource_identity.clone(), RecordMutation::Keep),
					],
					accounting: ThrottlerOutcomeAccounting::AlreadyConsumedOrAbsent,
				});
			};
			let anchor = self
				.decode_anchor(record)
				.map_err(|error| record_error(identity, error))?;
			if !anchor.matches_predicates(predicates) {
				return Err(record_error(
					identity,
					ThrottlerGroupPreparationError::AnchorMismatch,
				));
			}
			actions.push((identity.clone(), RecordMutation::Delete));
			ThrottlerOutcomeAccounting::Consumed
		} else {
			ThrottlerOutcomeAccounting::ResourceOwned
		};
		let resource = snapshot.snapshots()[resource_identity]
			.record()
			.ok_or_else(|| {
				record_error(
					resource_identity,
					ThrottlerGroupPreparationError::ResourceAbsent,
				)
			})?;
		let state = self
			.decode_record(resource)
			.map_err(|error| record_error(resource_identity, error.into()))?;
		let prepared = self
			.prepare_release(&state, purpose.into_input(snapshot.observed_at()))
			.map_err(|error| record_error(resource_identity, error.into()))?;
		let (state, mutation) = prepared.into_state();
		let resource_action = match mutation {
			ThrottlerMutationDecision::Keep => RecordMutation::Keep,
			ThrottlerMutationDecision::Write => RecordMutation::Write(
				self.encode_record(
					&state,
					RecordMetadata {
						expires_at: None,
						updated_at: snapshot.observed_at(),
					},
				)
				.map_err(|error| record_error(resource_identity, error.into()))?,
			),
		};
		actions.push((resource_identity.clone(), resource_action));
		Ok(ThrottlerOutcomeGroupPreparation {
			actions,
			accounting,
		})
	}

	fn decode_anchor(
		&self,
		record: &StoredRecord,
	) -> Result<NativeThrottlerOutcomeAnchor, ThrottlerGroupPreparationError> {
		require_anchor_envelope(record)?;
		Ok(self.outcome_schema.decode_document(record.body())?.1)
	}
	fn encode_anchor(
		&self,
		anchor: &NativeThrottlerOutcomeAnchor,
		now: crate::records::StateTimestampMicros,
	) -> Result<EncodedRecord, ThrottlerGroupPreparationError> {
		if self.outcome_schema.policy().writer().envelope != StateStorageEnvelope::NativeJson {
			return Err(ThrottlerStateError::NativeWriterRequired.into());
		}
		let body = self.outcome_schema.encode(anchor)?;
		EncodedRecord::derive(
			body,
			RecordMetadata {
				expires_at: None,
				updated_at: now,
			},
			|body, metadata| {
				if metadata.expires_at.is_some() {
					return Err(ThrottlerGroupPreparationError::AnchorEnvelope);
				}
				let (_, actual) = self.outcome_schema.decode_document(body)?;
				if !actual.matches_anchor(anchor) {
					return Err(ThrottlerGroupPreparationError::AnchorMismatch);
				}
				Ok(IndexFields::new())
			},
		)
	}
}

impl NativeThrottlerOutcomeAnchor {
	fn from_tentative(
		acquired: &TentativeThrottlerAcquisition,
	) -> Result<Self, ThrottlerGroupPreparationError> {
		let permit = &acquired.permit.predicates;
		Ok(Self {
			key: permit.key.clone(),
			outcome_id: *permit
				.outcome_id()
				.ok_or(ThrottlerGroupPreparationError::PermitAnchorMismatch)?,
			holder: permit
				.holder()
				.cloned()
				.ok_or(ThrottlerGroupPreparationError::PermitAnchorMismatch)?,
			capability: permit
				.capability()
				.cloned()
				.ok_or(ThrottlerGroupPreparationError::PermitAnchorMismatch)?,
			epoch: permit
				.epoch()
				.cloned()
				.ok_or(ThrottlerGroupPreparationError::PermitAnchorMismatch)?,
		})
	}
	fn matches_predicates(&self, permit: &ThrottlerAcquisitionPredicates) -> bool {
		self.key == permit.key
			&& Some(&self.outcome_id) == permit.outcome_id()
			&& Some(&self.holder) == permit.holder()
			&& Some(&self.capability) == permit.capability()
			&& Some(&self.epoch) == permit.epoch()
	}
	fn matches_anchor(&self, other: &Self) -> bool {
		self.key == other.key
			&& self.outcome_id == other.outcome_id
			&& self.holder == other.holder
			&& self.capability == other.capability
			&& self.epoch == other.epoch
	}
}

fn outcome_identity(
	resource: &RecordIdentity,
	key: &ThrottlerKey,
	id: &ThrottlerOutcomeId,
) -> RecordIdentity {
	RecordIdentity::new(
		resource.namespace().clone(),
		NativeFleetResource::throttler_outcome_key(key, id),
	)
}
fn require_group(
	key: &ThrottlerKey,
	resource: &RecordIdentity,
	anchor: Option<&RecordIdentity>,
	snapshot: &RecordGroupRead,
) -> Result<(), ThrottlerGroupPreparationError> {
	if *resource.key() != NativeFleetResource::Throttler(key).record_key() {
		return Err(ThrottlerGroupPreparationError::GroupIdentityMismatch);
	}
	let mut expected = BTreeSet::from([resource.clone()]);
	if let Some(anchor) = anchor {
		expected.insert(anchor.clone());
	}
	if snapshot.snapshots().len() != expected.len()
		|| !expected
			.iter()
			.all(|identity| snapshot.snapshots().contains_key(identity))
	{
		return Err(ThrottlerGroupPreparationError::GroupIdentityMismatch);
	}
	Ok(())
}
fn record_error(
	identity: &RecordIdentity,
	source: ThrottlerGroupPreparationError,
) -> ThrottlerGroupPreparationError {
	ThrottlerGroupPreparationError::Record {
		identity: identity.clone(),
		source: Box::new(source),
	}
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ThrottlerOutcomeCodecError {
	#[error("Throttler private anchor JSON is invalid: {summary}")]
	Json { summary: JsonDecodeFailureSummary },
	#[error("Throttler private anchor resource is invalid")]
	InvalidResource,
	#[error("Throttler private anchor holder is invalid")]
	InvalidHolder,
	#[error("Throttler private anchor capability is invalid")]
	Private(#[from] PrivateCapabilityWireError),
}
#[derive(Debug, thiserror::Error)]
pub(crate) enum ThrottlerGroupPreparationError {
	#[error("Throttler outcome locator generation failed")]
	OutcomeIdRandomness(#[source] RandomError),
	#[error("Throttler acquisition proposal belongs to another resource or profile")]
	ProposalMismatch,
	#[error("Throttler group differs from its complete closed identity set")]
	GroupIdentityMismatch,
	#[error("Throttler permit has inconsistent ordinary-anchor requirements")]
	PermitAnchorMismatch,
	#[error("Throttler ordinary anchor identity is already occupied")]
	AnchorCollision,
	#[error("Throttler ordinary anchor must be permanent and have no projections")]
	AnchorEnvelope,
	#[error("Throttler ordinary anchor does not authenticate this acquisition")]
	AnchorMismatch,
	#[error("Throttler outcome resource is absent")]
	ResourceAbsent,
	#[error("Throttler resource preparation failed")]
	Resource(#[from] ThrottlerStateError),
	#[error("Throttler ordinary anchor representation is unreadable")]
	AnchorRead(#[from] ContractStateReadError<ThrottlerOutcomeCodecError>),
	#[error("Throttler ordinary anchor selected encoding failed")]
	AnchorEncode(#[from] StateSchemaError<ThrottlerOutcomeCodecError>),
	#[error("Throttler group record preparation failed at {identity:?}")]
	Record {
		identity: RecordIdentity,
		#[source]
		source: Box<Self>,
	},
}
