//! Strict physical document-index comparison metadata, without executable declarations.

use std::{borrow::Cow, collections::BTreeSet};

use serde::{Deserialize, Serialize};

use crate::db::{InvalidPgIdentifierError, PgIdentifier, PgQualifiedTableName, PgSchemaName};
use crate::kv::document::postgres::index::{
	PgDocumentIndexDescriptor, PgDocumentIndexDescriptorError, PgDocumentIndexDescriptorFields,
	PgDocumentIndexFormat,
};
use crate::scope::wire::{IndexWire, ScopeIdentityWire, ScopeWireError};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DocumentIndexWire<'a> {
	format: Cow<'a, str>,
	scope: ScopeIdentityWire<'a>,
	schema_identifier: Cow<'a, str>,
	index: IndexWire<'a>,
	table: TableWire<'a>,
	index_name: Cow<'a, str>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TableWire<'a> {
	schema: Cow<'a, str>,
	table: Cow<'a, str>,
}

impl<'a> DocumentIndexWire<'a> {
	pub(super) fn from_descriptor(descriptor: &'a PgDocumentIndexDescriptor) -> Self {
		Self {
			format: Cow::Borrowed(descriptor.format().identifier()),
			scope: ScopeIdentityWire::from_identity(descriptor.scope()),
			schema_identifier: Cow::Borrowed(descriptor.schema_identifier()),
			index: IndexWire::from_descriptor(descriptor.index()),
			table: TableWire {
				schema: Cow::Borrowed(descriptor.table().schema().as_str()),
				table: Cow::Borrowed(descriptor.table().table().as_str()),
			},
			index_name: Cow::Borrowed(descriptor.index_name().as_str()),
		}
	}

	pub(super) fn into_descriptor(
		self,
	) -> Result<PgDocumentIndexDescriptor, DocumentIndexCodecError> {
		let format = PgDocumentIndexFormat::from_identifier(&self.format)
			.ok_or(DocumentIndexCodecError::UnsupportedFormat)?;
		let table = PgQualifiedTableName::new(
			PgSchemaName::from_identifier_text(self.table.schema.as_ref())?,
			PgIdentifier::new(self.table.table.as_ref())?,
		);
		PgDocumentIndexDescriptor::recorded(PgDocumentIndexDescriptorFields {
			format,
			scope: self.scope.into_identity()?,
			schema_identifier: self.schema_identifier.into_owned(),
			index: self.index.into_descriptor()?,
			table,
			index_name: PgIdentifier::new(self.index_name.as_ref())?,
		})
		.map_err(Into::into)
	}
}

/// Only ordinal positions appear in diagnostics; physical names and field values stay private.
#[derive(Debug, thiserror::Error)]
pub(crate) enum DocumentIndexCodecError {
	#[error("recorded document index physical format is unsupported")]
	UnsupportedFormat,
	#[error("recorded document index identifier is invalid")]
	Identifier(#[from] InvalidPgIdentifierError),
	#[error("recorded document index logical metadata is invalid")]
	Logical(#[from] ScopeWireError),
	#[error("recorded document index descriptor is invalid")]
	Descriptor(#[from] PgDocumentIndexDescriptorError),
	#[error("recorded document index {entry_index} repeats a logical scope/index association")]
	DuplicateLogical { entry_index: usize },
	#[error("recorded document index {entry_index} repeats a schema/index-name association")]
	DuplicatePhysicalName { entry_index: usize },
	#[error(
		"recorded document index {entry_index} conflicts with the requested logical association"
	)]
	ConflictingLogical { entry_index: usize },
	#[error("recorded document index {entry_index} owns the requested physical name")]
	ConflictingPhysicalName { entry_index: usize },
}

pub(super) fn check_unique_indexes(
	indexes: &[PgDocumentIndexDescriptor],
) -> Result<(), DocumentIndexCodecError> {
	let mut logical = BTreeSet::new();
	let mut physical = BTreeSet::new();
	for (entry_index, index) in indexes.iter().enumerate() {
		if !logical.insert((
			index.scope(),
			index.index().identity().identifier(),
			index.index().identity().version(),
		)) {
			return Err(DocumentIndexCodecError::DuplicateLogical { entry_index });
		}
		if !physical.insert((index.table().schema(), index.index_name())) {
			return Err(DocumentIndexCodecError::DuplicatePhysicalName { entry_index });
		}
	}
	Ok(())
}
