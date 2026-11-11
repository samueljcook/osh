//! Complete requested policies derived from explicit choices and actual Runtime code.

use std::sync::Arc;

use crate::fleet::counter::state::{CounterInitialPolicyError, CounterUpgradeSelection};
use crate::fleet::lease::{LeaseBindingError, LeaseSourceInitialPolicyError};
use crate::fleet::scalar::{
	FleetScalarInitialOrigin, FleetScalarInitialPolicyFields, NativeFleetScalarCodeError,
};
use crate::installation::administration::{
	NativeAdministrationError, PgInitialAdministrationPolicyFields, PgNativeAdministration,
};
use crate::installation::contract::StateContractError;
use crate::queue::{
	QueueGroupError, QueueInitialSourcePoliciesFields, QueueInitialSourcePolicyError,
	QueueJobInitialPolicyFields, QueueRecoveryPolicy,
};
use crate::runtime::admission::bindings::PgInstallationAdmissionBinding;
use crate::runtime::targets::NativeRuntimeTargetCode;
use crate::scope::ScopePolicy;
use crate::scope::selection::{ScopeSelectionError, ScopeSelections};
use crate::upgrade::policy::scope::ScopeUpgradeCandidatePolicies;
use crate::upgrade::policy::{
	CandidateComponentPolicy, InstallationUpgradeCandidate, UpgradeCandidatePolicies,
	UpgradeCandidatePoliciesFields, UpgradeCompatibilityError,
};

/// Whether the requested final target keeps the code that reads and writes v10 data.
///
/// A `NativeOnly` target requires the retirement and component-withdrawal procedures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CoreSourceRetention {
	/// The target keeps the published Counter representation, the v10 Queue source components
	/// in the installation's observed layout, the Fleet scalar source bridge, and the source
	/// Lease bridge.
	RetainV10,
	/// The target uses only the native Counter representation and omits every v10 source
	/// component and bridge.
	NativeOnly,
}

/// A complete core target expressed through actual code and application choices.
///
/// These choices request final meaning. Fixed procedures must establish every effective
/// change under the installation's real control and participant inspection.
pub(crate) struct UpgradeCoreSelection {
	/// Whether the target keeps the code that reads and writes v10 data.
	pub(crate) source: CoreSourceRetention,
	/// The Queue's stale-claim recovery settings.
	pub(crate) queue_recovery: QueueRecoveryPolicy,
}

pub(super) struct UpgradeTargetAssemblyFields<'a> {
	pub(super) code: &'a Arc<NativeRuntimeTargetCode>,
	pub(super) core: UpgradeCoreSelection,
	pub(super) scopes: Option<ScopeSelections>,
}

/// Pure binding supplies no current control observation, effective policy or IO authority.
pub(super) fn assemble_target(
	fields: UpgradeTargetAssemblyFields<'_>,
) -> Result<InstallationUpgradeCandidate, UpgradeTargetSelectionError> {
	let code = fields.code;
	let core = fields.core;
	let imported = matches!(
		code.installation().as_ref(),
		PgInstallationAdmissionBinding::Imported { .. }
	);
	if !imported && core.source == CoreSourceRetention::RetainV10 {
		return Err(UpgradeTargetSelectionError::ImportedSourceRequired);
	}
	let counter_selection = match core.source {
		CoreSourceRetention::RetainV10 => CounterUpgradeSelection::PublishedCoexistence,
		CoreSourceRetention::NativeOnly => CounterUpgradeSelection::NativeOnly,
	};

	let scopes = assemble_scopes(code, fields.scopes)?;
	let administration = code.administration();
	let administrative_policy =
		PgNativeAdministration::initial_policy_from_schemas(PgInitialAdministrationPolicyFields {
			participant: administration.participant_schema(),
			control: administration.control_component(),
			backend: administration.backend_schema(),
		})?;
	let counter = &code.counters().component;
	let raw = &code.raw_kv().component;
	let lease = &code.leases().component;
	let ledger = crate::installation::component_ledger::ComponentLedgerComponent::new();
	let mut candidates = vec![
		CandidateComponentPolicy::new(
			&counter.declaration(),
			counter.upgrade_policy(counter_selection)?,
		)?,
		CandidateComponentPolicy::new(&raw.declaration(), raw.initial_policy()?)?,
		CandidateComponentPolicy::new(&lease.declaration(), lease.initial_policy()?)?,
		CandidateComponentPolicy::new(&ledger.declaration(), ledger.initial_policy()?)?,
	];
	if core.source == CoreSourceRetention::RetainV10 {
		let source = code.lease_source_bridge();
		candidates.push(CandidateComponentPolicy::new(
			&source.declaration(),
			source.initial_sql_policy()?,
		)?);
	}
	{
		let scalar = code.fleet_scalar();
		for policy in scalar
			.code
			.initial_policies(FleetScalarInitialPolicyFields {
				origin: match core.source {
					CoreSourceRetention::RetainV10 => FleetScalarInitialOrigin::ImportedCoexisting,
					CoreSourceRetention::NativeOnly => FleetScalarInitialOrigin::NativeFresh,
				},
			})? {
			candidates.push(
				scalar
					.code
					.candidate(&policy)?
					.ok_or(UpgradeTargetSelectionError::DerivedComponentAbsent)?,
			);
		}
	}
	{
		let recovery = &core.queue_recovery;
		let queue = code.queue();
		let jobs = queue
			.jobs
			.initial_policy(QueueJobInitialPolicyFields {
				recovery,
				representation: match core.source {
					CoreSourceRetention::RetainV10 => {
						crate::queue::upgrade::QueueJobRepresentation::InlinePayload
					}
					CoreSourceRetention::NativeOnly => {
						crate::queue::upgrade::QueueJobRepresentation::SeparatePayload
					}
				},
			})
			.map_err(QueueGroupError::JobBinding)?;
		let auxiliary = queue.auxiliary.initial_policy()?;
		candidates.push(CandidateComponentPolicy::new(
			&queue.jobs.declaration(),
			jobs,
		)?);
		candidates.push(CandidateComponentPolicy::new(
			&queue.auxiliary.declaration(),
			auxiliary,
		)?);
	}
	if core.source == CoreSourceRetention::RetainV10 {
		let definitions = code
			.queue_source_imports()
			.ok_or(UpgradeTargetSelectionError::QueueSourceCodeMissing)?;
		let PgInstallationAdmissionBinding::Imported { source, .. } = code.installation().as_ref()
		else {
			return Err(UpgradeTargetSelectionError::ImportedSourceRequired);
		};
		let policies = definitions.initial_source_selection().initial_policies(
			QueueInitialSourcePoliciesFields {
				layout: source.layout().queue_layout(),
			},
		)?;
		candidates.push(CandidateComponentPolicy::new(
			&definitions.job_declaration(),
			policies.jobs,
		)?);
		candidates.push(CandidateComponentPolicy::new(
			&definitions.auxiliary_declaration(),
			policies.auxiliary,
		)?);
	}
	let declaration = administration.declaration();
	let core = UpgradeCandidatePolicies::new(UpgradeCandidatePoliciesFields {
		administration: &declaration,
		administrative_policy,
		other_policies: candidates,
	})?;
	Ok(match scopes {
		Some(scopes) => InstallationUpgradeCandidate::core_and_scopes(core, scopes),
		None => InstallationUpgradeCandidate::core(core),
	})
}

fn assemble_scopes(
	code: &NativeRuntimeTargetCode,
	explicit: Option<ScopeSelections>,
) -> Result<Option<ScopeUpgradeCandidatePolicies>, UpgradeTargetSelectionError> {
	let policies: Vec<ScopePolicy> = explicit
		.map(ScopeSelections::into_upgrade_candidate_policies)
		.transpose()?
		.map(|scopes| scopes.targets().policies().to_vec())
		.unwrap_or_else(Vec::new);
	if policies.is_empty() {
		return Ok(None);
	}
	let candidates = policies
		.into_iter()
		.map(|policy| code.scopes().bind_final_candidate(policy))
		.collect::<Result<Vec<_>, _>>()?;
	ScopeUpgradeCandidatePolicies::new(candidates)
		.map(Some)
		.map_err(UpgradeCompatibilityError::from)
		.map_err(UpgradeTargetSelectionError::from)
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum UpgradeTargetSelectionError {
	#[error("upgrade physical source choices require an imported installation")]
	ImportedSourceRequired,
	#[error("upgrade Queue source selection requires source definitions")]
	QueueSourceCodeMissing,
	#[error("upgrade owning code did not recognize its derived component")]
	DerivedComponentAbsent,
	#[error("upgrade administration does not bind its schemas")]
	Administration(#[from] NativeAdministrationError),
	#[error("upgrade Counter choice does not bind its representations")]
	Counter(#[from] CounterInitialPolicyError),
	#[error("upgrade native Lease policy does not bind its code")]
	Lease(#[from] LeaseBindingError),
	#[error("upgrade source Lease policy does not bind its SQL implementation")]
	SourceLease(#[from] LeaseSourceInitialPolicyError),
	#[error("upgrade scalar policies do not bind their code")]
	Scalar(#[from] NativeFleetScalarCodeError),
	#[error("upgrade native Queue policies do not bind their recovery settings")]
	Queue(#[from] QueueGroupError),
	#[error("upgrade Queue source policies do not bind the observed layout")]
	QueueSourcePolicy(#[from] QueueInitialSourcePolicyError),
	#[error("upgrade explicit scope selections are invalid")]
	Scopes(#[from] ScopeSelectionError),
	#[error("upgrade scope choices do not bind the Runtime's code")]
	ScopeCode(#[from] crate::scope::managed::ManagedScopeRegistryError),
	#[error("upgrade derived component policy does not bind its declaration")]
	Contract(#[from] StateContractError),
	#[error("upgrade complete target does not bind its code")]
	Candidate(#[from] UpgradeCompatibilityError),
}
