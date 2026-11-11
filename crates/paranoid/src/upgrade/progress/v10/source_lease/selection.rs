//! The actual lease and source bridge select one complete aggregate invariant.

use crate::fleet::lease::{LeaseComponent, LeaseSourceBridge, NativeLeaseBindings};
use crate::installation::contract::{BoundComponentContract, ComponentDeclaration};
use crate::runtime::admission::upgrade::contracts::NativeUpgradeContracts;
use crate::scope::ScopePolicySet;
use crate::upgrade::work::v10::source_checkpoint::{
	SourceConversionInvariant, SourceConversionInvariantFields,
};

use super::SourceLeaseConversionError;

pub(in crate::upgrade::progress) struct SourceLeaseSelectionFields<'a, 'd, 'current> {
	pub(in crate::upgrade::progress) code: &'a NativeLeaseBindings,
	pub(in crate::upgrade::progress) contracts: &'a NativeUpgradeContracts<'current>,
	pub(in crate::upgrade::progress) native_declaration:
		&'a ComponentDeclaration<'d, LeaseComponent>,
	pub(in crate::upgrade::progress) bridge_declaration:
		&'a ComponentDeclaration<'d, LeaseSourceBridge>,
}

pub(in crate::upgrade::progress) struct BoundSourceLeaseSelection<'a, 'd> {
	pub(in crate::upgrade::progress) native: BoundComponentContract<'a, 'd, LeaseComponent>,
	pub(in crate::upgrade::progress) bridge: BoundComponentContract<'a, 'd, LeaseSourceBridge>,
	pub(in crate::upgrade::progress) invariant: SourceConversionInvariant,
}

pub(in crate::upgrade::progress) fn bind_source_lease_selection<'a, 'd>(
	fields: SourceLeaseSelectionFields<'a, 'd, '_>,
) -> Result<BoundSourceLeaseSelection<'a, 'd>, SourceLeaseConversionError> {
	let source = fields
		.code
		.source
		.as_ref()
		.ok_or(SourceLeaseConversionError::SourceStorageAbsent)?;
	fields
		.contracts
		.require_lease_source_storage(&source.storage, &fields.code.namespace)?;
	let native = fields.contracts.bind(fields.native_declaration)?;
	let bridge = fields.contracts.bind(fields.bridge_declaration)?;
	fields.code.component.require_bound_policy(&native)?;
	let invariant = SourceConversionInvariant::new(SourceConversionInvariantFields {
		component_policies: vec![native.policy().clone(), bridge.policy().clone()],
		scope_policies: ScopePolicySet::new(Vec::new())
			.expect("this fixed source procedure selects no scoped policies"),
	})?;
	Ok(BoundSourceLeaseSelection {
		native,
		bridge,
		invariant,
	})
}
