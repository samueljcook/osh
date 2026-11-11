//! Exact ordinary borrowed declarations and optional matching retained code ownership.

use crate::fleet::payload::scope::cache::CacheEntryScopeDeclaration;
use crate::fleet::payload::scope::retained::{
	RetainedCacheEntryScopeCode, RetainedPayloadCode, RetainedTopicEventScopeCode,
};
use crate::fleet::payload::scope::topic::TopicEventScopeDeclaration;
use crate::fleet::primitive_scope::ManagedPrimitiveRegistration;
use crate::installation::contract::ComponentDeclaration;
use crate::runtime::admission::NativeAdmissionError;
use crate::scope::managed::{
	ManagedScopeRegistration, ManagedScopeRegistry, ManagedScopeRegistryError,
	ManagedScopeRegistryInnerError,
};
use crate::scope::payload::PayloadSchemaDeclaration;
use crate::scope::{ScopeError, ScopeIdentity};
use std::{fmt, sync::Arc};

pub(crate) enum NativePayloadConstructionCause {
	Registry(Box<ManagedScopeRegistryError>),
	WrongFamily,
	DifferentDeclaration,
	Admission(Box<NativeAdmissionError>),
}
pub(crate) struct NativePayloadConstructionError {
	source: NativePayloadConstructionCause,
}
impl NativePayloadConstructionError {
	fn new(source: NativePayloadConstructionCause) -> Self {
		Self { source }
	}
	pub(super) fn admission(source: NativeAdmissionError) -> Self {
		Self::new(NativePayloadConstructionCause::Admission(Box::new(source)))
	}
}
impl fmt::Debug for NativePayloadConstructionError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("NativePayloadConstructionError { .. }")
	}
}
impl fmt::Display for NativePayloadConstructionError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("ordinary native payload point code or placement is unavailable")
	}
}
impl std::error::Error for NativePayloadConstructionError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match &self.source {
			NativePayloadConstructionCause::Registry(source) => Some(source.as_ref()),
			NativePayloadConstructionCause::Admission(source) => Some(source.as_ref()),
			_ => None,
		}
	}
}
struct IntrinsicCodeFields<'a, 'actual, 'retained, T> {
	actual: &'a ComponentDeclaration<'actual, T>,
	retained: &'a ComponentDeclaration<'retained, T>,
}
fn matches_intrinsic<T>(fields: IntrinsicCodeFields<'_, '_, '_, T>) -> bool {
	fields.actual.identifier() == fields.retained.identifier()
		&& fields.actual.schemas().len() == fields.retained.schemas().len()
		&& fields.actual.schemas().iter().all(|actual| {
			fields.retained.schemas().iter().any(|retained| {
				actual.identifier() == retained.identifier()
					&& actual.is_same_declaration(*retained)
			})
		})
}
/// The declaration of a payload point family's scope, which names the code that the runtime
/// retains for that family.
pub(super) trait PayloadPointDeclaration {
	/// The code that the runtime retains for the family.
	type RetainedCode;
	/// The declared scope identity.
	fn scope_identity(&self) -> &ScopeIdentity;
	/// The retained code of this family in `registration`, or `None` for another family.
	fn retained_code(
		registration: &ManagedPrimitiveRegistration,
	) -> Option<&Arc<Self::RetainedCode>>;
	/// Whether `code` has this declaration's scope identity and intrinsic schemas.
	fn declares(&self, code: &Self::RetainedCode) -> bool;
}
impl PayloadPointDeclaration for CacheEntryScopeDeclaration<'_, '_> {
	type RetainedCode = RetainedCacheEntryScopeCode;
	fn scope_identity(&self) -> &ScopeIdentity {
		self.identity()
	}
	fn retained_code(
		registration: &ManagedPrimitiveRegistration,
	) -> Option<&Arc<RetainedCacheEntryScopeCode>> {
		match registration {
			ManagedPrimitiveRegistration::CacheEntry(code) => Some(code),
			_ => None,
		}
	}
	fn declares(&self, code: &RetainedCacheEntryScopeCode) -> bool {
		std::ptr::eq(self.identity(), code.identity())
			&& matches_intrinsic(IntrinsicCodeFields {
				actual: self.declaration(),
				retained: code.declaration().declaration(),
			})
	}
}
impl PayloadPointDeclaration for TopicEventScopeDeclaration<'_, '_> {
	type RetainedCode = RetainedTopicEventScopeCode;
	fn scope_identity(&self) -> &ScopeIdentity {
		self.identity()
	}
	fn retained_code(
		registration: &ManagedPrimitiveRegistration,
	) -> Option<&Arc<RetainedTopicEventScopeCode>> {
		match registration {
			ManagedPrimitiveRegistration::TopicEvent(code) => Some(code),
			_ => None,
		}
	}
	fn declares(&self, code: &RetainedTopicEventScopeCode) -> bool {
		std::ptr::eq(self.identity(), code.identity())
			&& matches_intrinsic(IntrinsicCodeFields {
				actual: self.declaration(),
				retained: code.declaration().declaration(),
			})
	}
}
/// The code that the runtime retains for the scope of `declaration`, or `None` when the runtime
/// retains no code for the scope.
pub(super) fn retained_point_code<D: PayloadPointDeclaration>(
	registry: &ManagedScopeRegistry,
	declaration: &D,
) -> Result<Option<Arc<D::RetainedCode>>, NativePayloadConstructionError> {
	let fail = |error| NativePayloadConstructionError::new(error);
	let owner = match registry.lookup(declaration.scope_identity()) {
		Ok(owner) => owner,
		Err(error)
			if matches!(
				error.inner_error(),
				ManagedScopeRegistryInnerError::MissingOwner
			) =>
		{
			return Ok(None);
		}
		Err(error) => {
			return Err(fail(NativePayloadConstructionCause::Registry(Box::new(
				error,
			))));
		}
	};
	let ManagedScopeRegistration::Primitive(registration) = owner else {
		return Err(fail(NativePayloadConstructionCause::WrongFamily));
	};
	let Some(code) = D::retained_code(registration) else {
		return Err(fail(NativePayloadConstructionCause::WrongFamily));
	};
	if !declaration.declares(code) {
		return Err(fail(NativePayloadConstructionCause::DifferentDeclaration));
	}
	Ok(Some(Arc::clone(code)))
}
pub(super) fn require_retained_payload<T, C>(
	code: Option<&RetainedPayloadCode>,
	declaration: &PayloadSchemaDeclaration<'_, '_, T, C>,
) -> Result<(), ScopeError> {
	match code {
		None => Ok(()),
		Some(RetainedPayloadCode::Schema(code))
			if code.matches_capabilities(declaration.schema().capabilities()) =>
		{
			Ok(())
		}
		Some(_) => Err(ScopeError::DifferentPayloadDeclaration),
	}
}
