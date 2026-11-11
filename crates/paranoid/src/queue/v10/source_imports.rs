//! The v10 Queue source readers that an installation's recorded component policies select.

use super::source_auxiliary_bridge::{QueueAuxiliarySourceBridge, QueueAuxiliarySourceImportError};
use super::source_bridge::{QueueSourceBridge, QueueSourceImportError};
use crate::installation::contract::{
	BoundComponentContract, ComponentDeclaration, ComponentDeclarationFields,
	ComponentSettingsDeclaration,
};
use crate::queue::PublishedV10QueueLayout;
use std::fmt;

mod initial;
pub(crate) use initial::{
	QueueInitialSourcePoliciesFields, QueueInitialSourcePolicyError, QueueInitialSourceSelection,
};

struct QueueSourceImportDef {
	jobs: QueueSourceBridge,
	auxiliary: QueueAuxiliarySourceBridge,
}

/// The v10 Queue source readers, without a selected policy or transaction authority.
pub(crate) struct QueueSourceImportDefs {
	def: QueueSourceImportDef,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum QueueSourceImportBindingError {
	#[error("Queue source job and auxiliary policies do not select one definition pair")]
	DefPairMismatch,
	#[error("Queue source selected job definition does not support the physical layout")]
	Job(#[from] QueueSourceImportError),
	#[error("Queue source selected auxiliary definition cannot bind its reader")]
	Auxiliary(#[from] QueueAuxiliarySourceImportError),
}

impl QueueSourceImportDefs {
	/// Builds the paired v10 job and auxiliary readers. The job readers define distinct
	/// published and prepared semantics, and the auxiliary readers define one semantics.
	pub(crate) fn new() -> Self {
		Self {
			def: QueueSourceImportDef {
				jobs: QueueSourceBridge::new(),
				auxiliary: QueueAuxiliarySourceBridge::new(),
			},
		}
	}

	pub(crate) fn job_declaration(&self) -> ComponentDeclaration<'_, QueueSourceBridge> {
		ComponentDeclaration::new(ComponentDeclarationFields {
			identifier: QueueSourceBridge::component_identifier(),
			fixed_schemas: Vec::new(),
			settings: ComponentSettingsDeclaration::Parameterless,
			semantics: self.def.jobs.semantic_defs().into(),
		})
		.expect("distinct source job definitions form a schema-free declaration")
	}

	pub(crate) fn auxiliary_declaration(
		&self,
	) -> ComponentDeclaration<'_, QueueAuxiliarySourceBridge> {
		ComponentDeclaration::new(ComponentDeclarationFields {
			identifier: QueueAuxiliarySourceBridge::component_identifier(),
			fixed_schemas: Vec::new(),
			settings: ComponentSettingsDeclaration::Parameterless,
			semantics: self.def.auxiliary.semantic_defs().into(),
		})
		.expect("a source auxiliary definition forms a schema-free declaration")
	}

	/// Both bindings must select this registry's definition pair, and the job reader must
	/// support the recorded physical layout.
	pub(crate) fn require_bound_pair(
		&self,
		jobs: &BoundComponentContract<'_, '_, QueueSourceBridge>,
		auxiliary: &BoundComponentContract<'_, '_, QueueAuxiliarySourceBridge>,
		layout: PublishedV10QueueLayout,
	) -> Result<(), QueueSourceImportBindingError> {
		if !std::ptr::eq(&self.def.jobs, jobs.transitions())
			|| !std::ptr::eq(&self.def.auxiliary, auxiliary.transitions())
		{
			return Err(QueueSourceImportBindingError::DefPairMismatch);
		}
		self.def.jobs.require_bound_layout(jobs, layout)?;
		self.def.auxiliary.bind(auxiliary)?;
		Ok(())
	}
}

impl fmt::Debug for QueueSourceImportDefs {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("QueueSourceImportDefs")
			.finish_non_exhaustive()
	}
}
