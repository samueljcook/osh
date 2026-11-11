//! Exact current importer omissions derived from retained code and the complete Work target.

use crate::fleet::lease::LeaseSourceBridge;
use crate::fleet::v10::scalar_source_bridge::ScalarFleetSourceBridge;
use crate::installation::contract::ComponentPolicy;
use crate::installation::control_state::InstallationControlState;
use crate::queue::{QueueAuxiliarySourceBridge, QueueSourceBridge};
use crate::runtime::admission::bindings::PgInstallationAdmissionBinding;
use crate::runtime::targets::NativeRuntimeTargetCode;
use crate::upgrade::work::CompleteUpgradeTarget;

use super::SourceComponentWithdrawalError;

pub(in crate::upgrade::participants) struct SourceComponentPreparationFields<'a> {
	pub(in crate::upgrade::participants) current: &'a InstallationControlState,
	pub(in crate::upgrade::participants) target: &'a CompleteUpgradeTarget,
	pub(in crate::upgrade::participants) code: &'a NativeRuntimeTargetCode,
}
pub(in crate::upgrade::participants) struct PreparedSourceComponents {
	pub(in crate::upgrade::participants) policies: Vec<ComponentPolicy>,
	pub(in crate::upgrade::participants) removed: Vec<String>,
}
impl PreparedSourceComponents {
	pub(in crate::upgrade::participants) fn new(
		fields: SourceComponentPreparationFields<'_>,
	) -> Result<Self, SourceComponentWithdrawalError> {
		let final_policies = fields.target.request().policies();
		let omitted = |identifier: &str| {
			!final_policies
				.iter()
				.any(|policy| policy.component_identifier() == identifier)
		};
		let mut result = Self {
			policies: Vec::new(),
			removed: Vec::new(),
		};
		let queue_jobs = QueueSourceBridge::component_identifier();
		let queue_auxiliary = QueueAuxiliarySourceBridge::component_identifier();
		if [queue_jobs, queue_auxiliary]
			.into_iter()
			.any(|component| omitted(component) && fields.current.component(component).is_some())
		{
			if !omitted(queue_jobs) || !omitted(queue_auxiliary) {
				return Err(SourceComponentWithdrawalError::QueuePairRequired);
			}
			let jobs = fields
				.current
				.component(queue_jobs)
				.ok_or(SourceComponentWithdrawalError::QueuePairRequired)?;
			let auxiliary = fields
				.current
				.component(queue_auxiliary)
				.ok_or(SourceComponentWithdrawalError::QueuePairRequired)?;
			let imports = fields.code.queue_source_imports().ok_or_else(|| {
				SourceComponentWithdrawalError::CodeMissing {
					component: queue_jobs.to_owned(),
				}
			})?;
			let job_declaration = imports.job_declaration();
			let auxiliary_declaration = imports.auxiliary_declaration();
			let jobs = job_declaration.bind(jobs).map_err(|error| {
				SourceComponentWithdrawalError::Contract {
					component: queue_jobs.to_owned(),
					source: error,
				}
			})?;
			let auxiliary = auxiliary_declaration.bind(auxiliary).map_err(|error| {
				SourceComponentWithdrawalError::Contract {
					component: queue_auxiliary.to_owned(),
					source: error,
				}
			})?;
			let PgInstallationAdmissionBinding::Imported { source, .. } =
				fields.code.installation().as_ref()
			else {
				return Err(SourceComponentWithdrawalError::NativeImportedRequired);
			};
			imports
				.require_bound_pair(&jobs, &auxiliary, source.layout().queue_layout())
				.map_err(|error| SourceComponentWithdrawalError::Queue(Box::new(error)))?;
		}
		for policy in fields.current.component_policies() {
			let component = policy.component_identifier();
			if !omitted(component) {
				result.policies.push(policy.clone());
				continue;
			}
			let contract_error = |error| SourceComponentWithdrawalError::Contract {
				component: component.to_owned(),
				source: error,
			};
			if component == LeaseSourceBridge::component_identifier() {
				// Both declared physical semantics retire; SQL execution is not required for import-only code.
				let declaration = fields.code.lease_source_bridge().declaration();
				declaration.bind(policy).map_err(contract_error)?;
			} else if component == queue_jobs || component == queue_auxiliary {
				// The complete omitted pair is bound before individual components are prepared.
			} else if component == ScalarFleetSourceBridge::component_identifier() {
				let scalar = fields.code.fleet_scalar();
				let declaration = scalar.code.source.declaration();
				let bound = declaration.bind(policy).map_err(contract_error)?;
				scalar
					.code
					.source
					.bind(&bound)
					.map_err(SourceComponentWithdrawalError::Scalar)?;
			} else {
				return Err(SourceComponentWithdrawalError::UnsupportedOmission {
					component: component.to_owned(),
				});
			}
			result.removed.push(component.to_owned());
		}
		Ok(result)
	}
}
