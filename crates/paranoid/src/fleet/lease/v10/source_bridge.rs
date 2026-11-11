//! Typed source lease import and explicitly selected coexistence SQL execution.
//!
//! A storage adapter must produce a coherent counter/claim observation for the exact key.
//! Import preserves aggregate facts without granting private authority. Its separate SQL
//! definition selects the actual execution kernel; neither definition proves source-writer
//! retirement or selects a native representation writer.

use super::source_execution::LeaseSourceSqlKernel;
use crate::fleet::lease::state::{
	LeasePreservedClaim, LeasePreservedParts, LeaseState, LeaseStateError,
};
use crate::fleet::lease::{FencingToken, HolderId, LeaseKey, Token};
use crate::installation::contract::{
	BoundComponentContract, ComponentDeclaration, ComponentDeclarationFields, ComponentPolicy,
	ComponentPolicyFields, ComponentSettings, ComponentSettingsDeclaration,
	ComponentSettingsRequirement, SemanticContractDef, StateContractError,
};
use crate::records::{RecordMetadata, StateTimestampMicros};

const LEASE_SOURCE_COMPONENT: &str = "paranoid.lease.source.v10";
const LEASE_SOURCE_SEMANTICS: &str = "paranoid.lease.source.v10.import.v1";
const LEASE_SOURCE_SQL_SEMANTICS: &str = "paranoid.lease.source.v10.sql.v1";

/// Exact source timestamp domain; adapters must use numeric microseconds, without rounding.
/// The variant order implements numerical comparison, including the two infinities.
#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
pub(super) enum LeaseSourceTimestamp {
	NegativeInfinity,
	FiniteUnixMicros(i128),
	PositiveInfinity,
}

impl LeaseSourceTimestamp {
	fn into_native(
		self,
		field: LeaseSourceTimestampField,
	) -> Result<StateTimestampMicros, LeaseSourceImportError> {
		match self {
			Self::FiniteUnixMicros(value) => i64::try_from(value)
				.map(StateTimestampMicros::new)
				.map_err(|_| LeaseSourceImportError::TimestampUnrepresentable { field }),
			Self::NegativeInfinity | Self::PositiveInfinity => {
				Err(LeaseSourceImportError::TimestampUnrepresentable { field })
			}
		}
	}
}

/// One source counter row. Its fence is parsed by the registered import transition.
pub(super) struct LeaseSourceCounter {
	pub(super) last_fencing_token: i64,
	pub(super) updated_at: LeaseSourceTimestamp,
}

/// One source claim row with parsed diagnostic identity and private token material.
/// Storage parsing must reject invalid holder text and token length before constructing it.
pub(super) struct LeaseSourceClaim {
	pub(super) holder_id: HolderId,
	pub(super) fencing_token: i64,
	pub(super) lease_token: Token,
	pub(super) expires_at: LeaseSourceTimestamp,
	pub(super) updated_at: LeaseSourceTimestamp,
}

/// Both optional physical rows for one exact parsed source key.
/// Construction does not prove snapshot coherence or lock ownership.
pub(crate) struct LeaseSourceRows {
	pub(super) key: LeaseKey,
	pub(super) counter: Option<LeaseSourceCounter>,
	pub(super) claim: Option<LeaseSourceClaim>,
}

impl std::fmt::Debug for LeaseSourceRows {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter
			.debug_struct("LeaseSourceRows")
			.field("counter_present", &self.counter.is_some())
			.field("claim_present", &self.claim.is_some())
			.finish_non_exhaustive()
	}
}

/// Import preserves physical absence or durable state with an immortal outer envelope.
/// The state contains preserved private material but exposes no Claim authority extraction.
pub(crate) enum LeaseSourceImport {
	Absent,
	Present {
		state: LeaseState,
		metadata: RecordMetadata,
	},
}

impl std::fmt::Debug for LeaseSourceImport {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter
			.debug_struct("LeaseSourceImport")
			.field("present", &matches!(self, Self::Present { .. }))
			.finish_non_exhaustive()
	}
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LeaseSourceFenceField {
	CounterLastFence,
	ClaimFence,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LeaseSourceTimestampField {
	CounterUpdatedAt,
	ClaimUpdatedAt,
	ClaimExpiry,
}

/// Sanitized source damage and exact native-domain failures; no source values are logged.
#[derive(Debug, thiserror::Error)]
pub(crate) enum LeaseSourceImportError {
	#[error("lease source observation belongs to a different key")]
	KeyMismatch,
	#[error("lease source claim has no durable fencing counter")]
	MissingCounter,
	#[error("lease source fence is outside the positive signed domain: {field:?}")]
	NonpositiveFence { field: LeaseSourceFenceField },
	#[error("lease source counter is below its claim generation")]
	CounterBelowClaim,
	#[error("lease source timestamp cannot be preserved in the native domain: {field:?}")]
	TimestampUnrepresentable { field: LeaseSourceTimestampField },
	#[error("lease source facts could not form native durable state")]
	NativeState(#[source] LeaseStateError),
}

/// Binding failures describe execution selection, independently of source data conversion.
#[derive(Debug, thiserror::Error)]
pub(crate) enum LeaseSourceBindingError {
	#[error("source lease binding does not select its SQL execution definition")]
	SqlExecutionNotSelected,
	#[error("source lease binding belongs to another transition instance")]
	TransitionInstanceMismatch,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum LeaseSourceInitialPolicyError {
	#[error("initial source Lease policy does not bind its declaration")]
	Contract(#[from] StateContractError),
	#[error("initial source Lease policy does not select its SQL implementation")]
	Binding(#[from] LeaseSourceBindingError),
}

type LeaseSourceImportFn =
	fn(&LeaseKey, LeaseSourceRows) -> Result<LeaseSourceImport, LeaseSourceImportError>;

/// Shared actual import and SQL execution implementations, independent of record identity.
pub(crate) struct LeaseSourceBridge {
	import_fn: LeaseSourceImportFn,
	sql: LeaseSourceSqlKernel,
}

impl LeaseSourceBridge {
	pub(crate) fn component_identifier() -> &'static str {
		LEASE_SOURCE_COMPONENT
	}

	pub(crate) fn new() -> Self {
		Self {
			import_fn: import_v10_lease,
			sql: LeaseSourceSqlKernel::new(),
		}
	}

	/// Actual import and SQL implementations establish support without a payload schema.
	pub(crate) fn declaration(&self) -> ComponentDeclaration<'_, Self> {
		ComponentDeclaration::new(ComponentDeclarationFields {
			identifier: Self::component_identifier(),
			fixed_schemas: Vec::new(),
			settings: ComponentSettingsDeclaration::Parameterless,
			semantics: vec![
				SemanticContractDef {
					identifier: LEASE_SOURCE_SEMANTICS,
					transitions: self,
					settings: ComponentSettingsRequirement::Parameterless,
				},
				SemanticContractDef {
					identifier: LEASE_SOURCE_SQL_SEMANTICS,
					transitions: self,
					settings: ComponentSettingsRequirement::Parameterless,
				},
			],
		})
		.expect("source leases declare actual import and SQL execution implementations")
	}

	/// Selects actual coexistence SQL semantics without invoking source operations or import.
	pub(crate) fn initial_sql_policy(
		&self,
	) -> Result<ComponentPolicy, LeaseSourceInitialPolicyError> {
		let policy = ComponentPolicy::new(ComponentPolicyFields {
			component_identifier: LEASE_SOURCE_COMPONENT.to_owned(),
			semantic_contract: LEASE_SOURCE_SQL_SEMANTICS.to_owned(),
			schemas: Vec::new(),
			settings: ComponentSettings::Parameterless,
		})?;
		let declaration = self.declaration();
		self.require_sql_execution(&declaration.bind(&policy)?)?;
		Ok(policy)
	}

	/// Only the selected executable definition can operate the source SQL kernel.
	pub(crate) fn require_sql_execution<'a>(
		&'a self,
		contract: &BoundComponentContract<'_, '_, Self>,
	) -> Result<&'a LeaseSourceSqlKernel, LeaseSourceBindingError> {
		if !std::ptr::eq(self, contract.transitions()) {
			return Err(LeaseSourceBindingError::TransitionInstanceMismatch);
		}
		if contract.policy().semantic_contract() != LEASE_SOURCE_SQL_SEMANTICS {
			return Err(LeaseSourceBindingError::SqlExecutionNotSelected);
		}
		Ok(&self.sql)
	}

	/// Invoke through the bound declaration's transitions after source-aware admission.
	/// An import result alone proves neither admission nor persistence.
	pub(crate) fn import(
		&self,
		requested_key: &LeaseKey,
		source: LeaseSourceRows,
	) -> Result<LeaseSourceImport, LeaseSourceImportError> {
		(self.import_fn)(requested_key, source)
	}
}

fn import_v10_lease(
	key: &LeaseKey,
	source: LeaseSourceRows,
) -> Result<LeaseSourceImport, LeaseSourceImportError> {
	if key != &source.key {
		return Err(LeaseSourceImportError::KeyMismatch);
	}
	let Some(counter) = source.counter else {
		return if source.claim.is_some() {
			Err(LeaseSourceImportError::MissingCounter)
		} else {
			Ok(LeaseSourceImport::Absent)
		};
	};
	let last_fencing_token = parse_fence(
		counter.last_fencing_token,
		LeaseSourceFenceField::CounterLastFence,
	)?;
	let mut aggregate_updated_at = (
		counter.updated_at,
		LeaseSourceTimestampField::CounterUpdatedAt,
	);
	let claim = source
		.claim
		.map(|claim| {
			let fencing_token =
				parse_fence(claim.fencing_token, LeaseSourceFenceField::ClaimFence)?;
			if fencing_token > last_fencing_token {
				return Err(LeaseSourceImportError::CounterBelowClaim);
			}
			if claim.updated_at > aggregate_updated_at.0 {
				aggregate_updated_at =
					(claim.updated_at, LeaseSourceTimestampField::ClaimUpdatedAt);
			}
			Ok(LeasePreservedClaim {
				holder_id: claim.holder_id,
				fencing_token,
				lease_token: claim.lease_token,
				expires_at: claim
					.expires_at
					.into_native(LeaseSourceTimestampField::ClaimExpiry)?,
			})
		})
		.transpose()?;
	let updated_at = aggregate_updated_at.0.into_native(aggregate_updated_at.1)?;
	let state = LeaseState::from_preserved_parts(LeasePreservedParts {
		last_fencing_token,
		claim,
	})
	.map_err(LeaseSourceImportError::NativeState)?;
	Ok(LeaseSourceImport::Present {
		state,
		metadata: RecordMetadata {
			expires_at: None,
			updated_at,
		},
	})
}

fn parse_fence(
	value: i64,
	field: LeaseSourceFenceField,
) -> Result<FencingToken, LeaseSourceImportError> {
	FencingToken::from_i64(value).map_err(|_| LeaseSourceImportError::NonpositiveFence { field })
}
