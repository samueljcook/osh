//! Actual whole-attempt restoration releases all data locks before native reacquisition.
use super::super::super::{
	DatabaseOperationKind, WriteTx,
	attempt_boundary::{GroupAttemptBoundaryPhase, GroupAttemptError, execute_boundary},
	owned_portable_query_as,
};
use super::super::{
	Observation, RawDeleteStorageError, RecordDeletionStep, acquire, cmds,
	records::{HeaderRow, RawDeleteRecords},
};
use super::collection::{self, CollectionFields};
use super::error::{RawNamespaceRejectionError, RawNamespaceRetryReasonError};
use super::{RawNamespaceDeletionFields, RawNamespaceStorageError};
use crate::kv::raw::model::delete::RecordDeleteContention;
use crate::kv::raw::model::namespace::{
	RecordSelectionDeleteResult, RecordSelectionDeleteResultFields,
};
use crate::records::RecordIdentity;
use std::collections::BTreeSet;
const ACQUIRE_NATIVE_LABEL: &str = "db.state_storage.raw_namespace.acquire_native";
const ACQUIRE_SOURCE_LABEL: &str = "db.state_storage.raw_namespace.acquire_source";
pub(super) async fn delete(
	fields: RawNamespaceDeletionFields<'_, '_>,
) -> Result<RecordSelectionDeleteResult, RawNamespaceStorageError> {
	let selection = fields.physical_selection();
	let mut known = collection::collect(
		fields.tx,
		CollectionFields {
			layout: fields.layout,
			namespace: fields.namespace,
			selection,
		},
	)
	.await?;
	if known.is_empty() {
		return Ok(RecordSelectionDeleteResult::new(
			RecordSelectionDeleteResultFields {
				deleted_count: 0,
				completed_attempt_count: 0,
			},
		));
	}
	let width = usize::try_from(fields.request.batch_size().get())
		.map_err(|_| RawNamespaceStorageError::CountUnrepresentable)?;
	let mut attempts = 0u32;
	loop {
		// Every ordinal is prepared before reservation insertion can write.
		let mut records = RawDeleteRecords::new(fields.namespace.clone());
		for key in &known {
			records.push(key.clone())?;
		}
		u64::try_from(records.candidates.len())
			.map_err(|_| RawNamespaceStorageError::CountUnrepresentable)?;
		let mut texts = Vec::new();
		let mut ranks = Vec::new();
		let mut expected = BTreeSet::new();
		for (index, candidate) in records.candidates.iter().enumerate() {
			texts.push(
				std::str::from_utf8(candidate.key.as_bytes())
					.map_err(|e| records.identify(e.into(), index, RecordDeletionStep::Source))?
					.to_owned(),
			);
			ranks.push(
				i64::try_from(index).map_err(|_| RawNamespaceStorageError::CountUnrepresentable)?,
			);
			expected.insert(index);
		}
		attempts = attempts
			.checked_add(1)
			.ok_or(RawNamespaceStorageError::CountUnrepresentable)?;
		let attempt = NamespaceAttempt::begin(fields.tx).await?;
		let keys = records
			.candidates
			.iter()
			.map(|c| c.key.as_bytes().to_vec())
			.collect::<Vec<_>>();
		let sql = acquire::native_query(acquire::QueryFields {
			table: fields
				.layout
				.native_table()
				.ok_or(RawDeleteStorageError::Shape)?,
			contention: RecordDeleteContention::Wait,
		});
		attempt.tx.record_database_operation(
			DatabaseOperationKind::FetchAll,
			ACQUIRE_NATIVE_LABEL,
			Some(sql.as_str()),
		);
		let rows = owned_portable_query_as::<HeaderRow>(sql)
			.bind_validated(fields.namespace.as_bytes())
			.bind_validated(keys)
			.fetch_all(attempt.tx.inner.as_mut())
			.await
			.map_err(|e| {
				RawDeleteStorageError::from(e.with_operation_label(ACQUIRE_NATIVE_LABEL))
			})?;
		acquire::receive_native(&mut records, fields.layout, rows)?;
		if let Some(candidate) = records.candidates.iter().find(|c| !c.native.is_held()) {
			let reason = RawNamespaceRetryReasonError::NativeRacedAbsent {
				identity: RecordIdentity::new(fields.namespace.clone(), candidate.key.clone()),
			};
			restore(
				attempt,
				RetryFields {
					reason,
					attempts,
					budget: fields.request.whole_set_attempts().get(),
				},
			)
			.await?;
			continue;
		}
		let sql = acquire::source_query_builder(acquire::QueryFields {
			table: fields
				.layout
				.source_table()
				.ok_or(RawDeleteStorageError::Shape)?,
			contention: RecordDeleteContention::Wait,
		})
		.finish();
		attempt.tx.record_database_operation(
			DatabaseOperationKind::FetchAll,
			ACQUIRE_SOURCE_LABEL,
			Some(sql.as_str()),
		);
		let rows = owned_portable_query_as::<HeaderRow>(sql)
			.bind_validated(texts)
			.bind_validated(ranks)
			.fetch_all(attempt.tx.inner.as_mut())
			.await
			.map_err(|e| {
				RawDeleteStorageError::from(e.with_operation_label(ACQUIRE_SOURCE_LABEL))
			})?;
		acquire::receive_source(
			&mut records,
			fields.layout,
			&expected,
			rows,
			Observation::UntimedAllValued,
		)?;
		if let Some(candidate) = records
			.candidates
			.iter()
			.find(|c| c.source.is_present() && !c.source.is_held())
		{
			let reason = RawNamespaceRetryReasonError::SourceUnheld {
				identity: RecordIdentity::new(fields.namespace.clone(), candidate.key.clone()),
			};
			restore(
				attempt,
				RetryFields {
					reason,
					attempts,
					budget: fields.request.whole_set_attempts().get(),
				},
			)
			.await?;
			continue;
		}
		let mut prepared = Vec::new();
		let mut deleted = 0u64;
		for start in (0..records.candidates.len()).step_by(width) {
			let end = start.saturating_add(width).min(records.candidates.len());
			let mut native = BTreeSet::new();
			let mut source = BTreeSet::new();
			let mut cleanup = BTreeSet::new();
			for index in start..end {
				let candidate = &records.candidates[index];
				match (candidate.native.value(), candidate.source.value()) {
					(Some(_), Some(_)) => {
						return Err(records
							.identify(
								RawDeleteStorageError::DuplicatePhysicalValues,
								index,
								RecordDeletionStep::Source,
							)
							.into());
					}
					(Some(_), None) => {
						native.insert(index);
					}
					(None, Some(_)) => {
						source.insert(index);
					}
					(None, None) => {}
				}
				if candidate.native.is_inserted_reservation() {
					cleanup.insert(index);
				}
			}
			let count = native
				.len()
				.checked_add(source.len())
				.ok_or(RawNamespaceStorageError::CountUnrepresentable)?;
			deleted = deleted
				.checked_add(
					u64::try_from(count)
						.map_err(|_| RawNamespaceStorageError::CountUnrepresentable)?,
				)
				.ok_or(RawNamespaceStorageError::CountUnrepresentable)?;
			prepared.push(cmds::prepare(cmds::CmdFields {
				records: &records,
				layout: fields.layout,
				native,
				source,
				cleanup,
			})?);
		}
		for cmd in prepared {
			cmds::apply(attempt.tx, &records, cmd).await?;
		}
		let residual = collection::collect(
			attempt.tx,
			CollectionFields {
				layout: fields.layout,
				namespace: fields.namespace,
				selection,
			},
		)
		.await?;
		if residual.is_empty() {
			attempt.release().await?;
			return Ok(RecordSelectionDeleteResult::new(
				RecordSelectionDeleteResultFields {
					deleted_count: deleted,
					completed_attempt_count: u64::from(attempts),
				},
			));
		}
		if let Some(key) = residual
			.iter()
			.find(|key| records.positions.contains_key(*key))
		{
			return Err(RawNamespaceStorageError::CoveredIdentityReappeared {
				identity: RecordIdentity::new(fields.namespace.clone(), key.clone()),
			});
		}
		let additional_count = residual.difference(&known).count();
		let reason = RawNamespaceRetryReasonError::Expanded { additional_count };
		restore(
			attempt,
			RetryFields {
				reason,
				attempts,
				budget: fields.request.whole_set_attempts().get(),
			},
		)
		.await?;
		known.extend(residual);
	}
}
struct RetryFields {
	reason: RawNamespaceRetryReasonError,
	attempts: u32,
	budget: u32,
}
async fn restore(
	attempt: NamespaceAttempt<'_, '_>,
	fields: RetryFields,
) -> Result<(), RawNamespaceStorageError> {
	let rejection = (fields.attempts >= fields.budget).then_some(RawNamespaceRejectionError {
		source: fields.reason,
	});
	let restored = attempt.restore().await;
	match (rejection, restored) {
		(Some(rejection), Err(cleanup)) => Err(RawNamespaceStorageError::RejectionCleanup {
			rejection_error: Box::new(rejection),
			cleanup_error: Box::new(cleanup),
		}),
		(Some(rejection), Ok(())) => Err(RawNamespaceStorageError::Rejected(rejection)),
		(None, Err(source)) => Err(source.into()),
		(None, Ok(())) => Ok(()),
	}
}

/// The outer namespace mutation token requires rollback if this attempt does not end.
struct NamespaceAttempt<'a, 'tx> {
	tx: &'a mut WriteTx<'tx>,
}
impl<'a, 'tx> NamespaceAttempt<'a, 'tx> {
	async fn begin(tx: &'a mut WriteTx<'tx>) -> Result<Self, GroupAttemptError> {
		execute_boundary(tx, GroupAttemptBoundaryPhase::Create).await?;
		Ok(Self { tx })
	}
	async fn release(self) -> Result<(), GroupAttemptError> {
		execute_boundary(self.tx, GroupAttemptBoundaryPhase::Release).await
	}
	async fn restore(self) -> Result<(), GroupAttemptError> {
		execute_boundary(self.tx, GroupAttemptBoundaryPhase::RestoreAndRelease).await
	}
}
