//! Physical names of document indexes, derived from the scope and the index identity.

use crate::db::{PgIdentifier, TransactionFailure};
use crate::installation::creation::NativeInstallationError;
use crate::kv::document::postgres::index::derived_index_name;
use crate::kv::document::postgres::index::managed::{
	PgManagedDocumentIndexBinding, PgManagedDocumentIndexBindingFields, PgManagedDocumentIndexError,
};
use crate::kv::document::schema::IndexContractIdentity;
use crate::kv::document::schema::managed::ManagedDocumentRegistryError;
use crate::runtime::targets::ResolvedNativeRuntimeTarget;
use crate::scope::{ScopeIdentity, ScopeOwner};
use crate::upgrade::requests::work::observation::NativeUpgradeControllerObservation;

pub(super) struct NativeUpgradeIndexResolutionFields<'a> {
	pub(super) observation: &'a NativeUpgradeControllerObservation,
	pub(super) resolved: &'a ResolvedNativeRuntimeTarget,
	pub(super) scope: &'a ScopeIdentity,
	pub(super) index: &'a IndexContractIdentity,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeUpgradeIndexPlacementError {
	#[error("upgrade physical index is not requested Ready by the Document target")]
	NotRequestedReady {
		scope: ScopeIdentity,
		index: IndexContractIdentity,
	},
	#[error("upgrade physical index registration is unavailable")]
	Registry {
		scope: ScopeIdentity,
		#[source]
		source: Box<ManagedDocumentRegistryError>,
	},
	#[error("upgrade physical index code and placement binding failed")]
	Binding {
		scope: ScopeIdentity,
		index: IndexContractIdentity,
		#[source]
		source: Box<PgManagedDocumentIndexError>,
	},
	#[error("upgrade physical index observed ledger conflicts with its descriptor")]
	Ledger {
		scope: ScopeIdentity,
		index: IndexContractIdentity,
		#[source]
		source: Box<NativeInstallationError>,
	},
}
impl NativeUpgradeIndexPlacementError {
	/// Returns an actual retained driver cause without replacing the primary typed failure.
	pub(crate) fn database_error(&self) -> Option<&crate::db::DatabaseError> {
		match self {
			Self::Ledger { source, .. } => source.database_error(),
			_ => None,
		}
	}

	pub(crate) fn involves_database_failure(&self) -> bool {
		match self {
			Self::Ledger { source, .. } => source.involves_database_failure(),
			_ => false,
		}
	}
	pub(crate) fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Ledger { source, .. } => source.commit_outcome_is_unknown(),
			_ => false,
		}
	}
}

pub(super) fn resolve_index_name(
	fields: NativeUpgradeIndexResolutionFields<'_>,
) -> Result<PgIdentifier, NativeUpgradeIndexPlacementError> {
	let NativeUpgradeIndexResolutionFields {
		observation,
		resolved,
		scope,
		index,
	} = fields;
	let target = resolved
		.target()
		.request()
		.scope_replacements()
		.and_then(|targets| targets.lookup(scope).ok());
	if !target.is_some_and(|target| matches!(target.owner(), ScopeOwner::Document(document) if document.is_index_ready(index))) {
			return Err(NativeUpgradeIndexPlacementError::NotRequestedReady { scope: scope.clone(), index: index.clone() });
		}
	let recorded = observation
		.backend()
		.document_indexes()
		.iter()
		.find(|entry| entry.scope() == scope && entry.index().identity() == index);
	let index_name = match recorded {
		Some(recorded) => recorded.index_name().clone(),
		// A recorded name stays authoritative.
		None => derived_index_name(scope, index),
	};
	let registration = resolved
		.code()
		.document_registry()
		.lookup(scope)
		.map_err(|error| NativeUpgradeIndexPlacementError::Registry {
			scope: scope.clone(),
			source: Box::new(error),
		})?;
	let binding = PgManagedDocumentIndexBinding::new(PgManagedDocumentIndexBindingFields {
		registration,
		index,
		native: resolved.code().administration().native_bindings(),
		index_name: index_name.clone(),
	})
	.map_err(|error| NativeUpgradeIndexPlacementError::Binding {
		scope: scope.clone(),
		index: index.clone(),
		source: Box::new(error),
	})?;
	observation
		.backend()
		.require_document_index_slot(binding.descriptor())
		.map_err(|error| NativeUpgradeIndexPlacementError::Ledger {
			scope: scope.clone(),
			index: index.clone(),
			source: Box::new(NativeInstallationError::from(error)),
		})?;
	Ok(index_name)
}
