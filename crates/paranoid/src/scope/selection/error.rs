//! Failures to derive a scope's installed meaning from its declared code.

use std::fmt;

use crate::fleet::primitive_scope::PrimitiveScopeBindingError;
use crate::installation::contract::StateContractError;
use crate::kv::document::schema::DocumentBindingError;
use crate::scope::{ScopeError, ScopeIdentity};
use crate::upgrade::policy::scope::ScopedUpgradeCompatibilityError;

/// The class of a failure to derive a scope's installed writer, readers, and indexes from the
/// declared code.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScopeSelectionErrorKind {
	/// The stored-format policy that Paranoid derived from the code's versions is invalid. This
	/// indicates a defect in Paranoid.
	InvalidSchemaSelection,
	/// An index in the derived target is not one of the document code's index declarations.
	/// This indicates a defect in Paranoid.
	InvalidIndexSelection,
	/// The derived target conflicts with the scope's kind or with its indexes.
	InvalidScopeSelection,
	/// The scope's code cannot read or write what the derived target requires, for example
	/// because the document code has no index projector for one of its declared indexes in
	/// the writer version.
	UnsupportedScopeSelection,
	/// The scope's code writes no representation as JSON, so Paranoid cannot choose a writer.
	NoJsonWriter,
	/// The newest version that the scope's code writes is older than the installation's
	/// writer version, so this code belongs to an earlier release; an upgrade never moves a
	/// scope to an older writer.
	WriterOlderThanInstalled,
}

#[derive(Debug, thiserror::Error)]
pub(super) enum ScopeSelectionInnerError {
	#[error("requested representation version {version} is not registered")]
	UnsupportedRepresentation { version: u32 },
	#[error("schema selection was rejected")]
	Contract(#[source] StateContractError),
	#[error("logical index declaration was rejected")]
	Index(#[source] DocumentBindingError),
	#[error("scope selection was rejected")]
	Scope(#[source] ScopeError),
	#[error("intrinsic Primitive code cannot support its fixed initial policy")]
	Primitive(#[source] PrimitiveScopeBindingError),
	#[error("code cannot support the requested scope target")]
	Candidate(#[source] ScopedUpgradeCompatibilityError),
	#[error("the code writes no representation as native JSON")]
	NoJsonWriter,
	#[error(
		"the code's newest writer version is older than the installed writer version {installed}"
	)]
	WriterOlderThanInstalled { installed: u32 },
	#[error("the payload code cannot support the derived scope target")]
	Payload(#[source] crate::fleet::payload::scope::retained::RetainedPayloadError),
}

pub(super) struct SelectionFailureFields<'a> {
	pub(super) scope: Option<&'a ScopeIdentity>,
	pub(super) schema_identifier: Option<&'a str>,
	pub(super) version: Option<u32>,
	pub(super) index_position: Option<usize>,
	pub(super) source: ScopeSelectionInnerError,
}

struct SelectionDiagnostic {
	scope: Option<ScopeIdentity>,
	schema_identifier: Option<String>,
	version: Option<u32>,
	index_position: Option<usize>,
	source: ScopeSelectionInnerError,
}

/// A failure to derive a scope's installed meaning from its declared code. Its formatting shows
/// no stored values.
pub struct ScopeSelectionError {
	diagnostic: Box<SelectionDiagnostic>,
}

impl ScopeSelectionError {
	pub(super) fn new(fields: SelectionFailureFields<'_>) -> Self {
		Self {
			diagnostic: Box::new(SelectionDiagnostic {
				scope: fields.scope.cloned(),
				schema_identifier: fields.schema_identifier.map(str::to_owned),
				version: fields.version,
				index_position: fields.index_position,
				source: fields.source,
			}),
		}
	}
	pub(super) fn from_scope(source: ScopeError) -> Self {
		Self::new(SelectionFailureFields {
			scope: None,
			schema_identifier: None,
			version: None,
			index_position: None,
			source: ScopeSelectionInnerError::Scope(source),
		})
	}
	pub(super) fn from_candidate(source: ScopedUpgradeCompatibilityError) -> Self {
		Self::new(SelectionFailureFields {
			scope: None,
			schema_identifier: None,
			version: None,
			index_position: None,
			source: ScopeSelectionInnerError::Candidate(source),
		})
	}
	/// The scope, when the failure concerns one scope.
	pub fn scope(&self) -> Option<&ScopeIdentity> {
		self.diagnostic.scope.as_ref()
	}
	/// The schema identifier, when the failure concerns a document or payload schema.
	pub fn schema_identifier(&self) -> Option<&str> {
		self.diagnostic.schema_identifier.as_deref()
	}
	/// The representation version, when the failure concerns one version.
	pub fn version(&self) -> Option<u32> {
		self.diagnostic
			.version
			.or_else(|| match &self.diagnostic.source {
				ScopeSelectionInnerError::UnsupportedRepresentation { version } => Some(*version),
				ScopeSelectionInnerError::Contract(source) => contract_version(source),
				ScopeSelectionInnerError::Scope(source) => scope_version(source),
				ScopeSelectionInnerError::Primitive(source)
				| ScopeSelectionInnerError::Candidate(
					ScopedUpgradeCompatibilityError::Primitive(source),
				) => primitive_version(source),
				ScopeSelectionInnerError::Candidate(ScopedUpgradeCompatibilityError::Scope(
					source,
				)) => scope_version(source),
				_ => None,
			})
	}
	/// The position of the failed index in the scope's index list, when the failure concerns
	/// one index.
	pub fn index_position(&self) -> Option<usize> {
		self.diagnostic.index_position.or_else(|| {
			let scope = match &self.diagnostic.source {
				ScopeSelectionInnerError::Scope(scope)
				| ScopeSelectionInnerError::Candidate(ScopedUpgradeCompatibilityError::Scope(
					scope,
				)) => scope,
				_ => return None,
			};
			match scope {
				ScopeError::DuplicateIndex { index }
				| ScopeError::MissingIndex { index }
				| ScopeError::IndexShapeMismatch { index } => Some(*index),
				_ => None,
			}
		})
	}
	/// The class of the failure.
	pub fn kind(&self) -> ScopeSelectionErrorKind {
		use ScopeSelectionErrorKind as Kind;
		match &self.diagnostic.source {
			// Paranoid derives every version from the registered representations, so an
			// unregistered version is a defect in the derivation.
			ScopeSelectionInnerError::UnsupportedRepresentation { .. } => {
				Kind::InvalidSchemaSelection
			}
			ScopeSelectionInnerError::Contract(_) => Kind::InvalidSchemaSelection,
			ScopeSelectionInnerError::Index(_) => Kind::InvalidIndexSelection,
			ScopeSelectionInnerError::Scope(_) => Kind::InvalidScopeSelection,
			ScopeSelectionInnerError::Primitive(_)
			| ScopeSelectionInnerError::Candidate(_)
			| ScopeSelectionInnerError::Payload(_) => Kind::UnsupportedScopeSelection,
			ScopeSelectionInnerError::NoJsonWriter => Kind::NoJsonWriter,
			ScopeSelectionInnerError::WriterOlderThanInstalled { .. } => {
				Kind::WriterOlderThanInstalled
			}
		}
	}
}

fn contract_version(source: &StateContractError) -> Option<u32> {
	match source {
		StateContractError::RepresentationVersionIsZero => Some(0),
		StateContractError::DuplicateRequiredReader { version }
		| StateContractError::WriterNotRetained { version }
		| StateContractError::UnsupportedReader { version, .. }
		| StateContractError::UnsupportedWriter { version, .. }
		| StateContractError::IncompatibleEnvelope { version, .. }
		| StateContractError::ReaderNotPermitted { version, .. } => Some(*version),
		_ => None,
	}
}
fn primitive_version(source: &PrimitiveScopeBindingError) -> Option<u32> {
	use crate::fleet::payload::scope::PayloadScopeError;
	use crate::fleet::semaphore::state::SemaphoreStateError;
	use crate::fleet::throttler::state::ThrottlerStateError;
	match source {
		PrimitiveScopeBindingError::Scope(source) => scope_version(source),
		PrimitiveScopeBindingError::Semaphore(SemaphoreStateError::Contract(source))
		| PrimitiveScopeBindingError::Throttler(ThrottlerStateError::Contract(source)) => {
			contract_version(source)
		}
		PrimitiveScopeBindingError::Payload(source) => match source.as_ref() {
			PayloadScopeError::Scope(source) => scope_version(source),
			_ => None,
		},
		_ => None,
	}
}
fn scope_version(source: &ScopeError) -> Option<u32> {
	match source {
		ScopeError::Contract(source) => contract_version(source),
		ScopeError::RepresentationVersionIsZero => Some(0),
		ScopeError::DuplicateRepresentation { version }
		| ScopeError::UnsupportedRepresentation { version } => Some(*version),
		_ => None,
	}
}

impl fmt::Debug for ScopeSelectionError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("ScopeSelectionError")
			.field("kind", &self.kind())
			.field("version", &self.version())
			.field("index_position", &self.index_position())
			.finish_non_exhaustive()
	}
}
impl fmt::Display for ScopeSelectionError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("code-owned application scope selection was rejected")
	}
}
impl std::error::Error for ScopeSelectionError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		Some(&self.diagnostic.source)
	}
}
