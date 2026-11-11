//! Configured physical association with actual retained heterogeneous document code.
//!
//! A binding grants no installed role, transaction, catalog, coverage or readiness authority.
//! Only a fixed executor can validate those facts under its actual installation control.

use std::fmt;

use crate::installation::creation::{NativeInstallationError, PgNativeStorageBindings};
use crate::kv::document::schema::IndexContractIdentity;
use crate::kv::document::schema::managed::{
	ManagedDocumentIndexLookupError, ManagedDocumentRegistration,
};

use super::{
	IndexContract, IndexContractDescriptor, PgDocumentIndexDescriptor, PgDocumentIndexError,
	PgDocumentIndexExpressions, PgDocumentIndexFormat, PgIdentifier, PgPointStorage,
};

pub(crate) struct PgManagedDocumentIndexBindingFields<'registration, 'lookup, 'placement> {
	pub(crate) registration: &'registration ManagedDocumentRegistration,
	pub(crate) index: &'lookup IndexContractIdentity,
	pub(crate) native: &'placement PgNativeStorageBindings,
	pub(crate) index_name: PgIdentifier,
}

/// The contract borrow belongs to retained code, not the temporary lookup identity.
pub(crate) struct PgManagedDocumentIndexBinding<'a> {
	storage: PgPointStorage,
	descriptor: PgDocumentIndexDescriptor,
	expressions: PgDocumentIndexExpressions<'a>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum PgManagedDocumentIndexError {
	#[error("managed physical index has no contract")]
	Lookup(#[source] Box<ManagedDocumentIndexLookupError>),
	#[error("managed physical index configured storage is invalid")]
	Placement(#[source] Box<NativeInstallationError>),
	#[error("managed physical index expression derivation failed")]
	Expression(#[from] PgDocumentIndexError),
}

impl<'a> PgManagedDocumentIndexBinding<'a> {
	pub(crate) fn new(
		fields: PgManagedDocumentIndexBindingFields<'a, '_, '_>,
	) -> Result<Self, PgManagedDocumentIndexError> {
		let registration = fields.registration;
		let index = registration
			.index_contract(fields.index)
			.map_err(|error| PgManagedDocumentIndexError::Lookup(Box::new(error)))?;
		let storage = fields
			.native
			.app_storage(registration.scope())
			.map_err(|error| PgManagedDocumentIndexError::Placement(Box::new(error)))?
			.clone();
		let descriptor = PgDocumentIndexDescriptor {
			format: PgDocumentIndexFormat::CompoundExpressionsV1,
			scope: registration.scope().clone(),
			schema_identifier: registration.schema_identifier().to_owned(),
			index: IndexContractDescriptor::from_declaration(index),
			table: storage.table().clone(),
			index_name: fields.index_name,
		};
		let expressions = PgDocumentIndexExpressions::new(registration.scope(), index)?;
		Ok(Self {
			storage,
			descriptor,
			expressions,
		})
	}

	pub(crate) fn index(&self) -> &IndexContract {
		self.expressions.index()
	}
	pub(crate) fn storage(&self) -> &PgPointStorage {
		&self.storage
	}
	pub(crate) fn descriptor(&self) -> &PgDocumentIndexDescriptor {
		&self.descriptor
	}
	pub(crate) fn expressions(&self) -> &PgDocumentIndexExpressions<'a> {
		&self.expressions
	}
}

impl fmt::Debug for PgManagedDocumentIndexBinding<'_> {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("PgManagedDocumentIndexBinding")
			.field("descriptor", &self.descriptor)
			.finish_non_exhaustive()
	}
}
