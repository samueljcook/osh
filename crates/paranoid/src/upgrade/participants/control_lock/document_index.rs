//! Fixed physical document-index work on the canonical guard's private transaction.

use crate::db::{
	DatabaseError, DatabaseOperationKind, SchemaError, TransactionFailure, owned_portable_query,
};
use crate::installation::creation::NativeInstallationError;
use crate::kv::document::postgres::index::managed::PgManagedDocumentIndexBinding;
use crate::kv::document::postgres::index::physical::{
	PgDocumentIndexCatalogObservation, PgDocumentIndexExpectedCatalog,
	PgDocumentIndexPhysicalError, PgIndexKeyCapacity, create_index_statement,
	inspect_document_index,
};
use crate::upgrade::participants::NativeUpgradeParticipantsError;
use crate::upgrade::participants::document_index::DocumentIndexRegistrationError;

use super::{InspectionState, PgParticipantControlLock};

const CREATE_LABEL: &str = "db.state_storage.document_index.create";

/// Physical inspection has no registration, Ready or adoption effect.
#[derive(Debug, thiserror::Error)]
pub(crate) enum PgDocumentIndexInspectionError {
	#[error("document index container validation failed")]
	StorageValidation(#[source] Box<NativeInstallationError>),
	#[error("document index physical catalog inspection failed")]
	Physical(#[source] Box<PgDocumentIndexPhysicalError>),
}

impl PgDocumentIndexInspectionError {
	fn storage_validation(error: SchemaError) -> Self {
		Self::StorageValidation(Box::new(error.into()))
	}
	fn physical(error: PgDocumentIndexPhysicalError) -> Self {
		Self::Physical(Box::new(error))
	}
	pub(crate) fn database_error(&self) -> Option<&DatabaseError> {
		match self {
			Self::StorageValidation(error) => error.database_error(),
			Self::Physical(error) => error.database_error(),
		}
	}
	pub(crate) fn involves_database_failure(&self) -> bool {
		match self {
			Self::StorageValidation(error) => error.involves_database_failure(),
			Self::Physical(error) => error.database_error().is_some(),
		}
	}
	pub(crate) fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::StorageValidation(error) => error.commit_outcome_is_unknown(),
			Self::Physical(error) => error
				.database_error()
				.is_some_and(DatabaseError::commit_outcome_is_unknown),
		}
	}
}

impl PgParticipantControlLock<'_, '_> {
	/// Validates the actual container and inspects only its exact configured index name.
	pub(crate) async fn inspect_document_index(
		&mut self,
		binding: &PgManagedDocumentIndexBinding<'_>,
	) -> Result<PgDocumentIndexCatalogObservation, PgDocumentIndexInspectionError> {
		let tx = self.physical.companion_tx();
		let mut completion = tx.track_state_mutation();
		binding
			.storage()
			.validate_existing(tx)
			.await
			.map_err(PgDocumentIndexInspectionError::storage_validation)?;
		let expected = PgDocumentIndexExpectedCatalog::from_descriptor(binding.descriptor())
			.map_err(PgDocumentIndexInspectionError::physical)?;
		let observation = inspect_document_index(tx, &expected)
			.await
			.map_err(PgDocumentIndexInspectionError::physical)?;
		completion.complete();
		Ok(observation)
	}

	/// Only this same guard's complete compatible inspection permits the fixed CREATE statement.
	/// A failed or cancelled create/postinspection retains mandatory transaction rollback.
	pub(in crate::upgrade) async fn create_and_inspect_document_index(
		&mut self,
		binding: &PgManagedDocumentIndexBinding<'_>,
		capacity: &PgIndexKeyCapacity,
	) -> Result<(), NativeUpgradeParticipantsError> {
		let inspection_compatible = matches!(self.inspection, InspectionState::Compatible);
		let tx = self.physical.companion_tx();
		let mut completion = tx.track_state_mutation();
		if !inspection_compatible {
			return Err(NativeUpgradeParticipantsError::InspectionNotCompleted);
		}
		let statement = create_index_statement(binding, capacity)
			.map_err(DocumentIndexRegistrationError::physical)?;
		let expected = PgDocumentIndexExpectedCatalog::from_descriptor(binding.descriptor())
			.map_err(DocumentIndexRegistrationError::physical)?;
		tx.record_database_operation(
			DatabaseOperationKind::Execute,
			CREATE_LABEL,
			Some(statement.as_str()),
		);
		owned_portable_query(statement)
			.execute(tx.inner.as_mut())
			.await
			.map_err(|error| {
				NativeUpgradeParticipantsError::Database(error.with_operation_label(CREATE_LABEL))
			})?;
		let observation = inspect_document_index(tx, &expected)
			.await
			.map_err(DocumentIndexRegistrationError::physical)?;
		if !matches!(observation, PgDocumentIndexCatalogObservation::Present) {
			return Err(DocumentIndexRegistrationError::CreatedIndexAbsent.into());
		}
		completion.complete();
		Ok(())
	}
}
