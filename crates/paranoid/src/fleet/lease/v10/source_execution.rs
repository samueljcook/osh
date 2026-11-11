//! Selected SQL lease transitions under source-writer coexistence admission.
//!
//! Source row authority and a fresh coherent observation precede mutation. Native target
//! codecs never describe these SQL writes, and a provisional first fence commits only with
//! its actual private grant. The caller owns transaction completion and grant delivery.

use super::queries::SourceCounterAdvance;
use super::source_store::PgLeaseSourceStorage;
use crate::db::{
	DatabaseError, DatabaseOperationKind, TransactionFailure, WriteTx, owned_portable_query_as,
	owned_portable_query_scalar,
};
use crate::fleet::lease::cleanup_target::LeaseCleanupTarget;
use crate::fleet::lease::state::{LeaseAcquisition, TentativeLeaseAuthority};
use crate::fleet::lease::{Claim, ClaimDuration, ClaimError, LeaseKey, OperationError, Token};
use crate::records::postgres::StateMutationCompletion;

const LOCK_CLAIM_LABEL: &str = "lease.source.coexistence.lock_claim";
const LOCK_COUNTER_LABEL: &str = "lease.source.coexistence.lock_counter";
const SEED_FIRST_FENCE_LABEL: &str = "lease.source.coexistence.seed_first_fence";
const OBSERVE_INTEGRITY_LABEL: &str = "lease.source.coexistence.observe_integrity";

/// Integrity observations retain the logical key without copying private claim material.
#[derive(Clone, Copy, Debug)]
pub(crate) enum LeaseSourceIntegrity {
	NativePlacement,
	MissingCounter,
	CounterBelowClaim,
	NonpositiveFence,
	CounterAuthorityAbsent,
	FirstFenceWithoutGrant,
	UnsupportedIsolation,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum LeaseSourceExecutionError {
	#[error("source lease authority belongs to a different fixed handle key")]
	KeyMismatch,
	#[error("source lease integrity failed for {key:?}: {failure:?}")]
	Integrity {
		key: LeaseKey,
		failure: LeaseSourceIntegrity,
	},
	#[error("source lease lock or first-fence statement failed")]
	Database(#[from] DatabaseError),
	#[error("source lease grant statement or parsing failed")]
	Grant(#[source] ClaimError),
	#[error("source lease release statement failed")]
	Release(#[source] OperationError),
}

impl LeaseSourceExecutionError {
	pub(in crate::fleet::lease) fn involves_database_failure(&self) -> bool {
		match self {
			Self::Database(_) => true,
			Self::Grant(error) => error.involves_database_failure(),
			Self::Release(error) => error.involves_database_failure(),
			Self::KeyMismatch | Self::Integrity { .. } => false,
		}
	}
	pub(in crate::fleet::lease) fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Database(error) => error.commit_outcome_is_unknown(),
			Self::Grant(error) => error.commit_outcome_is_unknown(),
			Self::Release(error) => error.commit_outcome_is_unknown(),
			Self::KeyMismatch | Self::Integrity { .. } => false,
		}
	}
}

/// Actual executable source SQL implementation; declaration binding selects its use.
pub(crate) struct LeaseSourceSqlKernel;

impl LeaseSourceSqlKernel {
	pub(super) fn new() -> Self {
		Self
	}

	pub(in crate::fleet::lease) async fn acquire(
		&self,
		storage: &PgLeaseSourceStorage,
		tx: &mut WriteTx<'_>,
		key: &LeaseKey,
		acquisition: LeaseAcquisition,
	) -> Result<Option<TentativeLeaseAuthority>, LeaseSourceExecutionError> {
		let mut guard = LeaseSourceMutationGuard::lock(storage, tx, key, Mutation::Acquire).await?;
		let advance = match guard.authority {
			SourceMutationAuthority::Acquisition(advance) => advance,
			SourceMutationAuthority::Contended => {
				guard.completion.complete();
				return Ok(None);
			}
			SourceMutationAuthority::ExistingOperation => {
				unreachable!("an acquisition retains its counter authority")
			}
		};
		let observer = guard.tx.database_operation_observer().cloned();
		let grant = storage
			.queries()
			.try_acquire_lease_with_executor(
				guard.tx.inner.as_mut(),
				observer.as_ref(),
				advance,
				key,
				&acquisition,
			)
			.await
			.map_err(LeaseSourceExecutionError::Grant)?;
		if matches!(advance, SourceCounterAdvance::OwnFirstFence) && grant.is_none() {
			return Err(integrity(key, LeaseSourceIntegrity::FirstFenceWithoutGrant));
		}
		guard.completion.complete();
		Ok(grant.map(TentativeLeaseAuthority::from_source_sql_grant))
	}

	pub(in crate::fleet::lease) async fn renew(
		&self,
		storage: &PgLeaseSourceStorage,
		tx: &mut WriteTx<'_>,
		key: &LeaseKey,
		authority: &Claim,
		next_token: Token,
		duration: ClaimDuration,
	) -> Result<Option<TentativeLeaseAuthority>, LeaseSourceExecutionError> {
		if key != authority.key() {
			return Err(LeaseSourceExecutionError::KeyMismatch);
		}
		let mut guard =
			LeaseSourceMutationGuard::lock(storage, tx, key, Mutation::Existing).await?;
		let observer = guard.tx.database_operation_observer().cloned();
		let grant = storage
			.queries()
			.try_renew_lease_with_executor(
				guard.tx.inner.as_mut(),
				observer.as_ref(),
				authority,
				&next_token,
				duration,
			)
			.await
			.map_err(LeaseSourceExecutionError::Grant)?;
		guard.completion.complete();
		Ok(grant.map(TentativeLeaseAuthority::from_source_sql_grant))
	}

	pub(in crate::fleet::lease) async fn release(
		&self,
		storage: &PgLeaseSourceStorage,
		tx: &mut WriteTx<'_>,
		key: &LeaseKey,
		authority: &Claim,
	) -> Result<bool, LeaseSourceExecutionError> {
		self.release_authorized(storage, tx, key, SourceReleaseAuthority::Claim(authority))
			.await
	}

	pub(in crate::fleet::lease) async fn release_cleanup(
		&self,
		storage: &PgLeaseSourceStorage,
		tx: &mut WriteTx<'_>,
		key: &LeaseKey,
		target: &LeaseCleanupTarget,
	) -> Result<bool, LeaseSourceExecutionError> {
		self.release_authorized(storage, tx, key, SourceReleaseAuthority::Cleanup(target))
			.await
	}

	async fn release_authorized(
		&self,
		storage: &PgLeaseSourceStorage,
		tx: &mut WriteTx<'_>,
		key: &LeaseKey,
		authority: SourceReleaseAuthority<'_>,
	) -> Result<bool, LeaseSourceExecutionError> {
		let authority_key = match authority {
			SourceReleaseAuthority::Claim(claim) => claim.key(),
			SourceReleaseAuthority::Cleanup(target) => target.key(),
		};
		if key != authority_key {
			return Err(LeaseSourceExecutionError::KeyMismatch);
		}
		let mut guard =
			LeaseSourceMutationGuard::lock(storage, tx, key, Mutation::Existing).await?;
		let queries = storage.queries();
		let observer = guard.tx.database_operation_observer().cloned();
		let released = match authority {
			SourceReleaseAuthority::Claim(claim) => {
				queries
					.release_lease_with_executor(guard.tx.inner.as_mut(), observer.as_ref(), claim)
					.await
			}
			SourceReleaseAuthority::Cleanup(target) => {
				queries
					.release_cleanup_targets_with_executor(
						guard.tx.inner.as_mut(),
						observer.as_ref(),
						target,
					)
					.await
			}
		}
		.map_err(LeaseSourceExecutionError::Release)?;
		guard.completion.complete();
		Ok(released)
	}
}

enum SourceReleaseAuthority<'a> {
	Claim(&'a Claim),
	Cleanup(&'a LeaseCleanupTarget),
}

#[derive(Clone, Copy)]
enum Mutation {
	Acquire,
	Existing,
}

/// All waits and parsing remain unfinished until the operation's whole SQL result succeeds.
struct LeaseSourceMutationGuard<'a, 'tx> {
	tx: &'a mut WriteTx<'tx>,
	authority: SourceMutationAuthority,
	completion: StateMutationCompletion,
}

enum SourceMutationAuthority {
	ExistingOperation,
	Contended,
	Acquisition(SourceCounterAdvance),
}

enum ClaimLock {
	Unheld,
	Held,
}

enum CounterAuthority {
	NotRequested,
	FirstFence,
	ExistingLocked,
	Contended,
}

impl CounterAuthority {
	fn is_first_fence(&self) -> bool {
		matches!(self, Self::FirstFence)
	}
	fn is_held(&self) -> bool {
		matches!(self, Self::FirstFence | Self::ExistingLocked)
	}
}

impl<'a, 'tx> LeaseSourceMutationGuard<'a, 'tx> {
	async fn lock(
		storage: &PgLeaseSourceStorage,
		tx: &'a mut WriteTx<'tx>,
		key: &LeaseKey,
		mutation: Mutation,
	) -> Result<Self, LeaseSourceExecutionError> {
		let completion = tx.track_state_mutation();
		let claim_query = crate::audited_sql!(
			"SELECT key FROM ONLY {table} WHERE key OPERATOR(pg_catalog.=) $1 FOR UPDATE",
			table = storage.claim_table()
		);
		tx.record_database_operation(
			DatabaseOperationKind::FetchOptional,
			LOCK_CLAIM_LABEL,
			Some(claim_query.as_str()),
		);
		let initial_claim = owned_portable_query_scalar::<String>(claim_query)
			.bind_validated(key.as_str())
			.fetch_optional(tx.inner.as_mut())
			.await
			.map_err(|error| error.with_operation_label(LOCK_CLAIM_LABEL))?;
		let mut claim_lock = match &initial_claim {
			Some(_) => ClaimLock::Held,
			None => ClaimLock::Unheld,
		};
		let mut counter = CounterAuthority::NotRequested;
		if matches!(mutation, Mutation::Acquire) && initial_claim.is_none() {
			let seed = crate::audited_sql!(
				"INSERT INTO {counter} (key, last_fencing_token, updated_at) SELECT $1, 1, pg_catalog.clock_timestamp() WHERE NOT EXISTS (SELECT 1 FROM ONLY {claim} WHERE key OPERATOR(pg_catalog.=) $1) ON CONFLICT (key) DO NOTHING RETURNING key",
				counter = storage.fencing_counter_table(),
				claim = storage.claim_table()
			);
			tx.record_database_operation(
				DatabaseOperationKind::FetchOptional,
				SEED_FIRST_FENCE_LABEL,
				Some(seed.as_str()),
			);
			counter = match owned_portable_query_scalar::<String>(seed)
				.bind_validated(key.as_str())
				.fetch_optional(tx.inner.as_mut())
				.await
				.map_err(|error| error.with_operation_label(SEED_FIRST_FENCE_LABEL))?
			{
				Some(_) => CounterAuthority::FirstFence,
				None => CounterAuthority::NotRequested,
			};
		}
		// Pre10 acquisitions can lock either row first. Never wait for a counter while
		// holding a claim. Renewal and release change only the claim and need no counter lock.
		if matches!(mutation, Mutation::Acquire) && !counter.is_first_fence() {
			let counter_lock = crate::audited_sql!(
				"SELECT key FROM ONLY {table} WHERE key OPERATOR(pg_catalog.=) $1 FOR UPDATE SKIP LOCKED",
				table = storage.fencing_counter_table()
			);
			tx.record_database_operation(
				DatabaseOperationKind::FetchOptional,
				LOCK_COUNTER_LABEL,
				Some(counter_lock.as_str()),
			);
			counter = match owned_portable_query_scalar::<String>(counter_lock)
				.bind_validated(key.as_str())
				.fetch_optional(tx.inner.as_mut())
				.await
				.map_err(|error| error.with_operation_label(LOCK_COUNTER_LABEL))?
			{
				Some(_) => CounterAuthority::ExistingLocked,
				None => CounterAuthority::Contended,
			};
		}
		// A claim can become visible after counter acquisition. Another acquisition may hold
		// that claim lock while waiting for our counter, so this claim lock must not wait.
		if matches!(mutation, Mutation::Acquire) && initial_claim.is_none() {
			let claim_query = crate::audited_sql!(
				"SELECT key FROM ONLY {table} WHERE key OPERATOR(pg_catalog.=) $1 FOR UPDATE SKIP LOCKED",
				table = storage.claim_table()
			);
			tx.record_database_operation(
				DatabaseOperationKind::FetchOptional,
				LOCK_CLAIM_LABEL,
				Some(claim_query.as_str()),
			);
			claim_lock = match owned_portable_query_scalar::<String>(claim_query)
				.bind_validated(key.as_str())
				.fetch_optional(tx.inner.as_mut())
				.await
				.map_err(|error| error.with_operation_label(LOCK_CLAIM_LABEL))?
			{
				Some(_) => ClaimLock::Held,
				None => ClaimLock::Unheld,
			};
		}
		// One fresh snapshot observes only placement and history. Source timestamps and
		// private tokens are neither fetched nor passed through a native representation.
		let observation = crate::audited_sql!(
			"SELECT n.body IS NOT NULL AS native_value, l.key IS NOT NULL AS claim_exists, l.fencing_token, c.key IS NOT NULL AS counter_exists, c.last_fencing_token, pg_catalog.current_setting('transaction_isolation') FROM (SELECT 1) observation LEFT JOIN ONLY {native} n ON n.namespace OPERATOR(pg_catalog.=) $1 AND n.key OPERATOR(pg_catalog.=) $2 LEFT JOIN ONLY {claim} l ON l.key OPERATOR(pg_catalog.=) $3 LEFT JOIN ONLY {counter} c ON c.key OPERATOR(pg_catalog.=) $3",
			native = storage.native_storage().table(),
			claim = storage.claim_table(),
			counter = storage.fencing_counter_table()
		);
		tx.record_database_operation(
			DatabaseOperationKind::FetchOne,
			OBSERVE_INTEGRITY_LABEL,
			Some(observation.as_str()),
		);
		let (native_value, claim_exists, claim_fence, counter_exists, counter_fence, isolation) =
			owned_portable_query_as::<(bool, bool, Option<i64>, bool, Option<i64>, String)>(
				observation,
			)
			.bind_validated(storage.namespace().as_bytes())
			.bind_validated(key.as_str().as_bytes())
			.bind_validated(key.as_str())
			.fetch_one(tx.inner.as_mut())
			.await
			.map_err(|error| error.with_operation_label(OBSERVE_INTEGRITY_LABEL))?;
		if isolation != "read committed" {
			return Err(integrity(key, LeaseSourceIntegrity::UnsupportedIsolation));
		}
		if native_value {
			return Err(integrity(key, LeaseSourceIntegrity::NativePlacement));
		}
		if claim_exists && (!counter_exists || counter.is_first_fence()) {
			return Err(integrity(key, LeaseSourceIntegrity::MissingCounter));
		}
		if claim_exists && claim_fence.is_none_or(|fence| fence <= 0)
			|| counter_exists && counter_fence.is_none_or(|fence| fence <= 0)
		{
			return Err(integrity(key, LeaseSourceIntegrity::NonpositiveFence));
		}
		if claim_exists && counter_fence < claim_fence {
			return Err(integrity(key, LeaseSourceIntegrity::CounterBelowClaim));
		}
		if matches!(mutation, Mutation::Acquire) && !counter_exists {
			return Err(integrity(key, LeaseSourceIntegrity::CounterAuthorityAbsent));
		}
		Ok(Self {
			tx,
			authority: match mutation {
				Mutation::Existing => SourceMutationAuthority::ExistingOperation,
				Mutation::Acquire
					if !counter.is_held()
						|| (claim_exists && matches!(claim_lock, ClaimLock::Unheld)) =>
				{
					SourceMutationAuthority::Contended
				}
				Mutation::Acquire => {
					SourceMutationAuthority::Acquisition(if counter.is_first_fence() {
						SourceCounterAdvance::OwnFirstFence
					} else {
						SourceCounterAdvance::Existing
					})
				}
			},
			completion,
		})
	}
}

fn integrity(key: &LeaseKey, failure: LeaseSourceIntegrity) -> LeaseSourceExecutionError {
	LeaseSourceExecutionError::Integrity {
		key: key.clone(),
		failure,
	}
}
