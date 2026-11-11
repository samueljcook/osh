//! Strict actual payload role support, with no intrinsic resource or active-use authority.

use super::{RepresentationWire, ScopeIdentityWire, ScopeWireError, WIRE_VERSION, require_version};
use crate::scope::payload::{PayloadRole, PayloadRoleSupport};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PayloadRoleSupportWire<'a> {
	version: u32,
	scope: ScopeIdentityWire<'a>,
	schema: Cow<'a, str>,
	role: PayloadRole,
	representations: Vec<RepresentationWire>,
}
impl<'a> PayloadRoleSupportWire<'a> {
	pub(crate) fn from_support(support: &'a PayloadRoleSupport) -> Self {
		Self {
			version: WIRE_VERSION,
			scope: ScopeIdentityWire::from_identity(support.identity()),
			schema: Cow::Borrowed(&support.schema_identifier),
			role: support.role(),
			representations: support
				.representations
				.iter()
				.copied()
				.map(Into::into)
				.collect(),
		}
	}
	pub(crate) fn into_support(self) -> Result<PayloadRoleSupport, ScopeWireError> {
		require_version(self.version)?;
		Ok(PayloadRoleSupport::parse_parts(
			self.scope.into_identity()?,
			self.schema.into_owned(),
			self.role,
			self.representations.into_iter().map(Into::into).collect(),
		)?)
	}
}
