//! Fixed maintenance binds actual Queue code once and reuses consuming source relocation.

use super::{
	Arc, GroupOperation, NativeAdmissionError, NativeQueueGroupError, NativeQueueGroupInner,
	NativeQueueGroupOutcome, NativeQueueSourceBindings, PgNativeQueueGroup,
	QueueAuxiliaryComponent, QueueJobComponent, QueueRelocationRecord,
	QueueSourceRelocationOutcome, TransactionFailure, WriteTx,
};
use crate::installation::contract::{
	BoundComponentContract, ComponentDeclaration, ComponentPolicy,
};
use crate::queue::v10::source_auxiliary_bridge::QueueAuxiliarySourceBridge;
use crate::queue::v10::source_bridge::QueueSourceBridge;
use crate::queue::v10::source_group_storage::QueueSourceGroupReaders;
use crate::queue::v10::source_group_storage::context::QueueSourceContext;
use crate::queue::v10::source_imports::QueueSourceImportDefs;
use crate::runtime::admission::upgrade::contracts::NativeUpgradeContracts;

/// Declarations borrow the same immutable instances used by the actual retained store.
pub(crate) struct QueueSourceConversionDeclarations<'defs> {
	inner: &'defs NativeQueueGroupInner,
	source: &'defs NativeQueueSourceBindings,
	jobs: ComponentDeclaration<'defs, QueueJobComponent>,
	auxiliary: ComponentDeclaration<'defs, QueueAuxiliaryComponent>,
	source_jobs: ComponentDeclaration<'defs, QueueSourceBridge>,
	source_auxiliary: ComponentDeclaration<'defs, QueueAuxiliarySourceBridge>,
}

/// Settings and policies are bound once; the actual group borrows them through every attempt.
pub(crate) struct BoundQueueSourceConversion<'bound, 'defs, 'current> {
	declarations: &'bound QueueSourceConversionDeclarations<'defs>,
	contracts: &'bound NativeUpgradeContracts<'current>,
	jobs: BoundComponentContract<'bound, 'defs, QueueJobComponent>,
	auxiliary: BoundComponentContract<'bound, 'defs, QueueAuxiliaryComponent>,
	source_jobs: BoundComponentContract<'bound, 'defs, QueueSourceBridge>,
	source_auxiliary: BoundComponentContract<'bound, 'defs, QueueAuxiliarySourceBridge>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum QueueSourceConversionError {
	#[error("source Queue conversion definitions differ from the store")]
	DefInstanceMismatch,
	#[error("source Queue conversion has no storage")]
	SourceMappingAbsent,
	#[error("source Queue conversion participant support failed")]
	Admission(#[source] NativeAdmissionError),
	#[error("source Queue conversion policy or operation failed")]
	Queue(#[source] Box<NativeQueueGroupError>),
	#[error("source Queue conversion returned a result outside physical relocation")]
	UnexpectedOutcome,
}

impl QueueSourceConversionError {
	/// Returns an actual retained driver cause without replacing the primary typed failure.
	pub(crate) fn database_error(&self) -> Option<&crate::db::DatabaseError> {
		match self {
			Self::Admission(error) => error.database_error(),
			Self::Queue(error) => error.database_error(),
			_ => None,
		}
	}

	pub(crate) fn involves_database_failure(&self) -> bool {
		match self {
			Self::Admission(error) => error.involves_database_failure(),
			Self::Queue(error) => error.involves_database_failure(),
			_ => false,
		}
	}
	pub(crate) fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Admission(error) => error.commit_outcome_is_unknown(),
			Self::Queue(error) => error.commit_outcome_is_unknown(),
			_ => false,
		}
	}
	fn queue(error: NativeQueueGroupError) -> Self {
		Self::Queue(Box::new(error))
	}
}

impl PgNativeQueueGroup {
	/// Rejects same-name substitutes before creating declarations or observing any source row.
	pub(crate) fn source_conversion_declarations<'defs>(
		&'defs self,
		jobs: &Arc<QueueJobComponent>,
		auxiliary: &Arc<QueueAuxiliaryComponent>,
		imports: &Arc<QueueSourceImportDefs>,
	) -> Result<QueueSourceConversionDeclarations<'defs>, QueueSourceConversionError> {
		let inner = self.inner.as_ref();
		let source = inner
			.bindings
			.source
			.as_deref()
			.ok_or(QueueSourceConversionError::SourceMappingAbsent)?;
		if !Arc::ptr_eq(jobs, &inner.bindings.jobs)
			|| !Arc::ptr_eq(auxiliary, &inner.bindings.auxiliary)
			|| !Arc::ptr_eq(imports, &source.imports)
		{
			return Err(QueueSourceConversionError::DefInstanceMismatch);
		}
		Ok(QueueSourceConversionDeclarations {
			inner,
			source,
			jobs: inner.bindings.jobs.declaration(),
			auxiliary: inner.bindings.auxiliary.declaration(),
			source_jobs: source.imports.job_declaration(),
			source_auxiliary: source.imports.auxiliary_declaration(),
		})
	}
}

impl<'defs> QueueSourceConversionDeclarations<'defs> {
	pub(crate) fn bind<'bound, 'current>(
		&'bound self,
		contracts: &'bound NativeUpgradeContracts<'current>,
	) -> Result<BoundQueueSourceConversion<'bound, 'defs, 'current>, QueueSourceConversionError> {
		QueueSourceContext::Upgrade(contracts)
			.require_queue_source_storage(
				self.source.storage.descriptor(),
				self.source.storage.native_storage(),
			)
			.map_err(|error| {
				QueueSourceConversionError::queue(NativeQueueGroupError::SourceObservation(
					error.into(),
				))
			})?;
		let jobs = contracts
			.bind(&self.jobs)
			.map_err(QueueSourceConversionError::Admission)?;
		let auxiliary = contracts
			.bind(&self.auxiliary)
			.map_err(QueueSourceConversionError::Admission)?;
		let source_jobs = contracts
			.bind(&self.source_jobs)
			.map_err(QueueSourceConversionError::Admission)?;
		let source_auxiliary = contracts
			.bind(&self.source_auxiliary)
			.map_err(QueueSourceConversionError::Admission)?;
		let readers = QueueSourceGroupReaders {
			jobs: &self.inner.bindings.jobs,
			job_contract: &jobs,
			imports: &self.source.imports,
			source_job_contract: &source_jobs,
			auxiliary: &self.inner.bindings.auxiliary,
			auxiliary_contract: &auxiliary,
			source_auxiliary_contract: &source_auxiliary,
		};
		readers
			.require_layout(self.source.storage.descriptor().layout().queue_layout())
			.map_err(|error| {
				QueueSourceConversionError::queue(NativeQueueGroupError::SourceObservation(error))
			})?;
		Ok(BoundQueueSourceConversion {
			declarations: self,
			contracts,
			jobs,
			auxiliary,
			source_jobs,
			source_auxiliary,
		})
	}
}

impl BoundQueueSourceConversion<'_, '_, '_> {
	pub(crate) fn selected_policies(&self) -> Vec<ComponentPolicy> {
		vec![
			self.jobs.policy().clone(),
			self.auxiliary.policy().clone(),
			self.source_jobs.policy().clone(),
			self.source_auxiliary.policy().clone(),
		]
	}

	/// The fixed progress owner supplies its exact Tx; the result grants no Claim or coverage.
	pub(crate) async fn convert_record(
		&self,
		tx: &mut WriteTx<'_>,
		record: QueueRelocationRecord,
	) -> Result<QueueSourceRelocationOutcome, QueueSourceConversionError> {
		let inner = self.declarations.inner;
		let source = self.declarations.source;
		let bound = inner
			.bindings
			.auxiliary
			.bind(
				&self.auxiliary,
				&inner.bindings.jobs,
				&self.jobs,
				&inner.namespaces,
			)
			.map_err(|error| {
				QueueSourceConversionError::queue(NativeQueueGroupError::PolicyBinding(error))
			})?;
		let readers = QueueSourceGroupReaders {
			jobs: &inner.bindings.jobs,
			job_contract: &self.jobs,
			imports: &source.imports,
			source_job_contract: &self.source_jobs,
			auxiliary: &inner.bindings.auxiliary,
			auxiliary_contract: &self.auxiliary,
			source_auxiliary_contract: &self.source_auxiliary,
		};
		let outcome = inner
			.execute_source(
				tx,
				QueueSourceContext::Upgrade(self.contracts),
				&bound,
				source,
				&readers,
				&GroupOperation::Relocate(record),
			)
			.await
			.map_err(QueueSourceConversionError::queue)?
			.map_err(|rejection| {
				QueueSourceConversionError::queue(NativeQueueGroupError::Rejected(rejection))
			})?;
		match outcome {
			NativeQueueGroupOutcome::SourceRelocation(outcome) => Ok(outcome),
			_ => Err(QueueSourceConversionError::UnexpectedOutcome),
		}
	}
}
