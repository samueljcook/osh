//! Initial metadata derived from actual code, explicit choices and the installation's real control.

use crate::fleet::counter::state::{
	CounterComponent, CounterInitialOrigin, CounterInitialPolicyError, CounterInitialPolicyFields,
};
use crate::fleet::lease::{
	LeaseBindingError, LeaseComponent, LeaseSourceBridge, LeaseSourceInitialPolicyError,
};
use crate::fleet::scalar::{
	FleetScalarCode, FleetScalarInitialOrigin, FleetScalarInitialPolicyFields,
	NativeFleetScalarCodeError,
};
use crate::installation::contract::{ComponentPolicy, StateContractError};
use crate::installation::control_state::InstallationControlComponent;
use crate::kv::raw::core_component::CoreRawComponent;
use crate::queue::{
	QueueAuxiliaryComponent, QueueGroupError, QueueInitialSourcePolicyError, QueueJobComponent,
	QueueJobInitialPolicyFields, QueueRecoveryPolicy,
};
use crate::runtime::participation::state::ParticipantSchema;
use crate::scope::selection::ScopeSelectionError;

use super::{NativeMappingOrigin, PgBackendRegistrationSchema};
use crate::installation::administration::{
	NativeAdministrationError, PgInitialAdministrationPolicyFields, PgNativeAdministration,
};

pub(super) struct InitialComponentPolicyFields<'a> {
	pub(super) participant: &'a ParticipantSchema,
	pub(super) control: &'a InstallationControlComponent,
	pub(super) backend: &'a PgBackendRegistrationSchema,
	pub(super) scalar_code: &'a FleetScalarCode,
	pub(super) queue_recovery: &'a QueueRecoveryPolicy,
	pub(super) origin: NativeMappingOrigin,
}

/// Fixed native policies are independent from application codecs and source layout observation.
pub(super) fn component_policies(
	fields: InitialComponentPolicyFields<'_>,
) -> Result<Vec<ComponentPolicy>, InitialPolicyError> {
	let counter = CounterComponent::new();
	let lease = LeaseComponent::new();
	let raw = CoreRawComponent::new();
	let mut policies = vec![
		PgNativeAdministration::initial_policy_from_schemas(PgInitialAdministrationPolicyFields {
			participant: fields.participant,
			control: fields.control,
			backend: fields.backend,
		})?,
		counter.initial_policy(CounterInitialPolicyFields {
			origin: match fields.origin {
				NativeMappingOrigin::NativeFresh => CounterInitialOrigin::NativeFresh,
				NativeMappingOrigin::ImportedV10 => CounterInitialOrigin::ImportedCoexisting,
			},
		})?,
		lease.initial_policy()?,
		crate::installation::component_ledger::ComponentLedgerComponent::new()
			.initial_policy()
			.map_err(InitialPolicyError::ComponentLedger)?,
		raw.initial_policy()?,
	];
	if let NativeMappingOrigin::ImportedV10 = fields.origin {
		policies.push(LeaseSourceBridge::new().initial_sql_policy()?);
	}
	policies.extend(
		fields
			.scalar_code
			.initial_policies(FleetScalarInitialPolicyFields {
				origin: match fields.origin {
					NativeMappingOrigin::NativeFresh => FleetScalarInitialOrigin::NativeFresh,
					NativeMappingOrigin::ImportedV10 => {
						FleetScalarInitialOrigin::ImportedCoexisting
					}
				},
			})?,
	);
	policies.push(
		QueueJobComponent::new()
			.initial_policy(QueueJobInitialPolicyFields {
				recovery: fields.queue_recovery,
				representation: match fields.origin {
					NativeMappingOrigin::NativeFresh => {
						crate::queue::upgrade::QueueJobRepresentation::SeparatePayload
					}
					NativeMappingOrigin::ImportedV10 => {
						crate::queue::upgrade::QueueJobRepresentation::InlinePayload
					}
				},
			})
			.map_err(QueueGroupError::JobBinding)?,
	);
	policies.push(QueueAuxiliaryComponent::new().initial_policy()?);
	Ok(policies)
}

/// Pure selection and code-binding failures; no database operation occurs in this assembly.
#[derive(Debug, thiserror::Error)]
pub(crate) enum InitialPolicyError {
	#[error("initial application scope selection cannot be installed")]
	Scopes(#[from] ScopeSelectionError),
	#[error("initial administration policy cannot bind its three schemas")]
	Administration(#[from] NativeAdministrationError),
	#[error("initial Counter policy cannot bind its storage origin")]
	Counter(#[from] CounterInitialPolicyError),
	#[error("initial component schema ledger policy cannot bind its code")]
	ComponentLedger(#[source] StateContractError),
	#[error("initial native Lease policy cannot bind its code")]
	Lease(#[from] LeaseBindingError),
	#[error("initial source Lease policy cannot bind its SQL execution")]
	SourceLease(#[from] LeaseSourceInitialPolicyError),
	#[error("initial Fleet scalar policies cannot bind their code")]
	Scalar(#[from] NativeFleetScalarCodeError),
	#[error("initial native Queue policies cannot bind code and recovery settings")]
	Queue(#[from] QueueGroupError),
	#[error("initial imported Queue policies cannot bind the selected code and observed layout")]
	SourceQueue(#[from] QueueInitialSourcePolicyError),
	#[error("initial core policy cannot bind its declaration")]
	Contract(#[from] StateContractError),
}
