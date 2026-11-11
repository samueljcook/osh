//! Native family access requires the actual authenticated scoped role and local callable code.
//!
//! Native Semaphore and Throttler scopes run on every storage route: v11 starts them empty and
//! never shares their records with v10. These bindings perform no protected record IO or
//! acquisition delivery.

use crate::fleet::primitive_scope::{
	BoundSemaphoreScopeReader, BoundSemaphoreScopeWriter, BoundThrottlerScopeReader,
	BoundThrottlerScopeWriter, SemaphoreScopeDeclaration, ThrottlerScopeDeclaration,
};
use crate::scope::{PrimitiveRole, ScopeIdentity, ScopePolicy};

use super::{NativeAdmissionError, NativeAdmittedContracts};

impl NativeAdmittedContracts {
	pub(super) fn primitive_policy(
		&self,
		scope: &ScopeIdentity,
		role: PrimitiveRole,
	) -> Result<&ScopePolicy, NativeAdmissionError> {
		let policy = self
			.observation
			.control
			.state()
			.scope_policies()
			.lookup(scope)?;
		self.observation
			.participant()?
			.capabilities()
			.primitive_roles()
			.lookup(scope, role)?
			.check_policy(policy)?;
		Ok(policy)
	}
	pub(crate) fn bind_semaphore_reader<'a, 'scope, 'code>(
		&'a self,
		declaration: &'a SemaphoreScopeDeclaration<'scope, 'code>,
	) -> Result<BoundSemaphoreScopeReader<'a, 'code>, NativeAdmissionError> {
		let policy = self.primitive_policy(declaration.identity(), PrimitiveRole::Reader)?;
		Ok(declaration.bind_reader(declaration.identity(), policy)?)
	}
	pub(crate) fn bind_semaphore_writer<'a, 'scope, 'code>(
		&'a self,
		declaration: &'a SemaphoreScopeDeclaration<'scope, 'code>,
	) -> Result<BoundSemaphoreScopeWriter<'a, 'code>, NativeAdmissionError> {
		let policy = self.primitive_policy(declaration.identity(), PrimitiveRole::Writer)?;
		Ok(declaration.bind_writer(declaration.identity(), policy)?)
	}
	pub(crate) fn bind_throttler_reader<'a, 'scope, 'code>(
		&'a self,
		declaration: &'a ThrottlerScopeDeclaration<'scope, 'code>,
	) -> Result<BoundThrottlerScopeReader<'a, 'code>, NativeAdmissionError> {
		let policy = self.primitive_policy(declaration.identity(), PrimitiveRole::Reader)?;
		Ok(declaration.bind_reader(declaration.identity(), policy)?)
	}
	pub(crate) fn bind_throttler_writer<'a, 'scope, 'code>(
		&'a self,
		declaration: &'a ThrottlerScopeDeclaration<'scope, 'code>,
	) -> Result<BoundThrottlerScopeWriter<'a, 'code>, NativeAdmissionError> {
		let policy = self.primitive_policy(declaration.identity(), PrimitiveRole::Writer)?;
		Ok(declaration.bind_writer(declaration.identity(), policy)?)
	}
}
