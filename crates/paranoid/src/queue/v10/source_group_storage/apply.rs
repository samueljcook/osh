//! Exact native/source plan preparation and protected source deletion.
//!
//! Pure plans provide no admission or lock proof. The consuming source observation owner
//! retains the actual descriptor, native set, and transaction through all application phases.

use super::{
	BTreeMap, BTreeSet, DatabaseError, DatabaseOperationKind, PgQueueSourceGroupStorage,
	PgV10SourceDescriptor, PgV10SourceRole, QueueGroupError, QueuePauseScope, QueueSourceGroupRead,
	RecordGroupRead, RecordIdentity, RecordKey, SourceRecord, WriteTx, owned_portable_query,
	paused_task_key, request, row_ctx,
};
use crate::db::bytea::Bytea;
use crate::db::{AuditedSql, PgQualifiedTableName, PortableQuery};
use crate::queue::group_state::{
	PreparedQueueGroupOutcome, PreparedQueueGroupPlan, QueueSourceRole,
};
use crate::records::postgres::StateStorageError;
use crate::records::postgres::group::PreparedPgRecordGroupMutation;
use crate::records::postgres::group_attempt::GroupAttemptError;
use crate::records::{InvalidGroupMutationError, RecordMutation};

/// An actual fully encoded Queue planner result, never an admission or writer-selection proof.
/// The owning operator must prepare it through this transaction's actual BoundQueueGroup.
pub(crate) struct QueueSourceGroupApplyPlan {
	plan: PreparedQueueGroupPlan,
}
impl QueueSourceGroupApplyPlan {
	pub(in crate::queue) fn from_prepared(plan: PreparedQueueGroupPlan) -> Self {
		Self { plan }
	}
}

/// The owning planner retains the exact failed record and its actual typed preparation cause.
pub(crate) struct QueueSourcePreparationRejection {
	identity: RecordIdentity,
	error: QueueGroupError,
}
impl QueueSourcePreparationRejection {
	pub(crate) fn into_rejection(self) -> QueueSourceApplyRejectionError {
		QueueSourceApplyRejectionError::Preparation {
			identity: self.identity,
			error: self.error,
		}
	}
	pub(in crate::queue) fn new(identity: RecordIdentity, error: QueueGroupError) -> Self {
		Self { identity, error }
	}
}

/// The diagnostic result grants no worker/private claim or known commit.
pub(crate) struct QueueSourceGroupApplyResult {
	outcome: PreparedQueueGroupOutcome,
}
impl QueueSourceGroupApplyResult {
	pub(in crate::queue) fn into_outcome(self) -> PreparedQueueGroupOutcome {
		self.outcome
	}
}
impl std::fmt::Debug for QueueSourceGroupApplyResult {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		self.outcome.fmt(f)
	}
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum QueueSourceApplyRejectionError {
	#[error("source Queue application requires a completed whole-set observation")]
	ObservationIncomplete,
	#[error("source Queue preparation failed at {identity:?}")]
	Preparation {
		identity: RecordIdentity,
		#[source]
		error: QueueGroupError,
	},
	#[error("source Queue native action plan is invalid")]
	NativePlan(#[from] InvalidGroupMutationError),
	#[error("source Queue plan coverage is invalid at {identity:?}: {reason}")]
	Coverage {
		identity: RecordIdentity,
		reason: SourceApplyCoverageReasonError,
	},
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum SourceApplyCoverageReasonError {
	#[error("identity is not held")]
	Unheld,
	#[error("source observation is unknown")]
	Unobserved,
	#[error("source value is absent")]
	Absent,
	#[error("source role, namespace, or body identity differs")]
	RoleMismatch,
	#[error("source deletion occurs more than once")]
	DuplicateDeletion,
	#[error("source deletion has no matching native change")]
	MissingNativeChange,
	#[error("native change has no matching source deletion")]
	MissingSourceDeletion,
	#[error("native change belongs to another Queue placement")]
	ForeignNamespace,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum QueueSourceGroupApplyError {
	#[error("source Queue native application failed")]
	Native(#[from] StateStorageError),
	#[error("source Queue deletion database operation failed")]
	Database(#[from] DatabaseError),
	#[error("source Queue attempt boundary failed")]
	Boundary(#[from] GroupAttemptError),
	#[error("source Queue preparation rejection and attempt restoration both failed")]
	RejectionCleanup {
		rejection_error: Box<QueueSourceApplyRejectionError>,
		#[source]
		cleanup_error: Box<GroupAttemptError>,
	},
	#[error("source Queue deletion returned an unexpected key {key:?} from {table:?}")]
	UnexpectedDeletion {
		table: crate::db::PgQualifiedTableName,
		key: RecordKey,
	},
	#[error("source Queue deletion returned an identity more than once {identity:?}")]
	DuplicateDeletion { identity: RecordIdentity },
	#[error("source Queue deletion did not return an expected identity {identity:?}")]
	MissingDeletion { identity: RecordIdentity },
}

impl QueueSourceGroupApplyError {
	pub(crate) fn database_error(&self) -> Option<&DatabaseError> {
		match self {
			Self::Native(error) => error.database_error(),
			Self::Database(error) => Some(error),
			Self::Boundary(error) => error.database_error(),
			Self::RejectionCleanup { cleanup_error, .. } => cleanup_error.database_error(),
			Self::UnexpectedDeletion { .. }
			| Self::DuplicateDeletion { .. }
			| Self::MissingDeletion { .. } => None,
		}
	}
	pub(crate) fn involves_database_failure(&self) -> bool {
		match self {
			Self::Boundary(error) => error.involves_database_failure(),
			Self::RejectionCleanup { cleanup_error, .. } => {
				cleanup_error.involves_database_failure()
			}
			_ => self.database_error().is_some(),
		}
	}
	pub(crate) fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Boundary(error) => error.commit_outcome_is_unknown(),
			Self::RejectionCleanup { cleanup_error, .. } => {
				cleanup_error.commit_outcome_is_unknown()
			}
			_ => self
				.database_error()
				.is_some_and(DatabaseError::commit_outcome_is_unknown),
		}
	}
}

pub(crate) struct PreparedQueueSourceApply {
	native: PreparedPgRecordGroupMutation,
	cleanup_reservations: BTreeSet<RecordIdentity>,
	source: PreparedQueueSourceDeletes,
}
impl PreparedQueueSourceApply {
	pub(crate) fn into_parts(
		self,
	) -> (
		PreparedPgRecordGroupMutation,
		BTreeSet<RecordIdentity>,
		PreparedQueueSourceDeletes,
	) {
		(self.native, self.cleanup_reservations, self.source)
	}
}
pub(crate) struct PreparedQueueSourceDeletes {
	roles: Vec<PreparedSourceDeleteRole>,
	result: PreparedQueueGroupOutcome,
}
struct PreparedSourceDeleteRole {
	expected: BTreeMap<Vec<u8>, RecordIdentity>,
	table: PgQualifiedTableName,
	statement: AuditedSql,
	query: PortableQuery,
}
struct SourceDeletes {
	pauses: BTreeMap<Vec<u8>, RecordIdentity>,
	jobs: BTreeMap<Vec<u8>, RecordIdentity>,
	dead_letters: BTreeMap<Vec<u8>, RecordIdentity>,
}
fn coverage(
	identity: &RecordIdentity,
	reason: SourceApplyCoverageReasonError,
) -> QueueSourceApplyRejectionError {
	QueueSourceApplyRejectionError::Coverage {
		identity: identity.clone(),
		reason,
	}
}

impl PgQueueSourceGroupStorage {
	/// Derives the entire native projection and exact source plan before any storage mutation.
	pub(crate) fn prepare_apply(
		&self,
		native: &RecordGroupRead,
		observed: &QueueSourceGroupRead,
		inserted_reservations: &BTreeSet<RecordIdentity>,
		plan: QueueSourceGroupApplyPlan,
	) -> Result<PreparedQueueSourceApply, QueueSourceApplyRejectionError> {
		let PreparedQueueGroupPlan {
			native: plan,
			source_deletes,
			result,
		} = plan.plan;
		let actions = plan.into_actions();
		let mut deletes = SourceDeletes {
			pauses: BTreeMap::new(),
			jobs: BTreeMap::new(),
			dead_letters: BTreeMap::new(),
		};
		let mut deleted = BTreeSet::new();
		for intent in source_deletes {
			let identity = &intent.identity;
			if !native.snapshots().contains_key(identity) {
				return Err(coverage(identity, SourceApplyCoverageReasonError::Unheld));
			}
			if !deleted.insert(identity.clone()) {
				return Err(coverage(
					identity,
					SourceApplyCoverageReasonError::DuplicateDeletion,
				));
			}
			if !matches!(
				actions.get(identity),
				Some(RecordMutation::Write(_) | RecordMutation::Delete)
			) {
				return Err(coverage(
					identity,
					SourceApplyCoverageReasonError::MissingNativeChange,
				));
			}
			let source =
				observed.facts.records.get(identity).ok_or_else(|| {
					coverage(identity, SourceApplyCoverageReasonError::Unobserved)
				})?;
			if matches!(source, SourceRecord::Absent) {
				return Err(coverage(identity, SourceApplyCoverageReasonError::Absent));
			}
			match (intent.role, source) {
				(QueueSourceRole::Job, SourceRecord::Job(state))
					if *identity == request::id_identity(self.native.jobs(), state.job().id) =>
				{
					deletes
						.jobs
						.insert(state.job().id.as_bytes().to_vec(), identity.clone());
				}
				(QueueSourceRole::DeadLetter, SourceRecord::DeadLetter(state))
					if *identity
						== request::id_identity(
							self.native.dead_letters(),
							state.dead_letter().id,
						) =>
				{
					deletes
						.dead_letters
						.insert(state.dead_letter().id.as_bytes().to_vec(), identity.clone());
				}
				(QueueSourceRole::Pause, SourceRecord::Pause { state, metadata })
					if identity.namespace() == self.native.pauses()
						&& identity.key() == &state.scope.key()
						&& metadata.expires_at.is_none() =>
				{
					let key = match &state.scope {
						QueuePauseScope::Global => {
							crate::queue::GLOBAL_PAUSE_KEY.as_bytes().to_vec()
						}
						QueuePauseScope::Task(task) => paused_task_key(task).into_bytes(),
					};
					deletes.pauses.insert(key, identity.clone());
				}
				_ => {
					return Err(coverage(
						identity,
						SourceApplyCoverageReasonError::RoleMismatch,
					));
				}
			}
		}
		for (identity, action) in &actions {
			if !native.snapshots().contains_key(identity) {
				return Err(coverage(identity, SourceApplyCoverageReasonError::Unheld));
			}
			if matches!(action, RecordMutation::Keep) {
				continue;
			}
			let namespace = identity.namespace();
			if namespace == self.native.jobs()
				|| namespace == self.native.dead_letters()
				|| namespace == self.native.pauses()
			{
				let source = observed.facts.records.get(identity).ok_or_else(|| {
					coverage(identity, SourceApplyCoverageReasonError::Unobserved)
				})?;
				if !matches!(source, SourceRecord::Absent) && !deleted.contains(identity) {
					return Err(coverage(
						identity,
						SourceApplyCoverageReasonError::MissingSourceDeletion,
					));
				}
			} else if namespace == self.native.active_dedupe() {
				if !observed.facts.active_dedupe.contains_key(identity) {
					return Err(coverage(
						identity,
						SourceApplyCoverageReasonError::Unobserved,
					));
				}
			} else if namespace == self.native.dead_letter_originals() {
				if !observed.facts.originals.contains_key(identity) {
					return Err(coverage(
						identity,
						SourceApplyCoverageReasonError::Unobserved,
					));
				}
			} else {
				return Err(coverage(
					identity,
					SourceApplyCoverageReasonError::ForeignNamespace,
				));
			}
		}
		let native = PreparedPgRecordGroupMutation::prepare(native, actions.into_iter().collect())?;
		let cleanup_reservations = native.inserted_keep_reservations(inserted_reservations);
		let roles = [
			(deletes.pauses, PgV10SourceRole::QueuePauses, true),
			(deletes.jobs, PgV10SourceRole::QueueJobs, false),
			(
				deletes.dead_letters,
				PgV10SourceRole::QueueDeadLetters,
				false,
			),
		]
		.into_iter()
		.filter(|(expected, _, _)| !expected.is_empty())
		.map(|(expected, role, pause)| {
			let statement = delete_query(&self.descriptor, role, pause);
			let keys: Vec<_> = expected.keys().cloned().collect();
			let query = owned_portable_query(statement.clone()).bind_validated(keys);
			PreparedSourceDeleteRole {
				expected,
				table: self.descriptor.table(role).clone(),
				statement,
				query,
			}
		})
		.collect();
		Ok(PreparedQueueSourceApply {
			native,
			cleanup_reservations,
			source: PreparedQueueSourceDeletes { roles, result },
		})
	}

	/// Only the consuming observation owner calls this after all native changes succeed.
	/// The same descriptor and protected source set remain retained; no new locks are acquired.
	pub(crate) async fn apply_source_deletes(
		&self,
		tx: &mut WriteTx<'_>,
		prepared: PreparedQueueSourceDeletes,
	) -> Result<QueueSourceGroupApplyResult, QueueSourceGroupApplyError> {
		for role in prepared.roles {
			let PreparedSourceDeleteRole {
				expected,
				table,
				statement,
				query,
			} = role;
			tx.record_database_operation(
				DatabaseOperationKind::FetchAll,
				"queue.source_group.delete_role",
				Some(statement.as_str()),
			);
			let returned = query
				.fetch_all(tx.inner.as_mut())
				.await
				.map_err(|error| error.with_operation_label("queue.source_group.delete_role"))?;
			let ctx = row_ctx(&statement);
			let mut seen = BTreeSet::new();
			for row in returned {
				let key: Bytea = ctx.column(&row, "deleted_key")?;
				let Some(identity) = expected.get(&key.0) else {
					return Err(QueueSourceGroupApplyError::UnexpectedDeletion {
						table,
						key: RecordKey::new(key.0),
					});
				};
				if !seen.insert(key.0) {
					return Err(QueueSourceGroupApplyError::DuplicateDeletion {
						identity: identity.clone(),
					});
				}
			}
			if let Some((_, identity)) = expected.iter().find(|(key, _)| !seen.contains(*key)) {
				return Err(QueueSourceGroupApplyError::MissingDeletion {
					identity: identity.clone(),
				});
			}
		}
		Ok(QueueSourceGroupApplyResult {
			outcome: prepared.result,
		})
	}
}

fn delete_query(
	descriptor: &PgV10SourceDescriptor,
	role: PgV10SourceRole,
	is_pause_table: bool,
) -> AuditedSql {
	if is_pause_table {
		crate::audited_sql!(
			"DELETE FROM ONLY {table} WHERE key COLLATE pg_catalog.\"C\" OPERATOR(pg_catalog.=) ANY (ARRAY(SELECT pg_catalog.convert_from(key, 'UTF8') COLLATE pg_catalog.\"C\" FROM pg_catalog.unnest($1::pg_catalog.bytea[]) AS r(key))) RETURNING pg_catalog.convert_to(key, 'UTF8') AS deleted_key",
			table = descriptor.table(role)
		)
	} else {
		crate::audited_sql!(
			"DELETE FROM ONLY {table} WHERE id OPERATOR(pg_catalog.=) ANY ($1::pg_catalog.bytea[]) RETURNING id AS deleted_key",
			table = descriptor.table(role)
		)
	}
}
