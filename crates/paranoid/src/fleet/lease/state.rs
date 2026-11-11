//! Bound native lease point operations over durable fencing history.
//!
//! The record envelope never expires. Claim expiry is embedded state, so releasing or
//! expiring a claim cannot remove its fencing history. Storage admission and persistence
//! belong to the caller. Tentative private results require an owned successful commit before
//! runtime integration can deliver usable claim authority.

use super::cleanup_target::LeaseCleanupTarget;
use super::{Claim, ClaimDuration, FencingToken, HolderId, HolderSnapshot, LeaseKey, Token};
use crate::installation::contract::{
	BoundComponentContract, BoundStateSchema, ComponentDeclaration, ComponentDeclarationFields,
	ComponentPolicy, ComponentPolicyFields, ComponentSettings, ComponentSettingsDeclaration,
	ComponentSettingsRequirement, ContractStateReadError, RepresentationPolicy, SchemaPolicy,
	SemanticContractDef, StateContractError, StateStorageEnvelope,
};
use crate::records::state_schema::{
	StateRepresentationFormat, StateSchema, StateSchemaError, StateVersion,
};
use crate::records::{
	EncodedRecord, IndexFields, IndexScalar, PointRecordLock, PointRecordRead, RecordMetadata,
	StateTimestampMicros, StoredRecord,
};
use primitives_rs::encoding::decimal::parse_canonical_decimal;
use primitives_rs::serialization::json::{JsonDecodeFailureSummary, deserialize_required_option};
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

const LEASE_SCHEMA: &str = "paranoid.lease";
const LEASE_COMPONENT: &str = "paranoid.lease";
const LEASE_SEMANTICS: &str = "paranoid.lease.v1";
const LEASE_REPRESENTATION_VERSION: u32 = 1;
const CLAIM_EXPIRY_INDEX: &str = "claim_expires_at";

/// A parsed native lease value or a rejected deterministic transition.
#[derive(Debug, thiserror::Error)]
pub(crate) enum LeaseStateError {
	#[error("lease state JSON is invalid: {summary}")]
	Json { summary: JsonDecodeFailureSummary },
	#[error("lease state is invalid: {reason}")]
	Invalid { reason: &'static str },
	#[error("lease fencing history exhausted the positive signed 64-bit domain")]
	FencingExhausted,
	#[error("lease expiration exceeds the signed timestamp domain")]
	ExpirationOverflow,
	#[error("lease transition requires a different private claim token")]
	TokenNotRotated,
}

/// Durable positive history and an optional current or expired claim.
#[derive(Clone, Debug)]
pub(crate) struct LeaseState {
	last_fencing_token: FencingToken,
	claim: Option<StoredClaim>,
}

/// Parsed durable facts for import without creating usable claim authority.
pub(super) struct LeasePreservedParts {
	pub(super) last_fencing_token: FencingToken,
	pub(super) claim: Option<LeasePreservedClaim>,
}

/// An embedded claim, including an expired claim, with a private token that remains state.
pub(super) struct LeasePreservedClaim {
	pub(super) holder_id: HolderId,
	pub(super) fencing_token: FencingToken,
	pub(super) lease_token: Token,
	pub(super) expires_at: StateTimestampMicros,
}

#[derive(Clone, Debug)]
struct StoredClaim {
	holder_id: HolderId,
	fencing_token: FencingToken,
	lease_token: Token,
	expires_at: StateTimestampMicros,
}

enum LeaseReleaseAuthority<'a> {
	Claim(&'a Claim),
	Cleanup(&'a LeaseCleanupTarget),
}

/// State to persist and private authority to deliver after a successful commit.
#[derive(Debug)]
struct PreparedLeaseGrant {
	state: LeaseState,
	claim: Claim,
}

/// Parsed acquisition material. The caller generates a fresh holder and private token.
pub(crate) struct LeaseAcquisition {
	pub(super) holder_id: HolderId,
	pub(super) lease_token: Token,
	pub(super) duration: ClaimDuration,
}

/// Shared lease semantic implementation and its native representation registry.
pub(crate) struct LeaseComponent {
	schema: StateSchema<LeaseState, LeaseStateError>,
}

impl LeaseComponent {
	pub(crate) fn new() -> Self {
		Self {
			schema: StateSchema::new(crate::records::state_schema::StateSchemaFields {
				identifier: LEASE_SCHEMA,
				versions: vec![StateVersion {
					version: LEASE_REPRESENTATION_VERSION,
					format: StateRepresentationFormat::Json,
					decoder: decode_state,
					encoder: Some(encode_state),
				}],
			})
			.expect("the lease declares one positive representation version"),
		}
	}

	/// Derives capabilities from this registry and semantic support from these transitions.
	pub(crate) fn declaration(&self) -> ComponentDeclaration<'_, Self> {
		ComponentDeclaration::new(ComponentDeclarationFields {
			identifier: LEASE_COMPONENT,
			fixed_schemas: vec![self.schema.capabilities()],
			settings: ComponentSettingsDeclaration::Parameterless,
			semantics: vec![SemanticContractDef {
				identifier: LEASE_SEMANTICS,
				transitions: self,
				settings: ComponentSettingsRequirement::Parameterless,
			}],
		})
		.expect("lease declares one schema and one semantic implementation")
	}

	pub(crate) fn schema(&self) -> &StateSchema<LeaseState, LeaseStateError> {
		&self.schema
	}

	/// Fixed native Lease history policy; the separate source bridge selects source execution.
	pub(crate) fn initial_policy(&self) -> Result<ComponentPolicy, LeaseBindingError> {
		let writer = RepresentationPolicy {
			version: LEASE_REPRESENTATION_VERSION,
			envelope: StateStorageEnvelope::NativeJson,
		};
		let policy = ComponentPolicy::new(ComponentPolicyFields {
			component_identifier: LEASE_COMPONENT.to_owned(),
			semantic_contract: LEASE_SEMANTICS.to_owned(),
			schemas: vec![SchemaPolicy::new(
				self.schema.identifier().to_owned(),
				writer,
				vec![writer],
			)?],
			settings: ComponentSettings::Parameterless,
		})?;
		let declaration = self.declaration();
		self.require_bound_policy(&declaration.bind(&policy)?)?;
		Ok(policy)
	}

	fn bind_actual_schema<'a>(
		&'a self,
		component: &'a BoundComponentContract<'_, '_, Self>,
	) -> Result<BoundStateSchema<'a, LeaseState, LeaseStateError>, LeaseBindingError> {
		if !std::ptr::eq(self, component.transitions()) {
			return Err(LeaseBindingError::TransitionInstanceMismatch);
		}
		let schema = component.bind_schema(&self.schema)?;
		if schema.policy().writer().envelope != StateStorageEnvelope::NativeJson {
			return Err(LeaseBindingError::NativeWriterRequired);
		}
		Ok(schema)
	}

	/// Checks actual code and selected native policy without manufacturing a lease key.
	pub(crate) fn require_bound_policy(
		&self,
		component: &BoundComponentContract<'_, '_, Self>,
	) -> Result<(), LeaseBindingError> {
		self.bind_actual_schema(component)?;
		Ok(())
	}

	/// Retains the actual transition instance, exact bound schema policy, and record key.
	pub(crate) fn bind_schema<'a>(
		&'a self,
		component: &'a BoundComponentContract<'_, '_, Self>,
		key: LeaseKey,
	) -> Result<LeaseSchemaBinding<'a>, LeaseBindingError> {
		let schema = self.bind_actual_schema(component)?;
		Ok(LeaseSchemaBinding {
			component: component.transitions(),
			schema,
			key,
		})
	}

	/// Refuses a live claim; an acquisition advances history and checks expiry before changing
	/// state.
	fn try_acquire(
		&self,
		key: &LeaseKey,
		current: Option<&LeaseState>,
		acquisition: LeaseAcquisition,
		observed_at: StateTimestampMicros,
	) -> Result<Option<PreparedLeaseGrant>, LeaseStateError> {
		if current
			.and_then(|state| state.live_claim(observed_at))
			.is_some()
		{
			return Ok(None);
		}
		let fencing_token = next_fencing_token(current)?;
		let expires_at = expiration(observed_at, acquisition.duration)?;
		if current
			.and_then(|state| state.claim.as_ref())
			.is_some_and(|claim| claim.lease_token == acquisition.lease_token)
		{
			return Err(LeaseStateError::TokenNotRotated);
		}
		let stored = StoredClaim {
			holder_id: acquisition.holder_id,
			fencing_token,
			lease_token: acquisition.lease_token,
			expires_at,
		};
		let claim = self.to_claim(key, &stored);
		Ok(Some(PreparedLeaseGrant {
			state: LeaseState {
				last_fencing_token: fencing_token,
				claim: Some(stored),
			},
			claim,
		}))
	}

	/// Renews only live matching authority, rotating its token while preserving its fence.
	fn try_renew(
		&self,
		key: &LeaseKey,
		current: &LeaseState,
		authority: &Claim,
		next_token: Token,
		duration: ClaimDuration,
		observed_at: StateTimestampMicros,
	) -> Result<Option<PreparedLeaseGrant>, LeaseStateError> {
		let Some(stored) = self.matching_claim(key, current, authority, observed_at) else {
			return Ok(None);
		};
		if stored.lease_token == next_token {
			return Err(LeaseStateError::TokenNotRotated);
		}
		let stored = StoredClaim {
			holder_id: stored.holder_id.clone(),
			fencing_token: stored.fencing_token,
			lease_token: next_token,
			expires_at: expiration(observed_at, duration)?,
		};
		let claim = self.to_claim(key, &stored);
		Ok(Some(PreparedLeaseGrant {
			state: LeaseState {
				last_fencing_token: current.last_fencing_token,
				claim: Some(stored),
			},
			claim,
		}))
	}

	/// Clears only a live matching private claim, preserving durable fencing history.
	fn try_release(
		&self,
		key: &LeaseKey,
		current: &LeaseState,
		authority: LeaseReleaseAuthority<'_>,
		observed_at: StateTimestampMicros,
	) -> Option<LeaseState> {
		match authority {
			LeaseReleaseAuthority::Claim(claim) => {
				self.matching_claim(key, current, claim, observed_at)?;
			}
			LeaseReleaseAuthority::Cleanup(target) => {
				let stored = current.live_claim(observed_at)?;
				if !target.matches(
					key,
					&stored.holder_id,
					stored.fencing_token,
					&stored.lease_token,
				) {
					return None;
				}
			}
		}
		Some(LeaseState {
			last_fencing_token: current.last_fencing_token,
			claim: None,
		})
	}

	/// Returns diagnostics without copying the private token into the result.
	pub(super) fn live_holder(
		&self,
		key: &LeaseKey,
		current: &LeaseState,
		observed_at: StateTimestampMicros,
	) -> Option<HolderSnapshot> {
		let claim = current.live_claim(observed_at)?;
		Some(HolderSnapshot {
			key: key.clone(),
			holder_id: claim.holder_id.clone(),
			fencing_token: claim.fencing_token,
			expires_at_unix_micros: claim.expires_at.as_i64(),
		})
	}

	fn matching_claim<'a>(
		&self,
		key: &LeaseKey,
		current: &'a LeaseState,
		authority: &Claim,
		observed_at: StateTimestampMicros,
	) -> Option<&'a StoredClaim> {
		let stored = current.live_claim(observed_at)?;
		(key == &authority.key
			&& stored.holder_id == authority.holder_id
			&& stored.fencing_token == authority.fencing_token
			&& stored.lease_token == authority.lease_token)
			.then_some(stored)
	}

	fn to_claim(&self, key: &LeaseKey, stored: &StoredClaim) -> Claim {
		Claim {
			key: key.clone(),
			holder_id: stored.holder_id.clone(),
			fencing_token: stored.fencing_token,
			lease_token: stored.lease_token.clone(),
			expires_at_unix_micros: stored.expires_at.as_i64(),
		}
	}
}

/// Native binding rejects an unsupported writer before the caller acquires mutation authority.
#[derive(Debug, thiserror::Error)]
pub(crate) enum LeaseBindingError {
	#[error("lease schema or semantic contract binding failed")]
	Contract(#[from] StateContractError),
	#[error("native lease storage requires an admitted native JSON writer")]
	NativeWriterRequired,
	#[error("lease transition instance differs from its schema owner")]
	TransitionInstanceMismatch,
}

/// Exact retained-reader, writer, or durable physical-envelope failures.
#[derive(Debug, thiserror::Error)]
pub(crate) enum LeaseRepresentationError {
	#[error("lease representation is not readable under its bound policy")]
	Read(#[from] ContractStateReadError<LeaseStateError>),
	#[error("lease selected representation could not be encoded")]
	Encode(#[from] StateSchemaError<LeaseStateError>),
	#[error("lease fencing history has an invalid physical envelope")]
	Envelope(#[source] LeaseStateError),
	#[error("lease stored projections differ from its decoded durable state")]
	ProjectionMismatch,
}

/// A native point operation or completed refusal/rejection cleanup failed.
#[derive(Debug)]
pub(crate) enum LeaseOperationError<E> {
	Representation(LeaseRepresentationError),
	Transition(LeaseStateError),
	Storage(E),
	KeepCleanupFailed(E),
	CleanupFailed {
		operation_error: Box<LeaseOperationError<E>>,
		cleanup_error: E,
	},
}

impl<E> std::fmt::Display for LeaseOperationError<E> {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		let message = match self {
			Self::Representation(_) => "lease physical representation could not be read or encoded",
			Self::Transition(_) => "lease semantic transition was rejected",
			Self::Storage(_) => "lease point storage work failed",
			Self::KeepCleanupFailed(_) => {
				"lease record preservation and reservation cleanup failed"
			}
			Self::CleanupFailed { .. } => {
				"lease operation failed and rejection cleanup also failed"
			}
		};
		formatter.write_str(message)
	}
}

impl<E: std::error::Error + 'static> std::error::Error for LeaseOperationError<E> {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self {
			Self::Representation(error) => Some(error),
			Self::Transition(error) => Some(error),
			Self::Storage(error) | Self::KeepCleanupFailed(error) => Some(error),
			Self::CleanupFailed {
				operation_error, ..
			} => Some(operation_error.as_ref()),
		}
	}
}

/// Undelivered private material after a point write, before any owned commit proof.
/// Its internal extraction belongs to the owned runtime's known-commit delivery boundary.
pub(crate) struct TentativeLeaseAuthority {
	claim: Claim,
}

impl TentativeLeaseAuthority {
	/// Holds an actual parsed SQL grant privately until its owned transaction confirms COMMIT.
	pub(super) fn from_source_sql_grant(claim: Claim) -> Self {
		Self { claim }
	}

	/// The owned runtime operation calls this only after its transaction reports known COMMIT.
	/// Point writes, caller-owned work, and uncertain completion cannot deliver this material.
	pub(super) fn into_claim_after_known_commit(self) -> Claim {
		self.claim
	}
}

impl std::fmt::Debug for TentativeLeaseAuthority {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter
			.debug_struct("TentativeLeaseAuthority")
			.finish_non_exhaustive()
	}
}

/// One exact record key and the shared actual lease implementation's bound native schema.
/// Operations retain this key instead of accepting a replacement key at mutation time.
pub(crate) struct LeaseSchemaBinding<'a> {
	component: &'a LeaseComponent,
	schema: BoundStateSchema<'a, LeaseState, LeaseStateError>,
	key: LeaseKey,
}

pub(super) struct NativeLeaseClaimReleaseFields<'a> {
	pub(super) snapshot: &'a PointRecordRead,
	pub(super) authority: &'a Claim,
}
/// A native release prepared at one locked observation of the lease record.
pub(super) enum PreparedNativeLeaseRelease {
	/// The claim does not hold the lease, so the release leaves the record unchanged.
	NoMatch,
	/// The claim holds the lease, and this record replaces the stored record.
	Release(EncodedRecord),
}

pub(super) struct NativeLeaseClaimMatchFields<'a> {
	pub(super) snapshot: &'a PointRecordRead,
	pub(super) authority: &'a Claim,
}

impl LeaseSchemaBinding<'_> {
	pub(super) fn key(&self) -> &LeaseKey {
		&self.key
	}

	/// Checks actual private state without creating a Claim or proving physical association.
	pub(super) fn matches_native_claim(
		&self,
		fields: NativeLeaseClaimMatchFields<'_>,
	) -> Result<bool, LeaseRepresentationError> {
		let Some(record) = fields.snapshot.record() else {
			return Ok(false);
		};
		let (_, state) = self.decode_native_stored(record)?;
		Ok(self
			.component
			.matching_claim(
				&self.key,
				&state,
				fields.authority,
				fields.snapshot.observed_at(),
			)
			.is_some())
	}

	fn decode_native_stored(
		&self,
		record: &StoredRecord,
	) -> Result<(u32, LeaseState), LeaseRepresentationError> {
		let decoded = self.decode_stored(record)?;
		if record.index_fields() != &lease_index_fields(&decoded.1) {
			return Err(LeaseRepresentationError::ProjectionMismatch);
		}
		Ok(decoded)
	}

	/// Parses physical values even when expired; outer expiry is damage to durable history.
	pub(super) fn decode_record(
		&self,
		snapshot: &PointRecordRead,
	) -> Result<Option<LeaseState>, LeaseRepresentationError> {
		snapshot
			.record()
			.map(|record| self.decode_stored(record).map(|(_, state)| state))
			.transpose()
	}

	fn decode_stored(
		&self,
		record: &StoredRecord,
	) -> Result<(u32, LeaseState), LeaseRepresentationError> {
		require_durable_envelope(record.metadata()).map_err(LeaseRepresentationError::Envelope)?;
		self.schema
			.decode_document(record.body())
			.map_err(LeaseRepresentationError::Read)
	}

	/// Uses the exact physical read observation and returns no private capability.
	pub(super) fn decode_live_holder(
		&self,
		snapshot: &PointRecordRead,
	) -> Result<Option<HolderSnapshot>, LeaseRepresentationError> {
		Ok(self.decode_record(snapshot)?.as_ref().and_then(|state| {
			self.component
				.live_holder(&self.key, state, snapshot.observed_at())
		}))
	}

	/// Acquires only after a fresh locked clock, incrementing preserved durable fencing history.
	pub(super) async fn acquire<L: PointRecordLock>(
		&self,
		mut lock: L,
		acquisition: LeaseAcquisition,
	) -> Result<Option<TentativeLeaseAuthority>, LeaseOperationError<L::Error>> {
		let current = match self.decode_record(lock.snapshot()) {
			Ok(current) => current,
			Err(error) => {
				return Err(
					clean_rejection(lock, LeaseOperationError::Representation(error)).await,
				);
			}
		};
		let now = lock
			.observe_time()
			.await
			.map_err(LeaseOperationError::Storage)?;
		let grant = match self
			.component
			.try_acquire(&self.key, current.as_ref(), acquisition, now)
		{
			Ok(Some(grant)) => grant,
			Ok(None) => return keep_without_mutation(lock, None).await,
			Err(error) => {
				return Err(clean_rejection(lock, LeaseOperationError::Transition(error)).await);
			}
		};
		self.persist_state(
			lock,
			grant.state,
			RecordMetadata {
				expires_at: None,
				updated_at: now,
			},
		)
		.await?;
		Ok(Some(TentativeLeaseAuthority { claim: grant.claim }))
	}

	/// Renews live matching private authority, rotating its token while retaining its fence.
	pub(super) async fn renew<L: PointRecordLock>(
		&self,
		mut lock: L,
		authority: &Claim,
		next_token: Token,
		duration: ClaimDuration,
	) -> Result<Option<TentativeLeaseAuthority>, LeaseOperationError<L::Error>> {
		let current = match self.decode_record(lock.snapshot()) {
			Ok(Some(current)) => current,
			Ok(None) => return keep_without_mutation(lock, None).await,
			Err(error) => {
				return Err(
					clean_rejection(lock, LeaseOperationError::Representation(error)).await,
				);
			}
		};
		let now = lock
			.observe_time()
			.await
			.map_err(LeaseOperationError::Storage)?;
		let grant = match self
			.component
			.try_renew(&self.key, &current, authority, next_token, duration, now)
		{
			Ok(Some(grant)) => grant,
			Ok(None) => return keep_without_mutation(lock, None).await,
			Err(error) => {
				return Err(clean_rejection(lock, LeaseOperationError::Transition(error)).await);
			}
		};
		self.persist_state(
			lock,
			grant.state,
			RecordMetadata {
				expires_at: None,
				updated_at: now,
			},
		)
		.await?;
		Ok(Some(TentativeLeaseAuthority { claim: grant.claim }))
	}

	/// Prepares one native release at the actual supplied locked clock without another read.
	pub(super) fn prepare_native_claim_release(
		&self,
		fields: NativeLeaseClaimReleaseFields<'_>,
	) -> Result<PreparedNativeLeaseRelease, LeaseRepresentationError> {
		let Some(record) = fields.snapshot.record() else {
			return Ok(PreparedNativeLeaseRelease::NoMatch);
		};
		let (_, current) = self.decode_native_stored(record)?;
		let now = fields.snapshot.observed_at();
		let Some(state) = self.component.try_release(
			&self.key,
			&current,
			LeaseReleaseAuthority::Claim(fields.authority),
			now,
		) else {
			return Ok(PreparedNativeLeaseRelease::NoMatch);
		};
		Ok(PreparedNativeLeaseRelease::Release(self.encode_record(
			&state,
			RecordMetadata {
				expires_at: None,
				updated_at: now,
			},
		)?))
	}

	/// Clears only live matching authority and persists history instead of deleting the record.
	pub(super) async fn release<L: PointRecordLock>(
		&self,
		lock: L,
		authority: &Claim,
	) -> Result<bool, LeaseOperationError<L::Error>> {
		self.release_authorized(lock, LeaseReleaseAuthority::Claim(authority))
			.await
	}

	/// Consumes one point and releases only sealed actual proposal/confirmed cleanup material.
	pub(super) async fn release_cleanup<L: PointRecordLock>(
		&self,
		lock: L,
		target: &LeaseCleanupTarget,
	) -> Result<bool, LeaseOperationError<L::Error>> {
		self.release_authorized(lock, LeaseReleaseAuthority::Cleanup(target))
			.await
	}

	async fn release_authorized<L: PointRecordLock>(
		&self,
		mut lock: L,
		authority: LeaseReleaseAuthority<'_>,
	) -> Result<bool, LeaseOperationError<L::Error>> {
		let current = match self.decode_record(lock.snapshot()) {
			Ok(Some(current)) => current,
			Ok(None) => return keep_without_mutation(lock, false).await,
			Err(error) => {
				return Err(
					clean_rejection(lock, LeaseOperationError::Representation(error)).await,
				);
			}
		};
		let now = lock
			.observe_time()
			.await
			.map_err(LeaseOperationError::Storage)?;
		let Some(state) = self
			.component
			.try_release(&self.key, &current, authority, now)
		else {
			return keep_without_mutation(lock, false).await;
		};
		self.persist_state(
			lock,
			state,
			RecordMetadata {
				expires_at: None,
				updated_at: now,
			},
		)
		.await?;
		Ok(true)
	}

	async fn persist_state<L: PointRecordLock>(
		&self,
		lock: L,
		state: LeaseState,
		metadata: RecordMetadata,
	) -> Result<(), LeaseOperationError<L::Error>> {
		let encoded = match self.encode_record(&state, metadata) {
			Ok(encoded) => encoded,
			Err(error) => {
				return Err(
					clean_rejection(lock, LeaseOperationError::Representation(error)).await,
				);
			}
		};
		lock.write(encoded)
			.await
			.map_err(LeaseOperationError::Storage)
	}

	pub(super) fn encode_record(
		&self,
		state: &LeaseState,
		metadata: RecordMetadata,
	) -> Result<EncodedRecord, LeaseRepresentationError> {
		require_durable_envelope(metadata).map_err(LeaseRepresentationError::Envelope)?;
		let body = self.schema.encode(state)?;
		EncodedRecord::derive(body, metadata, |encoded, metadata| {
			require_durable_envelope(metadata).map_err(LeaseRepresentationError::Envelope)?;
			let (_, state) = self.schema.decode_document(encoded)?;
			Ok(lease_index_fields(&state))
		})
	}
}

fn lease_index_fields(state: &LeaseState) -> IndexFields {
	let mut fields = IndexFields::new();
	if let Some(claim) = &state.claim {
		fields.insert(
			CLAIM_EXPIRY_INDEX.to_owned(),
			IndexScalar::Timestamp(claim.expires_at),
		);
	}
	fields
}

async fn keep_without_mutation<L: PointRecordLock, T>(
	lock: L,
	value: T,
) -> Result<T, LeaseOperationError<L::Error>> {
	lock.keep()
		.await
		.map_err(LeaseOperationError::KeepCleanupFailed)?;
	Ok(value)
}

async fn clean_rejection<L: PointRecordLock>(
	lock: L,
	operation_error: LeaseOperationError<L::Error>,
) -> LeaseOperationError<L::Error> {
	match lock.keep().await {
		Ok(()) => operation_error,
		Err(cleanup_error) => LeaseOperationError::CleanupFailed {
			operation_error: Box::new(operation_error),
			cleanup_error,
		},
	}
}

impl LeaseState {
	/// Preserves parsed history and claim facts without extracting a Claim capability.
	pub(super) fn from_preserved_parts(
		parts: LeasePreservedParts,
	) -> Result<Self, LeaseStateError> {
		if parts
			.claim
			.as_ref()
			.is_some_and(|claim| claim.fencing_token > parts.last_fencing_token)
		{
			return Err(invalid("claim generation exceeds durable fencing history"));
		}
		Ok(Self {
			last_fencing_token: parts.last_fencing_token,
			claim: parts.claim.map(|claim| StoredClaim {
				holder_id: claim.holder_id,
				fencing_token: claim.fencing_token,
				lease_token: claim.lease_token,
				expires_at: claim.expires_at,
			}),
		})
	}

	fn live_claim(&self, observed_at: StateTimestampMicros) -> Option<&StoredClaim> {
		self.claim
			.as_ref()
			.filter(|claim| observed_at < claim.expires_at)
	}
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LeaseWire {
	last_fencing_token: String,
	#[serde(deserialize_with = "deserialize_required_option")]
	claim: Option<ClaimWire>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ClaimWire {
	holder_id: String,
	fencing_token: String,
	lease_token: [u8; super::LEASE_TOKEN_LEN],
	expires_at: String,
}

impl Drop for ClaimWire {
	fn drop(&mut self) {
		self.lease_token.zeroize();
	}
}

fn decode_state(bytes: &[u8]) -> Result<LeaseState, LeaseStateError> {
	let wire: LeaseWire = serde_json::from_slice(bytes).map_err(json_error)?;
	let last_fencing_token = parse_fence(&wire.last_fencing_token)?;
	let claim = wire
		.claim
		.map(|wire| {
			let fencing_token = parse_fence(&wire.fencing_token)?;
			if fencing_token > last_fencing_token {
				return Err(invalid("claim generation exceeds durable fencing history"));
			}
			Ok(StoredClaim {
				holder_id: HolderId::new(&wire.holder_id)
					.map_err(|_| invalid("claim holder is invalid"))?,
				fencing_token,
				lease_token: Token::from_bytes(wire.lease_token),
				expires_at: StateTimestampMicros::new(parse_i64(&wire.expires_at)?),
			})
		})
		.transpose()?;
	Ok(LeaseState {
		last_fencing_token,
		claim,
	})
}

fn encode_state(state: &LeaseState) -> Result<Vec<u8>, LeaseStateError> {
	serde_json::to_vec(&LeaseWire {
		last_fencing_token: state.last_fencing_token.as_i64().to_string(),
		claim: state.claim.as_ref().map(|claim| ClaimWire {
			holder_id: claim.holder_id.as_str().to_owned(),
			fencing_token: claim.fencing_token.as_i64().to_string(),
			lease_token: *claim.lease_token.as_bytes(),
			expires_at: claim.expires_at.as_i64().to_string(),
		}),
	})
	.map_err(json_error)
}

fn next_fencing_token(current: Option<&LeaseState>) -> Result<FencingToken, LeaseStateError> {
	let previous_fence = current.map_or(0, |state| state.last_fencing_token.as_i64());
	let next_fence = previous_fence
		.checked_add(1)
		.ok_or(LeaseStateError::FencingExhausted)?;
	Ok(FencingToken::from_i64(next_fence)
		.expect("incrementing nonnegative history produces a positive fence"))
}

fn expiration(
	observed_at: StateTimestampMicros,
	duration: ClaimDuration,
) -> Result<StateTimestampMicros, LeaseStateError> {
	observed_at
		.as_i64()
		.checked_add(duration.positive_micros())
		.map(StateTimestampMicros::new)
		.ok_or(LeaseStateError::ExpirationOverflow)
}

fn require_durable_envelope(metadata: RecordMetadata) -> Result<(), LeaseStateError> {
	if metadata.expires_at.is_some() {
		Err(invalid("lease fencing-history envelope must not expire"))
	} else {
		Ok(())
	}
}

fn parse_i64(text: &str) -> Result<i64, LeaseStateError> {
	parse_canonical_decimal(text)
		.map_err(|_| invalid("integer text must be a canonical signed 64-bit decimal"))
}

fn parse_fence(text: &str) -> Result<FencingToken, LeaseStateError> {
	FencingToken::from_i64(parse_i64(text)?)
		.map_err(|_| invalid("fencing history and claim generations must be positive"))
}

fn invalid(reason: &'static str) -> LeaseStateError {
	LeaseStateError::Invalid { reason }
}

fn json_error(error: serde_json::Error) -> LeaseStateError {
	LeaseStateError::Json {
		summary: JsonDecodeFailureSummary::from(&error),
	}
}
