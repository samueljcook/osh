//! Durable bounded selected-writer projection verification metadata.
//!
//! These pure transitions prove no observation, writer exclusion, adoption or Ready permission.
//! A fixed executor must compare actual stored maps and persist its checkpoint atomically.

use crate::records::RecordKey;
use crate::scope::{ScopeIdentity, ScopePolicy, ScopePolicySet};
use crate::upgrade::policy::document_stage::DocumentWriterStage;

use super::document_checkpoint::{
	DocumentCheckpointContext, DocumentConversionCheckpoint, DocumentScanProgress,
	require_target_closure,
};
use super::{CompleteUpgradeTarget, PgUpgradeStatus, UpgradeWork, UpgradeWorkError};

#[cfg(test)]
mod tests;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::upgrade) enum DocumentVerificationProcedure {
	SelectedWriterProjectionEqualityV1,
}

/// The executor must observe this inclusive maximum after the actual writer preservation barrier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::upgrade) enum DocumentVerificationExtent<Key = RecordKey> {
	Empty,
	Through { upper_key: Key },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::upgrade) enum DocumentVerificationProgress<Key = RecordKey> {
	Scanning { after_key: Option<Key> },
	Verified { last_key: Option<Key> },
}

pub(in crate::upgrade) enum DocumentVerificationUpdate {
	Advance { after_key: RecordKey },
	Verified { last_key: Option<RecordKey> },
}

/// A first batch records the captured extent and page together; continuation retains that extent.
pub(in crate::upgrade) enum DocumentVerificationBatch {
	Start {
		extent: DocumentVerificationExtent,
		update: DocumentVerificationUpdate,
	},
	Continue {
		update: DocumentVerificationUpdate,
	},
}

pub(in crate::upgrade) struct DocumentVerificationCheckpoint {
	pub(super) invariant: ScopePolicy,
	pub(super) procedure: DocumentVerificationProcedure,
	state: DocumentVerificationState,
}

#[derive(Debug, Eq, PartialEq)]
enum DocumentVerificationState {
	EmptyVerified,
	Scanning {
		upper_key: RecordKey,
		after_key: Option<RecordKey>,
	},
	Verified {
		upper_key: RecordKey,
		last_key: Option<RecordKey>,
	},
}

impl DocumentVerificationCheckpoint {
	pub(super) fn from_fields(
		invariant: ScopePolicy,
		procedure: DocumentVerificationProcedure,
		extent: DocumentVerificationExtent,
		progress: DocumentVerificationProgress,
	) -> Result<Self, DocumentVerificationError> {
		require_bounds(extent.as_ref(), progress.as_ref())?;
		let state = match (extent, progress) {
			(
				DocumentVerificationExtent::Empty,
				DocumentVerificationProgress::Verified { last_key: None },
			) => DocumentVerificationState::EmptyVerified,
			(
				DocumentVerificationExtent::Through { upper_key },
				DocumentVerificationProgress::Scanning { after_key },
			) => DocumentVerificationState::Scanning {
				upper_key,
				after_key,
			},
			(
				DocumentVerificationExtent::Through { upper_key },
				DocumentVerificationProgress::Verified { last_key },
			) => DocumentVerificationState::Verified {
				upper_key,
				last_key,
			},
			(DocumentVerificationExtent::Empty, _) => {
				unreachable!("empty verification progress was parsed")
			}
		};
		Ok(Self {
			invariant,
			procedure,
			state,
		})
	}
	pub(crate) fn invariant(&self) -> &ScopePolicy {
		&self.invariant
	}
	pub(crate) fn procedure(&self) -> DocumentVerificationProcedure {
		self.procedure
	}
	pub(in crate::upgrade) fn extent(&self) -> DocumentVerificationExtent<&RecordKey> {
		match &self.state {
			DocumentVerificationState::EmptyVerified => DocumentVerificationExtent::Empty,
			DocumentVerificationState::Scanning { upper_key, .. }
			| DocumentVerificationState::Verified { upper_key, .. } => {
				DocumentVerificationExtent::Through { upper_key }
			}
		}
	}
	pub(crate) fn progress(&self) -> DocumentVerificationProgress<&RecordKey> {
		match &self.state {
			DocumentVerificationState::EmptyVerified => {
				DocumentVerificationProgress::Verified { last_key: None }
			}
			DocumentVerificationState::Scanning { after_key, .. } => {
				DocumentVerificationProgress::Scanning {
					after_key: after_key.as_ref(),
				}
			}
			DocumentVerificationState::Verified { last_key, .. } => {
				DocumentVerificationProgress::Verified {
					last_key: last_key.as_ref(),
				}
			}
		}
	}
	fn advance(
		&mut self,
		update: DocumentVerificationUpdate,
	) -> Result<(), DocumentVerificationError> {
		let DocumentVerificationState::Scanning {
			upper_key,
			after_key,
		} = &self.state
		else {
			return Err(DocumentVerificationError::AlreadyVerified);
		};
		let next = next_progress(
			DocumentVerificationExtent::Through { upper_key },
			after_key.as_ref(),
			update,
		)?;
		match next {
			DocumentVerificationProgress::Scanning { after_key: next } => {
				let DocumentVerificationState::Scanning { after_key, .. } = &mut self.state else {
					unreachable!("scanning state was checked")
				};
				*after_key = next;
			}
			DocumentVerificationProgress::Verified { last_key } => {
				let DocumentVerificationState::Scanning { upper_key, .. } =
					std::mem::replace(&mut self.state, DocumentVerificationState::EmptyVerified)
				else {
					unreachable!("scanning state was checked")
				};
				self.state = DocumentVerificationState::Verified {
					upper_key,
					last_key,
				};
			}
		}
		Ok(())
	}
	fn matches(&self, other: &Self) -> bool {
		self.procedure == other.procedure
			&& self.state == other.state
			&& self.invariant.matches_policy(&other.invariant)
	}
}

impl<Key> DocumentVerificationExtent<Key> {
	fn as_ref(&self) -> DocumentVerificationExtent<&Key> {
		match self {
			Self::Empty => DocumentVerificationExtent::Empty,
			Self::Through { upper_key } => DocumentVerificationExtent::Through { upper_key },
		}
	}
}
impl DocumentVerificationExtent<&RecordKey> {
	pub(crate) fn cloned(self) -> DocumentVerificationExtent {
		match self {
			Self::Empty => DocumentVerificationExtent::Empty,
			Self::Through { upper_key } => DocumentVerificationExtent::Through {
				upper_key: upper_key.clone(),
			},
		}
	}
}
impl<Key> DocumentVerificationProgress<Key> {
	fn as_ref(&self) -> DocumentVerificationProgress<&Key> {
		match self {
			Self::Scanning { after_key } => DocumentVerificationProgress::Scanning {
				after_key: after_key.as_ref(),
			},
			Self::Verified { last_key } => DocumentVerificationProgress::Verified {
				last_key: last_key.as_ref(),
			},
		}
	}
}

impl std::fmt::Debug for DocumentVerificationCheckpoint {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("DocumentVerificationCheckpoint")
			.field("procedure", &self.procedure)
			.field("extent", &self.extent())
			.field("progress", &self.progress())
			.finish_non_exhaustive()
	}
}

impl UpgradeWork {
	pub(in crate::upgrade) fn document_verification_checkpoints(
		&self,
	) -> &[DocumentVerificationCheckpoint] {
		self.progress().map_or(&[], |progress| {
			progress.document_verification_checkpoints.as_slice()
		})
	}
	pub(in crate::upgrade) fn document_verification(
		&self,
		scope: &ScopeIdentity,
	) -> Option<&DocumentVerificationCheckpoint> {
		self.document_verification_checkpoints()
			.iter()
			.find(|checkpoint| checkpoint.invariant.identity() == scope)
	}

	/// Checks complete actual next scopes before preserving verification through an effective
	/// change. This pure comparison grants no admission, data proof or physical mapping authority.
	pub(in crate::upgrade) fn require_document_verification_invariants(
		&self,
		scopes: &ScopePolicySet,
	) -> Result<(), UpgradeWorkError> {
		self.require_verification_invariants(scopes, None)
	}
	pub(super) fn require_verification_invariants(
		&self,
		scopes: &ScopePolicySet,
		excluded: Option<usize>,
	) -> Result<(), UpgradeWorkError> {
		for (index, checkpoint) in self.document_verification_checkpoints().iter().enumerate() {
			if excluded == Some(index) {
				continue;
			}
			let current = scopes
				.lookup(checkpoint.invariant.identity())
				.map_err(|_| DocumentVerificationError::CurrentInvariantMismatch)?;
			if !checkpoint.invariant.matches_policy(current) {
				return Err(DocumentVerificationError::CurrentInvariantMismatch.into());
			}
		}
		Ok(())
	}

	fn require_verification_context(
		&self,
		context: &DocumentCheckpointContext<'_>,
		stage: &DocumentWriterStage,
	) -> Result<(), UpgradeWorkError> {
		self.require_document_context(context, stage)?;
		if self.status() != PgUpgradeStatus::Building {
			return Err(DocumentVerificationError::BuildingRequired.into());
		}
		let conversion = self
			.document_conversion_checkpoint(stage.policy().identity())
			.ok_or(DocumentVerificationError::ConversionNotScanned)?;
		if !stage.matches_policy(conversion.invariant())
			|| !matches!(conversion.progress(), DocumentScanProgress::Scanned { .. })
		{
			return Err(DocumentVerificationError::ConversionNotScanned.into());
		}
		Ok(())
	}

	/// Checks the actual slot once and records one first or subsequent verification batch
	/// atomically. Start carries the captured extent and first page; Continue cannot replace that
	/// extent. The executor must establish writer exclusion, compare actual maps and persist them
	/// together.
	pub(in crate::upgrade) fn record_document_verification_batch(
		&mut self,
		context: DocumentCheckpointContext<'_>,
		stage: &DocumentWriterStage,
		batch: DocumentVerificationBatch,
	) -> Result<(), UpgradeWorkError> {
		self.require_verification_context(&context, stage)?;
		let position = self
			.document_verification_checkpoints()
			.iter()
			.position(|checkpoint| checkpoint.invariant.identity() == stage.policy().identity());
		match batch {
			DocumentVerificationBatch::Start { extent, update } => {
				if position.is_some() {
					return Err(DocumentVerificationError::AlreadyStarted.into());
				}
				let progress = next_progress(extent.as_ref(), None, update)?;
				let next = DocumentVerificationCheckpoint::from_fields(
					stage.policy().clone(),
					DocumentVerificationProcedure::SelectedWriterProjectionEqualityV1,
					extent,
					progress,
				)?;
				self.building_progress_mut()?
					.document_verification_checkpoints
					.push(next);
			}
			DocumentVerificationBatch::Continue { update } => {
				let index = position.ok_or(DocumentVerificationError::NotStarted)?;
				let checkpoint = &self.document_verification_checkpoints()[index];
				if !stage.matches_policy(&checkpoint.invariant) {
					return Err(DocumentVerificationError::CurrentInvariantMismatch.into());
				}
				self.building_progress_mut()?
					.document_verification_checkpoints[index]
					.advance(update)?;
			}
		}
		Ok(())
	}
}

pub(super) fn matches_checkpoints(
	left: &[DocumentVerificationCheckpoint],
	right: &[DocumentVerificationCheckpoint],
) -> bool {
	left.len() == right.len()
		&& left
			.iter()
			.all(|checkpoint| right.iter().any(|other| checkpoint.matches(other)))
}

pub(super) fn validate_checkpoints(
	target: &CompleteUpgradeTarget,
	status: PgUpgradeStatus,
	checkpoints: &[DocumentVerificationCheckpoint],
	conversions: &[DocumentConversionCheckpoint],
) -> Result<(), UpgradeWorkError> {
	if !checkpoints.is_empty() {
		match status {
			PgUpgradeStatus::Queued => return Err(DocumentVerificationError::QueuedProgress.into()),
			PgUpgradeStatus::Completed => {
				return Err(DocumentVerificationError::CompletedProgress.into());
			}
			PgUpgradeStatus::Building | PgUpgradeStatus::Cancelled => {}
		}
	}
	for (index, checkpoint) in checkpoints.iter().enumerate() {
		if checkpoints[..index]
			.iter()
			.any(|prior| prior.invariant.identity() == checkpoint.invariant.identity())
		{
			return Err(DocumentVerificationError::DuplicateScope { index }.into());
		}
		require_target_closure(target, &checkpoint.invariant)?;
		let conversion = conversions
			.iter()
			.find(|conversion| conversion.invariant().identity() == checkpoint.invariant.identity())
			.ok_or(DocumentVerificationError::ConversionNotScanned)?;
		if !checkpoint.invariant.matches_policy(conversion.invariant())
			|| !matches!(conversion.progress(), DocumentScanProgress::Scanned { .. })
		{
			return Err(DocumentVerificationError::ConversionNotScanned.into());
		}
		require_bounds(checkpoint.extent(), checkpoint.progress())?;
	}
	Ok(())
}

fn next_progress(
	extent: DocumentVerificationExtent<&RecordKey>,
	after_key: Option<&RecordKey>,
	update: DocumentVerificationUpdate,
) -> Result<DocumentVerificationProgress, DocumentVerificationError> {
	let next = match update {
		DocumentVerificationUpdate::Advance { after_key: next } => {
			if after_key.is_some_and(|previous| next <= *previous) {
				return Err(DocumentVerificationError::CursorNotAdvanced);
			}
			DocumentVerificationProgress::Scanning {
				after_key: Some(next),
			}
		}
		DocumentVerificationUpdate::Verified { last_key } => {
			if after_key
				.is_some_and(|previous| last_key.as_ref().is_none_or(|last| last < previous))
			{
				return Err(DocumentVerificationError::CursorRewind);
			}
			DocumentVerificationProgress::Verified { last_key }
		}
	};
	require_bounds(extent, next.as_ref())?;
	Ok(next)
}

fn require_bounds(
	extent: DocumentVerificationExtent<&RecordKey>,
	progress: DocumentVerificationProgress<&RecordKey>,
) -> Result<(), DocumentVerificationError> {
	match (extent, progress) {
		(
			DocumentVerificationExtent::Empty,
			DocumentVerificationProgress::Verified { last_key: None },
		) => Ok(()),
		(DocumentVerificationExtent::Empty, _) => {
			Err(DocumentVerificationError::InvalidEmptyProgress)
		}
		(
			DocumentVerificationExtent::Through { upper_key },
			DocumentVerificationProgress::Scanning { after_key },
		) => {
			if after_key.is_some_and(|after| after >= upper_key) {
				Err(DocumentVerificationError::CursorOutsideExtent)
			} else {
				Ok(())
			}
		}
		(
			DocumentVerificationExtent::Through { upper_key },
			DocumentVerificationProgress::Verified { last_key },
		) => {
			if last_key.is_some_and(|last| last > upper_key) {
				Err(DocumentVerificationError::CursorOutsideExtent)
			} else {
				Ok(())
			}
		}
	}
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum DocumentVerificationError {
	#[error("document verification checkpoint {index} repeats a scope")]
	DuplicateScope { index: usize },
	#[error("queued work cannot contain document verification")]
	QueuedProgress,
	#[error("completed work cannot contain document verification")]
	CompletedProgress,
	#[error("empty document verification range must be verified without a last key")]
	InvalidEmptyProgress,
	#[error("document verification cursor is outside its captured range")]
	CursorOutsideExtent,
	#[error("document verification requires scanned selected-writer conversion")]
	ConversionNotScanned,
	#[error("document verification requires building work")]
	BuildingRequired,
	#[error("document verification range is already captured")]
	AlreadyStarted,
	#[error("document verification range is not captured")]
	NotStarted,
	#[error("document verification range is already verified")]
	AlreadyVerified,
	#[error("document verification invariant differs from current scope or stage")]
	CurrentInvariantMismatch,
	#[error("document verification cursor must advance strictly bytewise")]
	CursorNotAdvanced,
	#[error("document verification terminal cursor cannot rewind or discard a prior key")]
	CursorRewind,
	#[error("document readiness requires completed document verification")]
	ReadinessNotVerified,
}
