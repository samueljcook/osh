//! The public classification of declaration failures.

use super::{AppDeclarationError, AppDeclarationInnerError};

/// The class of a declaration failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AppDeclarationErrorKind {
	/// The payload codecs of a cache or Topic scope cannot provide a payload role that the
	/// declaration marks as supported, for example the producer role when no representation
	/// has an encoder.
	InvalidPayloadDeclaration,
	/// Declarations of two different kinds, for example a document and a Raw declaration, name
	/// one scope.
	ConflictingScopeOwner,
	/// Two declarations of the same kind name one scope.
	DuplicateScopeDeclaration,
	/// A document declaration queries one index twice.
	DuplicateQuery {
		/// The repeated query's position in the supplied intent.
		index: usize,
	},
	/// Two document declarations of one scope have different schemas.
	ConflictingSchemaFamily {
		/// The conflicting document declaration's position.
		index: usize,
	},
	/// The scope's code cannot provide a role that the declaration marks as supported, or two
	/// declarations name one scope.
	InvalidScopeDeclaration,
	/// The document code cannot provide a role or a query that the declaration marks as
	/// supported.
	InvalidDocumentDeclaration,
	/// Two document declarations name one scope.
	DuplicateDocumentDeclaration,
}

impl AppDeclarationError {
	/// The class of the failure.
	pub fn kind(&self) -> AppDeclarationErrorKind {
		use AppDeclarationErrorKind as Kind;
		use AppDeclarationInnerError as Cause;
		match self.source.as_ref() {
			Cause::Payload(_) => Kind::InvalidPayloadDeclaration,
			Cause::ConflictingScopeOwner => Kind::ConflictingScopeOwner,
			Cause::ScopeRegistry(_) => Kind::DuplicateScopeDeclaration,
			Cause::DuplicateQuery { index } => Kind::DuplicateQuery { index: *index },
			Cause::ConflictingSchemaFamily { index } => {
				Kind::ConflictingSchemaFamily { index: *index }
			}
			Cause::Scope(_) => Kind::InvalidScopeDeclaration,
			Cause::DocumentDeclaration(_) => Kind::InvalidDocumentDeclaration,
			Cause::DocumentRegistry(_) => Kind::DuplicateDocumentDeclaration,
		}
	}
}
