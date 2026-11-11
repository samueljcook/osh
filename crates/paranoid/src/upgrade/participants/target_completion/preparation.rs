//! Supported complete-target component changes derived from actual current and retained code.

use crate::fleet::counter::state::{CounterReaderRetirement, CounterReaderRetirementFields};
use crate::installation::contract::ComponentPolicy;
use crate::installation::control_state::InstallationControlState;
use crate::runtime::admission::bindings::InstallationStorageRoute;
use crate::runtime::targets::NativeRuntimeTargetCode;
use crate::upgrade::work::CompleteUpgradeTarget;

use super::super::v10::source_components::preparation::{
	PreparedSourceComponents, SourceComponentPreparationFields,
};
use super::{NativeUpgradeParticipantsError, TargetCompletionError};

pub(super) struct TargetComponentPreparationFields<'a> {
	pub(super) current: &'a InstallationControlState,
	pub(super) target: &'a CompleteUpgradeTarget,
	pub(super) code: &'a NativeRuntimeTargetCode,
	pub(super) route: InstallationStorageRoute,
}

pub(super) fn prepare_components(
	fields: TargetComponentPreparationFields<'_>,
) -> Result<Vec<ComponentPolicy>, NativeUpgradeParticipantsError> {
	let target = fields.target.request().policies();
	for policy in target {
		if fields
			.current
			.component(policy.component_identifier())
			.is_none()
		{
			return Err(TargetCompletionError::ComponentAdditionRequiresProcedure {
				component: policy.component_identifier().to_owned(),
			}
			.into());
		}
	}
	for policy in fields.current.component_policies() {
		if !target.iter().any(|final_policy| {
			final_policy.component_identifier() == policy.component_identifier()
		}) && fields.route != InstallationStorageRoute::NativeImported
		{
			return Err(TargetCompletionError::DataRetirementRequired {
				component: policy.component_identifier().to_owned(),
			}
			.into());
		}
	}
	let mut prepared = PreparedSourceComponents::new(SourceComponentPreparationFields {
		current: fields.current,
		target: fields.target,
		code: fields.code,
	})?;
	let counter = &fields.code.counters().component;
	let declaration = counter.declaration();
	for policy in &mut prepared.policies {
		let final_policy = target
			.iter()
			.find(|final_policy| {
				final_policy.component_identifier() == policy.component_identifier()
			})
			.expect("source omission preparation retains only target identities");
		if policy.matches_policy(final_policy) {
			continue;
		}
		if policy.component_identifier() != declaration.identifier() {
			return Err(TargetCompletionError::ComponentChangeRequiresProcedure {
				component: policy.component_identifier().to_owned(),
			}
			.into());
		}
		if fields.route != InstallationStorageRoute::NativeImported {
			return Err(TargetCompletionError::DataRetirementRequired {
				component: policy.component_identifier().to_owned(),
			}
			.into());
		}
		*policy = CounterReaderRetirement::new(CounterReaderRetirementFields {
			component: counter,
			current: policy,
			target: final_policy,
		})
		.map_err(|error| TargetCompletionError::Counter(Box::new(error)))?
		.policy()
		.clone();
	}
	Ok(prepared.policies)
}
