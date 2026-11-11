//! Actual selected-operation errors without cause flattening or commit-outcome reinterpretation.

use crate::db::TransactionFailure;
use crate::runtime::admission::upgrade::NativeUpgradeAdmissionError;
use crate::upgrade::participants::NativeUpgradeParticipantsError;
use crate::upgrade::participants::document_index::DocumentIndexRegistrationError;
use crate::upgrade::participants::document_readiness::DocumentReadyError;
use crate::upgrade::participants::v10::source_data::NativeSourceDataRetirementError;
use crate::upgrade::participants::v10::source_writer::NativeSourceWriterRetirementError;
use crate::upgrade::requests::batch::NativeUpgradeBatchError;

use super::super::selection::PgUpgradeStep;
use super::index_placements::NativeUpgradeIndexPlacementError;

#[derive(Debug, thiserror::Error)]
#[error("selected fixed upgrade operation failed")]
pub(crate) struct NativeUpgradeStepExecutionError {
	step: PgUpgradeStep,
	#[source]
	source: Box<NativeUpgradeStepExecutionCauseError>,
}
impl NativeUpgradeStepExecutionError {
	pub(in crate::upgrade) fn driver_reason(
		&self,
	) -> super::failure::NativeUpgradeStepFailureReason<'_> {
		super::failure::classify_cause(&self.source)
	}
	pub(crate) fn database_error(&self) -> Option<&crate::db::DatabaseError> {
		self.source.database_error()
	}

	pub(super) fn new(step: PgUpgradeStep, source: NativeUpgradeStepExecutionCauseError) -> Self {
		Self {
			step,
			source: Box::new(source),
		}
	}
	pub(crate) fn step(&self) -> &PgUpgradeStep {
		&self.step
	}
	pub(crate) fn typed_source(&self) -> &NativeUpgradeStepExecutionCauseError {
		&self.source
	}
	pub(crate) fn involves_database_failure(&self) -> bool {
		self.source.involves_database_failure()
	}
	pub(crate) fn commit_outcome_is_unknown(&self) -> bool {
		self.source.commit_outcome_is_unknown()
	}
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeUpgradeStepExecutionCauseError {
	#[error("upgrade execution observation has no complete Work")]
	WorkAbsent,
	#[error("upgrade execution observation differs from installation or resolved target")]
	ContextMismatch,
	#[error("upgrade execution local continuation enrollment failed")]
	Admission(#[source] Box<NativeUpgradeAdmissionError>),
	#[error("upgrade execution physical index name/code association failed")]
	IndexPlacement(#[source] Box<NativeUpgradeIndexPlacementError>),
	#[error("upgrade execution physical index request parsing failed")]
	IndexRequest(#[source] Box<DocumentIndexRegistrationError>),
	#[error("upgrade execution Ready request parsing failed")]
	ReadyRequest(#[source] Box<DocumentReadyError>),
	#[error("upgrade execution effective fixed actor failed")]
	Participants(#[source] Box<NativeUpgradeParticipantsError>),
	#[error("upgrade execution source writer retirement failed")]
	SourceWriter(#[source] Box<NativeSourceWriterRetirementError>),
	#[error("upgrade execution source data retirement failed")]
	SourceData(#[source] Box<NativeSourceDataRetirementError>),
	#[error("upgrade execution batch failed")]
	Batch(#[source] Box<NativeUpgradeBatchError>),
}
impl NativeUpgradeStepExecutionCauseError {
	pub(crate) fn database_error(&self) -> Option<&crate::db::DatabaseError> {
		match self {
			Self::Admission(error) => error.database_error(),
			Self::IndexPlacement(error) => error.database_error(),
			Self::IndexRequest(error) => error.database_error(),
			Self::ReadyRequest(error) => error.database_error(),
			Self::Participants(error) => error.database_error(),
			Self::SourceWriter(error) | Self::SourceData(error) => {
				super::failure::source_retirement_database_error(error)
			}
			Self::Batch(error) => error.database_error(),
			Self::WorkAbsent | Self::ContextMismatch => None,
		}
	}

	pub(crate) fn involves_database_failure(&self) -> bool {
		match self {
			Self::Admission(error) => error.involves_database_failure(),
			Self::IndexPlacement(error) => error.involves_database_failure(),
			Self::IndexRequest(error) => error.involves_database_failure(),
			Self::ReadyRequest(error) => error.involves_database_failure(),
			Self::Participants(error) => error.involves_database_failure(),
			Self::SourceWriter(error) => error.involves_database_failure(),
			Self::SourceData(error) => error.involves_database_failure(),
			Self::Batch(error) => error.involves_database_failure(),
			Self::WorkAbsent | Self::ContextMismatch => false,
		}
	}
	pub(crate) fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Admission(error) => error.commit_outcome_is_unknown(),
			Self::IndexPlacement(error) => error.commit_outcome_is_unknown(),
			Self::IndexRequest(error) => error.commit_outcome_is_unknown(),
			Self::ReadyRequest(error) => error.commit_outcome_is_unknown(),
			Self::Participants(error) => error.commit_outcome_is_unknown(),
			Self::SourceWriter(error) => error.commit_outcome_is_unknown(),
			Self::SourceData(error) => error.commit_outcome_is_unknown(),
			Self::Batch(error) => error.commit_outcome_is_unknown(),
			Self::WorkAbsent | Self::ContextMismatch => false,
		}
	}
}
