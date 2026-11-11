use super::queries::{Queries, SourceCounterAdvance};
use crate::db::bytea::Bytea;
use crate::db::{
	DatabaseOperationKind, DatabaseOperationObserver, owned_portable_query,
	owned_portable_query_as, record_database_operation,
};
use crate::fleet::lease::state::LeaseAcquisition;
use crate::fleet::lease::{
	Claim, ClaimDuration, ClaimError, FencingToken, HolderId, LEASE_ACQUIRE_LABEL,
	LEASE_RELEASE_LABEL, LEASE_RELEASE_UNDELIVERED_LABEL, LEASE_RENEW_LABEL, LeaseKey,
	OperationError, PersistedLeaseError, Token,
};
use sqlx::{Executor, Postgres};

impl Queries {
	pub(super) async fn try_acquire_lease_with_executor<'e, E>(
		&self,
		executor: E,
		database_operation_observer: Option<&DatabaseOperationObserver>,
		advance: SourceCounterAdvance,
		key: &LeaseKey,
		acquisition: &LeaseAcquisition,
	) -> Result<Option<Claim>, ClaimError>
	where
		E: Executor<'e, Database = Postgres>,
	{
		let duration_micros = acquisition.duration.positive_micros();
		let query = self.acquire_lease(advance);
		record_database_operation(
			database_operation_observer,
			DatabaseOperationKind::FetchOptional,
			LEASE_ACQUIRE_LABEL,
			Some(query),
		);
		let row = owned_portable_query_as::<(String, i64, Bytea, i64)>(sqlx::AssertSqlSafe(query))
			.bind_validated(key.as_str())
			.bind_validated(acquisition.holder_id.as_str())
			.bind_validated(acquisition.lease_token.as_bytes())
			.bind_validated(duration_micros)
			.fetch_optional(executor)
			.await
			.map_err(|error| error.with_operation_label(LEASE_ACQUIRE_LABEL))?;

		Ok(row
			.map(
				|(persisted_holder_id, fencing_token, persisted_lease_token, expires_at)| {
					parse_lease_claim_from_persisted_parts(
						key,
						persisted_holder_id,
						fencing_token,
						persisted_lease_token.0,
						expires_at,
					)
				},
			)
			.transpose()?)
	}

	pub(super) async fn try_renew_lease_with_executor<'e, E>(
		&self,
		executor: E,
		database_operation_observer: Option<&DatabaseOperationObserver>,
		claim: &Claim,
		next_lease_token: &Token,
		duration: ClaimDuration,
	) -> Result<Option<Claim>, ClaimError>
	where
		E: Executor<'e, Database = Postgres>,
	{
		let duration_micros = duration.positive_micros();
		record_database_operation(
			database_operation_observer,
			DatabaseOperationKind::FetchOptional,
			LEASE_RENEW_LABEL,
			Some(self.renew_lease.as_str()),
		);
		let row = owned_portable_query_as::<(String, i64, Bytea, i64)>(sqlx::AssertSqlSafe(
			self.renew_lease.as_str(),
		))
		.bind_validated(claim.key.as_str())
		.bind_validated(claim.holder_id.as_str())
		.bind_validated(claim.fencing_token.as_i64())
		.bind_validated(claim.lease_token.as_bytes())
		.bind_validated(next_lease_token.as_bytes())
		.bind_validated(duration_micros)
		.fetch_optional(executor)
		.await
		.map_err(|error| error.with_operation_label(LEASE_RENEW_LABEL))?;

		Ok(row
			.map(
				|(persisted_holder_id, fencing_token, persisted_lease_token, expires_at)| {
					parse_lease_claim_from_persisted_parts(
						&claim.key,
						persisted_holder_id,
						fencing_token,
						persisted_lease_token.0,
						expires_at,
					)
				},
			)
			.transpose()?)
	}

	pub(super) async fn release_lease_with_executor<'e, E>(
		&self,
		executor: E,
		database_operation_observer: Option<&DatabaseOperationObserver>,
		claim: &Claim,
	) -> Result<bool, OperationError>
	where
		E: Executor<'e, Database = Postgres>,
	{
		record_database_operation(
			database_operation_observer,
			DatabaseOperationKind::Execute,
			LEASE_RELEASE_LABEL,
			Some(self.release_lease.as_str()),
		);
		let rows_updated = owned_portable_query(sqlx::AssertSqlSafe(self.release_lease.as_str()))
			.bind_validated(claim.key.as_str())
			.bind_validated(claim.holder_id.as_str())
			.bind_validated(claim.fencing_token.as_i64())
			.bind_validated(claim.lease_token.as_bytes())
			.execute(executor)
			.await
			.map_err(|error| error.with_operation_label(LEASE_RELEASE_LABEL))?
			.rows_affected();

		Ok(rows_updated != 0)
	}

	pub(super) async fn release_cleanup_targets_with_executor<'e, E>(
		&self,
		executor: E,
		database_operation_observer: Option<&DatabaseOperationObserver>,
		target: &crate::fleet::lease::cleanup_target::LeaseCleanupTarget,
	) -> Result<bool, OperationError>
	where
		E: Executor<'e, Database = Postgres>,
	{
		record_database_operation(
			database_operation_observer,
			DatabaseOperationKind::Execute,
			LEASE_RELEASE_UNDELIVERED_LABEL,
			Some(self.release_cleanup_targets.as_str()),
		);
		let affected =
			owned_portable_query(sqlx::AssertSqlSafe(self.release_cleanup_targets.as_str()))
				.bind_validated(target.key().as_str())
				.bind_validated(target.holder_id().as_str())
				.bind_validated(target.fencing_token().map(FencingToken::as_i64))
				.bind_validated(target.first_token().as_bytes())
				.bind_validated(target.second_token().map(Token::as_bytes))
				.execute(executor)
				.await
				.map_err(|error| error.with_operation_label(LEASE_RELEASE_UNDELIVERED_LABEL))?
				.rows_affected();
		Ok(affected != 0)
	}
}

fn parse_lease_claim_from_persisted_parts(
	key: &LeaseKey,
	holder_id: String,
	fencing_token: i64,
	lease_token: Vec<u8>,
	expires_at_unix_micros: i64,
) -> Result<Claim, PersistedLeaseError> {
	Ok(Claim {
		key: key.clone(),
		holder_id: HolderId::new(holder_id).map_err(PersistedLeaseError::HolderId)?,
		fencing_token: FencingToken::from_i64(fencing_token)?,
		lease_token: Token::from_persisted_bytes(lease_token)?,
		expires_at_unix_micros,
	})
}
