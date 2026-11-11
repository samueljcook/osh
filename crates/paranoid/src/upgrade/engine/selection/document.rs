//! Actual Document stage, Ready-only physical association and whole-map progress selection.

use crate::installation::creation::{DocumentIndexLedgerSlot, NativeInstallationError};
use crate::kv::document::postgres::index::managed::{
	PgManagedDocumentIndexBinding, PgManagedDocumentIndexBindingFields,
};
use crate::kv::document::schema::IndexContractIdentity;
use crate::scope::{ScopeIdentity, ScopeOwner};
use crate::upgrade::policy::document_reader::{
	DocumentReaderCompletion, DocumentReaderCompletionFields,
};
use crate::upgrade::policy::document_readiness::DocumentReadinessStage;
use crate::upgrade::policy::document_stage::{DocumentWriterStage, RetainedDocumentScopeCandidate};
use crate::upgrade::work::document_checkpoint::DocumentScanProgress;
use crate::upgrade::work::document_verification::DocumentVerificationProgress;
use crate::upgrade::work::v10::source_checkpoint::PgV10ConversionProcedure;

use super::{
	IndexOrder, NativeUpgradeIndexPrerequisiteError, NativeUpgradeScopePrerequisiteError,
	NativeUpgradeSelection, PgUpgradeStep, ScopeSelectionFields, compare_indexes,
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
	let bound_target = match RetainedDocumentScopeCandidate::bind(
		fields.resolved.code().document_registry(),
		target.clone(),
	) {
		Ok(target) => target,
		Err(source) => {
			selection.push_scope_prerequisite(
				scope,
				NativeUpgradeScopePrerequisiteError::DocumentStage(source),
			);
			return;
		}
	};
	let stage = match DocumentWriterStage::new(&bound_target, current) {
		Ok(stage) => stage,
		Err(source) => {
			selection.push_scope_prerequisite(
				scope,
				NativeUpgradeScopePrerequisiteError::DocumentStage(source),
			);
			return;
		}
	};
	let work = fields
		.observation
		.work()
		.expect("selection checked complete Work");
	let conversion = work.document_conversion_checkpoint(scope);
	let verification = work.document_verification(scope);
	if !stage.matches_policy(current) {
		if conversion.is_some() || verification.is_some() {
			selection.push_scope_prerequisite(
				scope,
				NativeUpgradeScopePrerequisiteError::StageProgressConflict,
			);
		} else {
			selection.steps.push(PgUpgradeStep::PromoteDocumentWriter {
				scope: scope.clone(),
			});
		}
		return;
	}
	let (ScopeOwner::Document(installed), ScopeOwner::Document(proposed)) =
		(current.owner(), target.owner())
	else {
		unreachable!("actual Document stage requires current and target Document owners")
	};
	let mut ready = proposed.ready_indexes().collect::<Vec<_>>();
	ready.sort_by(|left, right| compare_indexes(IndexOrder { left, right }));
	let mut recorded = Vec::new();
	for index in &ready {
		let descriptor = fields
			.observation
			.backend()
			.document_indexes()
			.iter()
			.find(|entry| entry.scope() == scope && entry.index().identity() == *index);
		let Some(descriptor) = descriptor else {
			if installed.is_index_ready(index) {
				push_index_prerequisite(
					selection,
					scope,
					index,
					NativeUpgradeIndexPrerequisiteError::ReadyLedgerMissing,
				);
			} else {
				selection.steps.push(PgUpgradeStep::RegisterDocumentIndex {
					scope: scope.clone(),
					index: (*index).clone(),
				});
			}
			continue;
		};
		let binding =
			match PgManagedDocumentIndexBinding::new(PgManagedDocumentIndexBindingFields {
				registration: bound_target.registration(),
				index,
				native: fields.resolved.code().administration().native_bindings(),
				index_name: descriptor.index_name().clone(),
			}) {
				Ok(binding) => binding,
				Err(source) => {
					push_index_prerequisite(
						selection,
						scope,
						index,
						NativeUpgradeIndexPrerequisiteError::Binding(source),
					);
					continue;
				}
			};
		match fields
			.observation
			.backend()
			.require_document_index_slot(binding.descriptor())
		{
			Ok(DocumentIndexLedgerSlot::RecordedMatching { .. }) => recorded.push(*index),
			Ok(DocumentIndexLedgerSlot::Absent) => push_index_prerequisite(
				selection,
				scope,
				index,
				NativeUpgradeIndexPrerequisiteError::ReadyLedgerMissing,
			),
			Err(source) => push_index_prerequisite(
				selection,
				scope,
				index,
				NativeUpgradeIndexPrerequisiteError::Ledger(NativeInstallationError::from(source)),
			),
		}
	}
	if verification.is_some() && conversion.is_none() {
		selection.push_scope_prerequisite(
			scope,
			NativeUpgradeScopePrerequisiteError::VerificationConversionMissing,
		);
		return;
	}

	if conversion.is_some_and(|checkpoint| {
		matches!(checkpoint.progress(), DocumentScanProgress::Scanning { .. })
	}) {
		selection.steps.push(PgUpgradeStep::ConvertDocument {
			scope: scope.clone(),
		});
		return;
	}
	let ready_addition = ready.iter().any(|index| !installed.is_index_ready(index));
	let completion = if ready_addition {
		None
	} else {
		match DocumentReaderCompletion::new(DocumentReaderCompletionFields {
			writer_stage: &stage,
			current_scopes: fields.observation.control().scope_policies(),
		}) {
			Ok(completion) => Some(completion),
			Err(source) => {
				selection.push_scope_prerequisite(
					scope,
					NativeUpgradeScopePrerequisiteError::DocumentCompletion(source),
				);
				return;
			}
		}
	};
	if conversion.is_none()
		&& (ready_addition
			|| completion
				.as_ref()
				.is_some_and(|completion| !completion.schema_policy_unchanged()))
	{
		selection.steps.push(PgUpgradeStep::ConvertDocument {
			scope: scope.clone(),
		});
		return;
	}
	if verification.is_some_and(|checkpoint| {
		matches!(
			checkpoint.progress(),
			DocumentVerificationProgress::Scanning { .. }
		)
	}) {
		select_verification(
			selection,
			VerificationSelectionFields {
				scope,
				ready: &ready,
				recorded: &recorded,
			},
		);
		return;
	}
	if ready_addition {
		if verification.is_some_and(|checkpoint| {
			matches!(
				checkpoint.progress(),
				DocumentVerificationProgress::Verified { .. }
			)
		}) {
			for index in recorded {
				if installed.is_index_ready(index) {
					continue;
				}
				match DocumentReadinessStage::new(&bound_target, current, index) {
					Ok(_) => selection.steps.push(PgUpgradeStep::PublishDocumentReady {
						scope: scope.clone(),
						index: index.clone(),
					}),
					Err(source) => selection.push_scope_prerequisite(
						scope,
						NativeUpgradeScopePrerequisiteError::DocumentReadiness(source),
					),
				}
			}
		} else {
			select_verification(
				selection,
				VerificationSelectionFields {
					scope,
					ready: &ready,
					recorded: &recorded,
				},
			);
		}
		return;
	}
	if recorded.len() != ready.len() {
		return;
	}
	let captured = work
		.source_checkpoint(PgV10ConversionProcedure::RawBytesRelocationV1)
		.and_then(|checkpoint| checkpoint.invariant().scope_policies().lookup(scope).ok());
	if captured.is_some_and(|captured| !captured.matches_policy(target)) {
		selection.push_scope_prerequisite(
			scope,
			NativeUpgradeScopePrerequisiteError::StageProgressConflict,
		);
		return;
	}
	selection.steps.push(PgUpgradeStep::FinalizeScope {
		scope: scope.clone(),
	});
}

fn push_index_prerequisite(
	selection: &mut NativeUpgradeSelection,
	scope: &ScopeIdentity,
	index: &IndexContractIdentity,
	source: NativeUpgradeIndexPrerequisiteError,
) {
	selection.push_scope_prerequisite(
		scope,
		NativeUpgradeScopePrerequisiteError::Index {
			index: index.clone(),
			source: Box::new(source),
		},
	);
}

struct VerificationSelectionFields<'a> {
	scope: &'a ScopeIdentity,
	ready: &'a [&'a IndexContractIdentity],
	recorded: &'a [&'a IndexContractIdentity],
}
fn select_verification(
	selection: &mut NativeUpgradeSelection,
	fields: VerificationSelectionFields<'_>,
) {
	let VerificationSelectionFields {
		scope,
		ready,
		recorded,
	} = fields;
	if let Some(index) = recorded.first() {
		selection.steps.push(PgUpgradeStep::VerifyDocument {
			scope: scope.clone(),
			index: (*index).clone(),
		});
	} else if ready.is_empty() {
		selection.push_scope_prerequisite(
			scope,
			NativeUpgradeScopePrerequisiteError::VerificationTargetMissing,
		);
	}
}
