//! Errors returned when [`super::PgRuntime`] cannot construct an application scope handle.

use std::error::Error;
use std::fmt;

use crate::runtime::admission::NativeAdmissionError;
use crate::scope::ScopeIdentity;

pub(crate) struct PgScopeHandleErrorFields<'a> {
	pub(crate) scope: &'a ScopeIdentity,
	pub(crate) source: ScopeHandleCause,
}

pub(crate) enum ScopeHandleCause {
	NotDeclared,
	DeclarationMismatch,
	Admission(NativeAdmissionError),
}

/// The class of a failure to build a scope handle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PgScopeHandleErrorKind {
	/// The app declarations that the application passed in
	/// [`PgOpenInstallationFields::app_declarations`](crate::installation::PgOpenInstallationFields::app_declarations)
	/// when it opened the runtime's installation declare no scope with this identity.
	NotDeclared,
	/// Those app declarations declare the scope for another family, or with other value, codec
	/// failure, or projector failure types.
	DeclarationMismatch,
	/// A defect in Paranoid.
	Internal,
}

/// A scope handle that the runtime could not build. Building a handle does no database work.
pub struct PgScopeHandleError {
	scope: ScopeIdentity,
	source: ScopeHandleCause,
}

impl PgScopeHandleError {
	pub(crate) fn new(fields: PgScopeHandleErrorFields<'_>) -> Self {
		Self {
			scope: fields.scope.clone(),
			source: fields.source,
		}
	}

	/// The scope for which the caller requested the handle.
	pub fn scope(&self) -> &ScopeIdentity {
		&self.scope
	}

	/// The class of the failure.
	pub fn kind(&self) -> PgScopeHandleErrorKind {
		match self.source {
			ScopeHandleCause::NotDeclared => PgScopeHandleErrorKind::NotDeclared,
			ScopeHandleCause::DeclarationMismatch => PgScopeHandleErrorKind::DeclarationMismatch,
			ScopeHandleCause::Admission(_) => PgScopeHandleErrorKind::Internal,
		}
	}
}

impl fmt::Debug for PgScopeHandleError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("PgScopeHandleError")
			.field("scope", &self.scope)
			.field("kind", &self.kind())
			.finish()
	}
}

impl fmt::Display for PgScopeHandleError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self.source {
			ScopeHandleCause::NotDeclared => {
				formatter.write_str("the app declarations declare no scope with this identity")
			}
			ScopeHandleCause::DeclarationMismatch => formatter.write_str(
				"the app declarations declare the scope for another family or with other types",
			),
			ScopeHandleCause::Admission(_) => {
				formatter.write_str("the runtime could not build the scope handle")
			}
		}
	}
}

impl Error for PgScopeHandleError {
	fn source(&self) -> Option<&(dyn Error + 'static)> {
		match &self.source {
			ScopeHandleCause::Admission(source) => Some(source),
			ScopeHandleCause::NotDeclared | ScopeHandleCause::DeclarationMismatch => None,
		}
	}
}
