//! The core Raw contract and source storage that the v10 Raw relocation uses, and the meaning
//! that its checkpoint records.

use super::SourceRawConversionError;
use crate::installation::contract::{BoundComponentContract, ComponentDeclaration};
use crate::kv::raw::engine::store::RawKvComponent;
use crate::records::postgres::v10::relocating_point::PgRelocatingPointStorage;
use crate::runtime::admission::upgrade::contracts::NativeUpgradeContracts;
use crate::runtime::targets::ResolvedNativeRuntimeTarget;
use crate::scope::ScopePolicySet;
use crate::upgrade::work::v10::source_checkpoint::{
	SourceConversionInvariant, SourceConversionInvariantFields,
};
use std::sync::Arc;

pub(in crate::upgrade::progress) struct SourceRawSelectionFields<'a, 'd, 'current> {
	pub(in crate::upgrade::progress) resolved: &'a ResolvedNativeRuntimeTarget,
	pub(in crate::upgrade::progress) contracts: &'a NativeUpgradeContracts<'current>,
	pub(in crate::upgrade::progress) declaration: &'a ComponentDeclaration<'d, RawKvComponent>,
}
pub(in crate::upgrade::progress) struct BoundSourceRawSelection<'a, 'd> {
	pub(in crate::upgrade::progress) storage: &'a PgRelocatingPointStorage,
	pub(in crate::upgrade::progress) bound: BoundComponentContract<'a, 'd, RawKvComponent>,
	pub(in crate::upgrade::progress) invariant: SourceConversionInvariant,
}
pub(in crate::upgrade::progress) fn bind_source_raw_selection<'a, 'd>(
	fields: SourceRawSelectionFields<'a, 'd, '_>,
) -> Result<BoundSourceRawSelection<'a, 'd>, SourceRawConversionError> {
	let code = fields.resolved.code();
	if !Arc::ptr_eq(code.installation(), fields.contracts.installation_binding()) {
		return Err(SourceRawConversionError::TargetMismatch);
	}
	let raw = code.raw_kv();
	let source = raw
		.source
		.as_ref()
		.ok_or(SourceRawConversionError::SourceStorageAbsent)?;
	let storage = source.relocation();
	fields
		.contracts
		.require_raw_source_storage(storage, &raw.namespace)?;
	let bound = fields.contracts.bind(fields.declaration)?;
	let invariant = SourceConversionInvariant::new(SourceConversionInvariantFields {
		component_policies: vec![bound.policy().clone()],
		scope_policies: ScopePolicySet::new(Vec::new())
			.expect("the Raw relocation selects no scoped policies"),
	})?;
	Ok(BoundSourceRawSelection {
		storage,
		bound,
		invariant,
	})
}
