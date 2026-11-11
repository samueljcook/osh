//! Actual Queue code selects the shared Queue invariant.

use crate::queue::engine::conversion::{
	BoundQueueSourceConversion, QueueSourceConversionDeclarations,
};
use crate::runtime::admission::upgrade::contracts::NativeUpgradeContracts;
use crate::runtime::targets::NativeRuntimeTargetCode;
use crate::scope::ScopePolicySet;
use crate::upgrade::work::v10::source_checkpoint::{
	SourceConversionInvariant, SourceConversionInvariantFields,
};

use super::SourceQueueConversionError;

pub(in crate::upgrade::progress) struct SourceQueueDeclarations<'d> {
	declarations: Box<QueueSourceConversionDeclarations<'d>>,
}

pub(in crate::upgrade::progress) struct BoundSourceQueueSelection<'a, 'd, 'current> {
	pub(in crate::upgrade::progress) conversion: Box<BoundQueueSourceConversion<'a, 'd, 'current>>,
	pub(in crate::upgrade::progress) invariant: SourceConversionInvariant,
}

impl<'d> SourceQueueDeclarations<'d> {
	pub(in crate::upgrade::progress) fn new(
		code: &'d NativeRuntimeTargetCode,
	) -> Result<Self, SourceQueueConversionError> {
		let queue = code.queue();
		let imports = code
			.queue_source_imports()
			.ok_or(SourceQueueConversionError::SourceDefsAbsent)?;
		Ok(Self {
			declarations: Box::new(
				queue
					.store
					.source_conversion_declarations(&queue.jobs, &queue.auxiliary, imports)
					.map_err(SourceQueueConversionError::Binding)?,
			),
		})
	}
	pub(in crate::upgrade::progress) fn bind<'a, 'current>(
		&'a self,
		contracts: &'a NativeUpgradeContracts<'current>,
	) -> Result<BoundSourceQueueSelection<'a, 'd, 'current>, SourceQueueConversionError> {
		let bound = self
			.declarations
			.bind(contracts)
			.map_err(SourceQueueConversionError::Binding)?;
		let policies = bound.selected_policies();
		let invariant = SourceConversionInvariant::new(SourceConversionInvariantFields {
			component_policies: policies,
			scope_policies: ScopePolicySet::new(Vec::new())
				.expect("this fixed Queue source selection requires no scoped policy"),
		})?;
		Ok(BoundSourceQueueSelection {
			conversion: Box::new(bound),
			invariant,
		})
	}
}
