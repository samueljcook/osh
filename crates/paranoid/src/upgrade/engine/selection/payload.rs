//! Actual retained Cache/Topic writer stages and durable payload progress selection.

use crate::runtime::admission::bindings::InstallationStorageRoute;
use crate::upgrade::policy::payload_reader::{
	PrimitivePayloadReaderCompletion, PrimitivePayloadReaderCompletionFields,
};
use crate::upgrade::policy::payload_stage::{
	PrimitivePayloadWriterStage, PrimitivePayloadWriterStageFields,
};
use crate::upgrade::work::payload_checkpoint::PayloadScanProgress;

use super::{
	NativeUpgradeScopePrerequisiteError, NativeUpgradeSelection, PgUpgradeStep,
	ScopeSelectionFields,
};

pub(super) fn collect(
	selection: &mut NativeUpgradeSelection,
	fields: ScopeSelectionFields<'_, '_>,
) {
	let ScopeSelectionFields {
		context: fields,
		current,
		target,
	} = fields;
	let scope = target.identity();
	let stage = match PrimitivePayloadWriterStage::new(PrimitivePayloadWriterStageFields {
		registry: fields.resolved.code().scopes(),
		current,
		target,
	}) {
		Ok(stage) => stage,
		Err(source) => {
			selection.push_scope_prerequisite(
				scope,
				NativeUpgradeScopePrerequisiteError::PayloadStage(source),
			);
			return;
		}
	};
	if !matches!(
		fields.observation.route(),
		InstallationStorageRoute::NativeFresh | InstallationStorageRoute::NativeImported
	) {
		selection.push_scope_prerequisite(
			scope,
			NativeUpgradeScopePrerequisiteError::NativePayloadRoute {
				route: fields.observation.route(),
			},
		);
		return;
	}
	let work = fields
		.observation
		.work()
		.expect("selection checked complete Work");
	let checkpoint = work.payload_checkpoint(scope);
	if !stage.matches_policy(current) {
		if checkpoint.is_some() {
			selection.push_scope_prerequisite(
				scope,
				NativeUpgradeScopePrerequisiteError::StageProgressConflict,
			);
			return;
		}
		selection.steps.push(PgUpgradeStep::PromotePayloadWriter {
			scope: scope.clone(),
		});
		return;
	}
	if checkpoint.is_some_and(|checkpoint| {
		matches!(checkpoint.progress(), PayloadScanProgress::Scanned { .. })
	}) {
		if let Err(source) =
			PrimitivePayloadReaderCompletion::new(PrimitivePayloadReaderCompletionFields {
				writer_stage: &stage,
				current_scopes: fields.observation.control().scope_policies(),
			}) {
			selection.push_scope_prerequisite(
				scope,
				NativeUpgradeScopePrerequisiteError::PayloadCompletion(source),
			);
			return;
		}
		selection.steps.push(PgUpgradeStep::FinalizeScope {
			scope: scope.clone(),
		});
	} else {
		selection.steps.push(PgUpgradeStep::ConvertPayload {
			scope: scope.clone(),
		});
	}
}
