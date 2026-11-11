//! Counter and Fleet scalar code select the Fleet invariant.

use crate::fleet::counter::state::{CounterComponent, CounterSchemaBinding};
use crate::fleet::scalar::FleetScalarCode;
use crate::installation::contract::{BoundComponentContract, ComponentDeclaration};
use crate::runtime::admission::NativeAdmissionError;
use crate::runtime::admission::upgrade::contracts::NativeUpgradeContracts;
use crate::runtime::targets::NativeRuntimeTargetCode;
use crate::scope::ScopePolicySet;
use crate::upgrade::work::v10::source_checkpoint::{
	SourceConversionInvariant, SourceConversionInvariantFields,
};

use super::SourceFleetConversionError;

pub(in crate::upgrade) struct SourceFleetDeclarations<'d> {
	code: &'d NativeRuntimeTargetCode,
	counter: ComponentDeclaration<'d, CounterComponent>,
}

pub(in crate::upgrade) struct BoundSourceFleetSelection<'a, 'd> {
	declarations: &'a SourceFleetDeclarations<'d>,
	contracts: &'a NativeUpgradeContracts<'a>,
	counter: BoundComponentContract<'a, 'd, CounterComponent>,
	invariant: SourceConversionInvariant,
}

impl<'d> SourceFleetDeclarations<'d> {
	pub(in crate::upgrade) fn new(code: &'d NativeRuntimeTargetCode) -> Self {
		Self {
			code,
			counter: code.counters().component.declaration(),
		}
	}
	pub(in crate::upgrade) fn bind<'a>(
		&'a self,
		contracts: &'a NativeUpgradeContracts<'_>,
	) -> Result<BoundSourceFleetSelection<'a, 'd>, SourceFleetConversionError> {
		let counter = contracts.bind(&self.counter)?;
		self.code
			.counters()
			.component
			.bind_schema(&counter)
			.map_err(NativeAdmissionError::Contract)?
			.require_native_writer()?;
		let mut policies = vec![counter.policy().clone()];
		policies.extend(scalar_policies(contracts, &self.code.fleet_scalar().code)?);
		let invariant = SourceConversionInvariant::new(SourceConversionInvariantFields {
			component_policies: policies,
			scope_policies: ScopePolicySet::new(Vec::new())
				.expect("a set of no scope policies is valid"),
		})?;
		Ok(BoundSourceFleetSelection {
			declarations: self,
			contracts,
			counter,
			invariant,
		})
	}
}
impl BoundSourceFleetSelection<'_, '_> {
	pub(in crate::upgrade::progress) fn matches_context(
		&self,
		code: &NativeRuntimeTargetCode,
		contracts: &NativeUpgradeContracts<'_>,
	) -> bool {
		std::ptr::eq(self.declarations.code, code) && std::ptr::eq(self.contracts, contracts)
	}
	pub(in crate::upgrade::progress) fn invariant(&self) -> &SourceConversionInvariant {
		&self.invariant
	}
	pub(in crate::upgrade::progress) fn counter_schema(
		&self,
	) -> Result<CounterSchemaBinding<'_>, SourceFleetConversionError> {
		self.declarations
			.code
			.counters()
			.component
			.bind_schema(&self.counter)
			.map_err(|error| NativeAdmissionError::Contract(error).into())
	}
}

fn scalar_policies(
	contracts: &NativeUpgradeContracts<'_>,
	code: &FleetScalarCode,
) -> Result<Vec<crate::installation::contract::ComponentPolicy>, SourceFleetConversionError> {
	let once = code.once.declaration();
	let epoch = code.cache_epoch.declaration();
	let topic = code.topic.declaration();
	let source = code.source.declaration();
	let once = contracts.bind(&once)?;
	let epoch = contracts.bind(&epoch)?;
	let topic = contracts.bind(&topic)?;
	let source = contracts.bind(&source)?;
	code.once.bind_schema(&once)?;
	code.cache_epoch.bind_schema(&epoch)?;
	code.topic.bind_sequence(&topic)?;
	code.topic.bind_cursor(&topic)?;
	code.source.bind(&source)?;
	Ok(vec![
		once.policy().clone(),
		epoch.policy().clone(),
		topic.policy().clone(),
		source.policy().clone(),
	])
}
