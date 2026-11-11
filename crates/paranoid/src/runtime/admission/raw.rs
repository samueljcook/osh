//! Raw scope admission excludes document/primitive ownership and undeclared byte access.

use crate::installation::contract::{BoundComponentContract, ComponentDeclaration};
use crate::kv::raw::engine::store::RawKvComponent;
use crate::scope::{RawRole, RawScopeDeclaration, ScopeError, ScopeOwner};

use super::{NativeAdmissionError, NativeAdmittedContracts};

impl NativeAdmittedContracts {
	pub(crate) fn bind_raw_scope<'a, 'd>(
		&'a self,
		scope: &RawScopeDeclaration,
		declaration: &'a ComponentDeclaration<'d, RawKvComponent>,
		role: RawRole,
	) -> Result<BoundComponentContract<'a, 'd, RawKvComponent>, NativeAdmissionError> {
		let policy = self
			.observation
			.control
			.state()
			.scope_policies()
			.lookup(scope.identity())?;
		let support = self
			.observation
			.participant()?
			.capabilities()
			.raw_roles()
			.lookup(scope.identity(), role)?;
		support.check_policy(policy)?;
		let ScopeOwner::Raw(component) = policy.owner() else {
			return Err(ScopeError::WrongOwner.into());
		};
		let bound = declaration
			.bind(component)
			.map_err(NativeAdmissionError::Contract)?;
		if !std::ptr::eq(bound.transitions(), scope.component()) {
			return Err(ScopeError::DifferentRawDeclaration.into());
		}
		Ok(bound)
	}
}
