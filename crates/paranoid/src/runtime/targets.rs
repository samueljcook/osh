//! Pure upgrade-target resolution against retained actual runtime code.
//!
//! Code binding provides no admission, current-generation observation, source exclusion,
//! data coverage, readiness, or executable procedure. The retained owner contains no runtime
//! lifecycle owner, and keeping its code does not activate a component use or source IO.

use std::{error::Error, fmt, sync::Arc};

use crate::fleet::counter::engine::NativeCounterBindings;
use crate::fleet::lease::{LeaseSourceBridge, NativeLeaseBindings};
use crate::fleet::scalar::FleetScalarCode;
use crate::installation::administration::PgNativeAdministration;
use crate::installation::contract::{ComponentPolicy, StateContractError};
use crate::kv::document::schema::managed::ManagedDocumentRegistry;
use crate::kv::raw::engine::store::NativeRawKvBindings;
use crate::queue::engine::PgNativeQueueGroup;
use crate::queue::{
	QueueAuxiliaryComponent, QueueAuxiliarySourceBridge, QueueJobComponent, QueueSourceBridge,
	QueueSourceImportDefs,
};
use crate::records::postgres::v10::relocating_point::PgRelocatingPointStorage;
use crate::runtime::admission::bindings::PgInstallationAdmissionBinding;
use crate::scope::managed::{ManagedScopeRegistry, ManagedScopeRegistryError};
use crate::upgrade::policy::scope::ScopeUpgradeCandidatePolicies;
use crate::upgrade::policy::{
	CandidateComponentPolicy, InstallationUpgradeCandidate, UpgradeCandidatePolicies,
	UpgradeCandidatePoliciesFields, UpgradeCompatibilityError,
};
use crate::upgrade::work::{CompleteUpgradeTarget, UpgradeSourceOrigin, UpgradeWorkError};

/// Exact configured native Queue code; source assignment definitions remain a separate owner.
pub(crate) struct NativeRuntimeTargetQueueCode {
	pub(crate) jobs: Arc<QueueJobComponent>,
	pub(crate) auxiliary: Arc<QueueAuxiliaryComponent>,
	pub(crate) store: PgNativeQueueGroup,
}

pub(crate) struct NativeRuntimeTargetFleetScalarCode {
	pub(crate) code: Arc<FleetScalarCode>,
	pub(crate) source: Option<PgRelocatingPointStorage>,
}

/// Actual owners supplied together, without an independent capability or component inventory.
pub(crate) struct NativeRuntimeTargetCodeFields {
	pub(crate) administration: Arc<PgNativeAdministration>,
	pub(crate) installation: Arc<PgInstallationAdmissionBinding>,
	pub(crate) counters: Arc<NativeCounterBindings>,
	pub(crate) raw_kv: Arc<NativeRawKvBindings>,
	pub(crate) leases: Arc<NativeLeaseBindings>,
	pub(crate) lease_source_bridge: Arc<LeaseSourceBridge>,
	pub(crate) queue: NativeRuntimeTargetQueueCode,
	pub(crate) queue_source_imports: Option<Arc<QueueSourceImportDefs>>,
	pub(crate) fleet_scalar: NativeRuntimeTargetFleetScalarCode,
	pub(crate) scopes: Arc<ManagedScopeRegistry>,
}

/// Callable code is retained independently of runtime lifecycle ownership and active uses.
pub(crate) struct NativeRuntimeTargetCode {
	fields: NativeRuntimeTargetCodeFields,
}

impl NativeRuntimeTargetCode {
	pub(crate) fn new(
		fields: NativeRuntimeTargetCodeFields,
	) -> Result<Self, NativeRuntimeTargetError> {
		let native = match fields.installation.as_ref() {
			PgInstallationAdmissionBinding::Fresh { native }
			| PgInstallationAdmissionBinding::Imported { native, .. } => native,
		};
		if !Arc::ptr_eq(native, fields.administration.native_bindings()) {
			return Err(NativeRuntimeTargetError::target(
				NativeRuntimeTargetResolutionError::AdministrationAssociationMismatch,
			));
		}
		if let Some(source) = fields.fleet_scalar.source.as_ref() {
			fields
				.installation
				.verify_fleet_scalar_source_pair(source)
				.map_err(|error| {
					NativeRuntimeTargetError::target(NativeRuntimeTargetResolutionError::Code(
						Box::new(error),
					))
				})?;
		}

		Ok(Self { fields })
	}

	pub(crate) fn administration(&self) -> &Arc<PgNativeAdministration> {
		&self.fields.administration
	}
	pub(crate) fn installation(&self) -> &Arc<PgInstallationAdmissionBinding> {
		&self.fields.installation
	}
	pub(crate) fn document_registry(&self) -> &Arc<ManagedDocumentRegistry> {
		self.fields.scopes.document_registry()
	}
	pub(crate) fn scopes(&self) -> &Arc<ManagedScopeRegistry> {
		&self.fields.scopes
	}

	pub(crate) fn counters(&self) -> &Arc<NativeCounterBindings> {
		&self.fields.counters
	}
	pub(crate) fn raw_kv(&self) -> &Arc<NativeRawKvBindings> {
		&self.fields.raw_kv
	}
	pub(crate) fn leases(&self) -> &Arc<NativeLeaseBindings> {
		&self.fields.leases
	}
	pub(crate) fn lease_source_bridge(&self) -> &Arc<LeaseSourceBridge> {
		&self.fields.lease_source_bridge
	}
	pub(crate) fn queue(&self) -> &NativeRuntimeTargetQueueCode {
		&self.fields.queue
	}
	pub(crate) fn queue_source_imports(&self) -> Option<&Arc<QueueSourceImportDefs>> {
		self.fields.queue_source_imports.as_ref()
	}
	pub(crate) fn fleet_scalar(&self) -> &NativeRuntimeTargetFleetScalarCode {
		&self.fields.fleet_scalar
	}

	/// Resolves one immutable target without invoking application payload functions or IO.
	pub(crate) fn resolve(
		self: &Arc<Self>,
		target: Arc<CompleteUpgradeTarget>,
	) -> Result<ResolvedNativeRuntimeTarget, NativeRuntimeTargetError> {
		let fields = &self.fields;
		let origin = match fields.installation.as_ref() {
			PgInstallationAdmissionBinding::Fresh { .. } => UpgradeSourceOrigin::Fresh,
			PgInstallationAdmissionBinding::Imported { .. } => UpgradeSourceOrigin::Imported,
		};
		target.require_origin(origin).map_err(|error| {
			NativeRuntimeTargetError::target(NativeRuntimeTargetResolutionError::Target(error))
		})?;
		if target.request().identity().installation_id
			!= fields.administration.control_component().installation_id()
		{
			return Err(NativeRuntimeTargetError::target(
				NativeRuntimeTargetResolutionError::InstallationMismatch,
			));
		}
		let policies = target.request().policies();
		let administration = fields.administration.declaration();
		let counter = fields.counters.component.declaration();
		let raw = fields.raw_kv.component.declaration();
		let lease = fields.leases.component.declaration();
		let ledger_code = crate::installation::component_ledger::ComponentLedgerComponent::new();
		let ledger = ledger_code.declaration();
		let source_lease = fields.lease_source_bridge.declaration();
		let (administrative_index, administrative_policy) =
			require_policy(policies, administration.identifier())?;
		require_policy(policies, counter.identifier())?;
		require_policy(policies, raw.identifier())?;
		require_policy(policies, lease.identifier())?;
		require_policy(policies, ledger.identifier())?;
		require_policy(policies, QueueJobComponent::component_identifier())?;
		require_policy(policies, QueueAuxiliaryComponent::component_identifier())?;
		for declaration in [
			fields.fleet_scalar.code.once.declaration().identifier(),
			fields
				.fleet_scalar
				.code
				.cache_epoch
				.declaration()
				.identifier(),
			fields.fleet_scalar.code.topic.declaration().identifier(),
		] {
			require_policy(policies, declaration)?;
		}
		let administrative_binding =
			administration
				.bind(administrative_policy)
				.map_err(|error| {
					NativeRuntimeTargetError::component(
						administrative_index,
						NativeRuntimeTargetResolutionError::Contract(error),
					)
				})?;
		fields
			.administration
			.require_bound_policy(&administrative_binding)
			.map_err(|error| {
				NativeRuntimeTargetError::component(
					administrative_index,
					NativeRuntimeTargetResolutionError::Code(Box::new(error)),
				)
			})?;

		let source_jobs = find_policy(policies, QueueSourceBridge::component_identifier());
		let source_auxiliary =
			find_policy(policies, QueueAuxiliarySourceBridge::component_identifier());
		let source_pair = match (source_jobs, source_auxiliary) {
			(None, None) => None,
			(Some(_), None) => {
				return Err(NativeRuntimeTargetError::missing());
			}
			(None, Some(_)) => {
				return Err(NativeRuntimeTargetError::missing());
			}
			(Some((job_index, jobs)), Some((auxiliary_index, auxiliary))) => {
				let imports = fields.queue_source_imports.as_ref().ok_or_else(|| {
					NativeRuntimeTargetError::component(
						job_index,
						NativeRuntimeTargetResolutionError::ConfiguredCodeMissing,
					)
				})?;
				let PgInstallationAdmissionBinding::Imported { source, .. } =
					fields.installation.as_ref()
				else {
					return Err(NativeRuntimeTargetError::component(
						job_index,
						NativeRuntimeTargetResolutionError::SourceLayoutUnavailable,
					));
				};
				let jobs_declaration = imports.job_declaration();
				let auxiliary_declaration = imports.auxiliary_declaration();
				let jobs_binding = jobs_declaration.bind(jobs).map_err(|error| {
					NativeRuntimeTargetError::component(
						job_index,
						NativeRuntimeTargetResolutionError::Contract(error),
					)
				})?;
				let auxiliary_binding = auxiliary_declaration.bind(auxiliary).map_err(|error| {
					NativeRuntimeTargetError::component(
						auxiliary_index,
						NativeRuntimeTargetResolutionError::Contract(error),
					)
				})?;
				imports
					.require_bound_pair(
						&jobs_binding,
						&auxiliary_binding,
						source.layout().queue_layout(),
					)
					.map_err(|error| {
						NativeRuntimeTargetError::component(
							job_index,
							NativeRuntimeTargetResolutionError::Code(Box::new(error)),
						)
					})?;
				Some((
					CandidateComponentPolicy::from_bound(&jobs_binding),
					CandidateComponentPolicy::from_bound(&auxiliary_binding),
				))
			}
		};

		let mut candidates = Vec::with_capacity(policies.len() - 1);
		for (policy_index, policy) in policies.iter().enumerate() {
			let identifier = policy.component_identifier();
			let fail = |error| NativeRuntimeTargetError::component(policy_index, error);
			let candidate = if identifier == administration.identifier() {
				continue;
			} else if identifier == counter.identifier() {
				let binding = counter
					.bind(policy)
					.map_err(|error| fail(NativeRuntimeTargetResolutionError::Contract(error)))?;
				fields
					.counters
					.component
					.bind_schema(&binding)
					.map_err(|error| fail(NativeRuntimeTargetResolutionError::Contract(error)))?;
				CandidateComponentPolicy::from_bound(&binding)
			} else if identifier == raw.identifier() {
				let binding = raw
					.bind(policy)
					.map_err(|error| fail(NativeRuntimeTargetResolutionError::Contract(error)))?;
				CandidateComponentPolicy::from_bound(&binding)
			} else if identifier == lease.identifier() {
				let binding = lease
					.bind(policy)
					.map_err(|error| fail(NativeRuntimeTargetResolutionError::Contract(error)))?;
				fields
					.leases
					.component
					.require_bound_policy(&binding)
					.map_err(|error| {
						fail(NativeRuntimeTargetResolutionError::Code(Box::new(error)))
					})?;
				CandidateComponentPolicy::from_bound(&binding)
			} else if identifier == ledger.identifier() {
				let binding = ledger
					.bind(policy)
					.map_err(|error| fail(NativeRuntimeTargetResolutionError::Contract(error)))?;
				ledger_code
					.require_bound_policy(&binding)
					.map_err(|error| fail(NativeRuntimeTargetResolutionError::Contract(error)))?;
				CandidateComponentPolicy::from_bound(&binding)
			} else if identifier == source_lease.identifier() {
				let binding = source_lease
					.bind(policy)
					.map_err(|error| fail(NativeRuntimeTargetResolutionError::Contract(error)))?;
				CandidateComponentPolicy::from_bound(&binding)
			} else if identifier == QueueJobComponent::component_identifier() {
				let declaration = fields.queue.jobs.declaration();
				let binding = declaration
					.bind(policy)
					.map_err(|error| fail(NativeRuntimeTargetResolutionError::Contract(error)))?;
				fields
					.queue
					.jobs
					.require_bound_policy(&binding)
					.map_err(|error| {
						fail(NativeRuntimeTargetResolutionError::Code(Box::new(error)))
					})?;
				CandidateComponentPolicy::from_bound(&binding)
			} else if identifier == QueueAuxiliaryComponent::component_identifier() {
				let declaration = fields.queue.auxiliary.declaration();
				let binding = declaration
					.bind(policy)
					.map_err(|error| fail(NativeRuntimeTargetResolutionError::Contract(error)))?;
				fields
					.queue
					.auxiliary
					.require_bound_policy(&binding)
					.map_err(|error| {
						fail(NativeRuntimeTargetResolutionError::Code(Box::new(error)))
					})?;
				CandidateComponentPolicy::from_bound(&binding)
			} else if identifier == QueueSourceBridge::component_identifier()
				|| identifier == QueueAuxiliarySourceBridge::component_identifier()
			{
				continue;
			} else {
				if let Some(candidate) =
					fields
						.fleet_scalar
						.code
						.candidate(policy)
						.map_err(|error| {
							fail(NativeRuntimeTargetResolutionError::Code(Box::new(error)))
						})? {
					candidates.push(candidate);
					continue;
				}
				return Err(fail(NativeRuntimeTargetResolutionError::UnknownComponent));
			};
			candidates.push(candidate);
		}
		if let Some((jobs, auxiliary)) = source_pair {
			candidates.push(jobs);
			candidates.push(auxiliary);
		}
		let core = UpgradeCandidatePolicies::new(UpgradeCandidatePoliciesFields {
			administration: &administration,
			administrative_policy: administrative_policy.clone(),
			other_policies: candidates,
		})
		.map_err(|error| {
			NativeRuntimeTargetError::target(NativeRuntimeTargetResolutionError::Candidate(error))
		})?;
		let resolved_scopes = target
			.request()
			.scope_replacements()
			.map(|scopes| {
				scopes
					.policies()
					.iter()
					.map(|policy| fields.scopes.bind_final_candidate(policy.clone()))
					.collect::<Result<Vec<_>, _>>()
			})
			.transpose()
			.map_err(|error| {
				NativeRuntimeTargetError::target(NativeRuntimeTargetResolutionError::Scope(
					Box::new(error),
				))
			})?;
		let candidate = match resolved_scopes {
			None => InstallationUpgradeCandidate::core(core),
			Some(scopes) => InstallationUpgradeCandidate::core_and_scopes(
				core,
				ScopeUpgradeCandidatePolicies::new(scopes).map_err(|error| {
					NativeRuntimeTargetError::target(NativeRuntimeTargetResolutionError::Candidate(
						error.into(),
					))
				})?,
			),
		};
		if !target.matches_candidate(&candidate) {
			return Err(NativeRuntimeTargetError::target(
				NativeRuntimeTargetResolutionError::TargetMismatch,
			));
		}
		Ok(ResolvedNativeRuntimeTarget {
			code: Arc::clone(self),
			target,
			candidate,
		})
	}
}

fn find_policy<'a>(
	policies: &'a [ComponentPolicy],
	identifier: &str,
) -> Option<(usize, &'a ComponentPolicy)> {
	policies
		.iter()
		.enumerate()
		.find(|(_, policy)| policy.component_identifier() == identifier)
}
fn require_policy<'a>(
	policies: &'a [ComponentPolicy],
	identifier: &str,
) -> Result<(usize, &'a ComponentPolicy), NativeRuntimeTargetError> {
	find_policy(policies, identifier).ok_or_else(NativeRuntimeTargetError::missing)
}
/// Bound policies and their original target retain actual callable code, without execution proof.
pub(crate) struct ResolvedNativeRuntimeTarget {
	code: Arc<NativeRuntimeTargetCode>,
	target: Arc<CompleteUpgradeTarget>,
	candidate: InstallationUpgradeCandidate,
}
impl ResolvedNativeRuntimeTarget {
	pub(crate) fn code(&self) -> &Arc<NativeRuntimeTargetCode> {
		&self.code
	}
	pub(crate) fn target(&self) -> &Arc<CompleteUpgradeTarget> {
		&self.target
	}
	pub(crate) fn candidate(&self) -> &InstallationUpgradeCandidate {
		&self.candidate
	}
}
impl fmt::Debug for NativeRuntimeTargetCode {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("NativeRuntimeTargetCode { .. }")
	}
}
impl fmt::Debug for ResolvedNativeRuntimeTarget {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("ResolvedNativeRuntimeTarget { .. }")
	}
}

/// Exact native code errors remain inspectable; default formatting omits selected identities.
pub(crate) enum NativeRuntimeTargetResolutionError {
	AdministrationAssociationMismatch,
	InstallationMismatch,
	RequiredPolicyMissing,
	ConfiguredCodeMissing,
	UnknownComponent,
	SourceLayoutUnavailable,
	TargetMismatch,
	Target(UpgradeWorkError),
	Contract(StateContractError),
	Candidate(UpgradeCompatibilityError),
	Scope(Box<ManagedScopeRegistryError>),
	Code(Box<dyn Error + Send + Sync>),
}
impl fmt::Debug for NativeRuntimeTargetResolutionError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(match self {
			Self::AdministrationAssociationMismatch => "AdministrationAssociationMismatch",
			Self::InstallationMismatch => "InstallationMismatch",
			Self::RequiredPolicyMissing => "RequiredPolicyMissing",
			Self::ConfiguredCodeMissing => "ConfiguredCodeMissing",
			Self::UnknownComponent => "UnknownComponent",
			Self::SourceLayoutUnavailable => "SourceLayoutUnavailable",
			Self::TargetMismatch => "TargetMismatch",
			Self::Target(_) => "Target",
			Self::Contract(_) => "Contract",
			Self::Candidate(_) => "Candidate",
			Self::Scope(_) => "Scope",
			Self::Code(_) => "Code",
		})
	}
}
impl fmt::Display for NativeRuntimeTargetResolutionError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "native runtime target resolution rejected {self:?}")
	}
}
impl Error for NativeRuntimeTargetResolutionError {
	fn source(&self) -> Option<&(dyn Error + 'static)> {
		match self {
			Self::Target(error) => Some(error),
			Self::Contract(error) => Some(error),
			Self::Candidate(error) => Some(error),
			Self::Scope(error) => Some(error.as_ref()),
			Self::Code(error) => Some(error.as_ref()),
			_ => None,
		}
	}
}

pub(crate) struct NativeRuntimeTargetError {
	policy_index: Option<usize>,
	source: NativeRuntimeTargetResolutionError,
}
impl NativeRuntimeTargetError {
	fn target(source: NativeRuntimeTargetResolutionError) -> Self {
		Self {
			policy_index: None,
			source,
		}
	}
	fn missing() -> Self {
		Self {
			policy_index: None,
			source: NativeRuntimeTargetResolutionError::RequiredPolicyMissing,
		}
	}
	fn component(index: usize, source: NativeRuntimeTargetResolutionError) -> Self {
		Self {
			policy_index: Some(index),
			source,
		}
	}
	pub(crate) fn typed_source(&self) -> &NativeRuntimeTargetResolutionError {
		&self.source
	}
}
impl fmt::Debug for NativeRuntimeTargetError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("NativeRuntimeTargetError")
			.field("policy_index", &self.policy_index)
			.field("source", &self.source)
			.finish_non_exhaustive()
	}
}
impl fmt::Display for NativeRuntimeTargetError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("native runtime cannot bind this complete upgrade target")
	}
}
impl Error for NativeRuntimeTargetError {
	fn source(&self) -> Option<&(dyn Error + 'static)> {
		Some(&self.source)
	}
}
