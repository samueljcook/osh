//! The physical indexes of the document indexes that a new installation makes ready for
//! queries.
//!
//! The opening that creates an installation, or imports v10, creates every container with
//! `CREATE TABLE` in its creation transaction, so no document of any scope exists when that
//! transaction creates the scopes' indexes. Every declared index is maintained from the
//! installation's start, so every document that a later transaction writes carries the
//! projections of every declared index, and no participant exists before the creation
//! transaction commits; pre.10 deployments of an imported installation write no document
//! scope. The creation transaction therefore establishes everything that an upgrade's
//! registration, verification, and readiness steps establish for an index: the physical
//! index, the ledger entry that records it, and the projections of every document of the
//! scope.

use crate::db::{DatabaseOperationKind, WriteTx, owned_portable_query};
use crate::kv::document::postgres::index::PgDocumentIndexDescriptor;
use crate::kv::document::postgres::index::derived_index_name;
use crate::kv::document::postgres::index::managed::{
	PgManagedDocumentIndexBinding, PgManagedDocumentIndexBindingFields,
};
use crate::kv::document::postgres::index::physical::{
	PgDocumentIndexCatalogObservation, PgDocumentIndexExpectedCatalog,
	PgDocumentIndexPhysicalError, create_index_statement, inspect_document_index,
};
use crate::kv::document::schema::managed::ManagedDocumentRegistry;
use crate::scope::{ScopeOwner, ScopePolicySet};

use super::{NativeInstallationError, PgBackendRegistration, PgNativeStorageBindings};

const CREATE_DOCUMENT_INDEX_LABEL: &str = "db.native_installation.create_document_index";

/// The physical index of every query-ready index of a new installation's scope policies.
pub(crate) struct InitialDocumentIndexes<'a> {
	bindings: Vec<PgManagedDocumentIndexBinding<'a>>,
}

impl<'a> InitialDocumentIndexes<'a> {
	/// Binds the physical index of every query-ready index in `policies` to the code in
	/// `documents`, under the name that an upgrade would give it. This does no database work.
	pub(crate) fn new(
		policies: &ScopePolicySet,
		documents: &'a ManagedDocumentRegistry,
		native: &PgNativeStorageBindings,
	) -> Result<Self, NativeInstallationError> {
		let mut bindings = Vec::new();
		for policy in policies.policies() {
			let ScopeOwner::Document(document) = policy.owner() else {
				continue;
			};
			if document.ready_indexes().len() == 0 {
				continue;
			}
			let scope = policy.identity();
			let registration = documents.lookup(scope).map_err(|error| {
				NativeInstallationError::InitialDocumentIndexCode {
					scope: scope.clone(),
					source: Box::new(error),
				}
			})?;
			for index in document.ready_indexes() {
				let binding =
					PgManagedDocumentIndexBinding::new(PgManagedDocumentIndexBindingFields {
						registration,
						index,
						native,
						index_name: derived_index_name(scope, index),
					})
					.map_err(|error| {
						NativeInstallationError::InitialDocumentIndexBinding {
							scope: scope.clone(),
							source: Box::new(error),
						}
					})?;
				bindings.push(binding);
			}
		}
		Ok(Self { bindings })
	}

	/// The ledger entries of the physical indexes.
	pub(crate) fn descriptors(&self) -> impl Iterator<Item = &PgDocumentIndexDescriptor> {
		self.bindings
			.iter()
			.map(PgManagedDocumentIndexBinding::descriptor)
	}

	/// Creates each physical index in the creation transaction, after the transaction has
	/// created the index's container, and checks the created index in the catalog.
	pub(crate) async fn create(&self, tx: &mut WriteTx<'_>) -> Result<(), NativeInstallationError> {
		for binding in &self.bindings {
			let scope = binding.descriptor().scope();
			let physical = |error| NativeInstallationError::InitialDocumentIndexPhysical {
				scope: scope.clone(),
				source: Box::new(error),
			};
			let expected = PgDocumentIndexExpectedCatalog::from_descriptor(binding.descriptor())
				.map_err(physical)?;
			let PgDocumentIndexCatalogObservation::Absent { capacity } =
				inspect_document_index(tx, &expected)
					.await
					.map_err(physical)?
			else {
				return Err(NativeInstallationError::InitialDocumentIndexNameOccupied {
					scope: scope.clone(),
				});
			};
			let statement = create_index_statement(binding, &capacity).map_err(physical)?;
			tx.record_database_operation(
				DatabaseOperationKind::Execute,
				CREATE_DOCUMENT_INDEX_LABEL,
				Some(statement.as_str()),
			);
			owned_portable_query(statement)
				.execute(tx.inner.as_mut())
				.await
				.map_err(|error| {
					physical(PgDocumentIndexPhysicalError::Database(
						error.with_operation_label(CREATE_DOCUMENT_INDEX_LABEL),
					))
				})?;
			if !matches!(
				inspect_document_index(tx, &expected)
					.await
					.map_err(physical)?,
				PgDocumentIndexCatalogObservation::Present
			) {
				return Err(NativeInstallationError::InitialDocumentIndexAbsent {
					scope: scope.clone(),
				});
			}
		}
		Ok(())
	}
}

/// Checks that `registration` records exactly one ledger entry for each query-ready index in
/// `policies`, which a new installation's opening creates in the creation transaction.
pub(super) fn require_initial_index_ledger(
	policies: &ScopePolicySet,
	registration: &PgBackendRegistration,
) -> Result<(), NativeInstallationError> {
	let mut ready = 0_usize;
	for policy in policies.policies() {
		let ScopeOwner::Document(document) = policy.owner() else {
			continue;
		};
		for index in document.ready_indexes() {
			ready += 1;
			if !registration.document_indexes().iter().any(|entry| {
				entry.scope() == policy.identity() && entry.index().identity() == index
			}) {
				return Err(NativeInstallationError::InitialDocumentIndexUnrecorded {
					scope: policy.identity().clone(),
				});
			}
		}
	}
	if registration.document_indexes().len() != ready {
		return Err(NativeInstallationError::InitialDocumentIndexLedgerExtra);
	}
	Ok(())
}
