//! The document code of a scope: its codecs, its indexes, and its index projectors.

use crate::records::state_schema::StateSchema;

use super::{
	DocumentDeclarationError, DocumentSchema, IndexContract, IndexContractIdentity, IndexProjector,
	RegisteredIndexProjector, RepresentationIndexes,
};

/// One index's projector for one JSON stored format version.
pub struct IndexProjectorEntry<T, P> {
	/// The index that the function projects, which the document code declares.
	pub index: IndexContractIdentity,
	/// The projection function.
	pub project_fn: IndexProjector<T, P>,
}

/// The index projectors of one JSON stored format version.
pub struct RepresentationProjectors<T, P> {
	/// The stored format version.
	pub representation_version: u32,
	/// The version's projectors, at most one for each index.
	pub projectors: Vec<IndexProjectorEntry<T, P>>,
}

/// The inputs of a scope's document code.
pub struct DocumentCodeFields<T, C, P> {
	/// The document's stored formats.
	pub schema: StateSchema<T, C>,
	/// The document's indexes.
	pub indexes: Vec<IndexContract>,
	/// The index projectors of each JSON stored format version.
	pub representations: Vec<RepresentationProjectors<T, P>>,
}

/// A scope's checked document code.
pub struct DocumentCode<T, C, P> {
	schema: StateSchema<T, C>,
	indexes: Vec<IndexContract>,
	representations: Vec<RepresentationProjectors<T, P>>,
}

impl<T, C, P> DocumentCode<T, C, P> {
	/// Checks the indexes and the projectors against the schema without database work.
	pub fn new(fields: DocumentCodeFields<T, C, P>) -> Result<Self, DocumentDeclarationError> {
		let code = Self {
			schema: fields.schema,
			indexes: fields.indexes,
			representations: fields.representations,
		};
		code.checked_view()?;
		Ok(code)
	}

	/// The document's stored formats.
	pub fn schema(&self) -> &StateSchema<T, C> {
		&self.schema
	}

	/// Borrows the retained contract itself; metadata cannot create another declaration.
	pub(super) fn index_contract(
		&self,
		identity: &IndexContractIdentity,
	) -> Option<&IndexContract> {
		self.indexes
			.iter()
			.find(|index| index.identity() == identity)
	}

	/// Borrows a checked document declaration from these immutable code owners.
	pub(crate) fn document_schema(&self) -> DocumentSchema<'_, T, C, P> {
		self.checked_view()
			.expect("immutable owned document registrations passed the shared declaration parser")
	}

	fn checked_view(&self) -> Result<DocumentSchema<'_, T, C, P>, DocumentDeclarationError> {
		let mut representations = Vec::with_capacity(self.representations.len());
		for representation in &self.representations {
			let version = representation.representation_version;
			let mut projectors = Vec::with_capacity(representation.projectors.len());
			for (projector_index, projector) in representation.projectors.iter().enumerate() {
				let contract = self.index_contract(&projector.index).ok_or(
					DocumentDeclarationError::UndeclaredIndex {
						version,
						projector_index,
					},
				)?;
				projectors.push(RegisteredIndexProjector {
					contract,
					project_fn: projector.project_fn,
				});
			}
			representations.push(RepresentationIndexes {
				representation_version: version,
				projectors,
			});
		}
		DocumentSchema::new(&self.schema, self.indexes.iter().collect(), representations)
	}
}
