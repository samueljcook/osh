use super::catalog::{LeaseColumn, expires_at_unix_micros_expression};
use crate::db::PgQualifiedTableName;

/// The coexistence statements of one pair of v10 lease tables, built once with the storage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Queries {
	acquire_after_existing_fence: String,
	acquire_with_own_first_fence: String,
	pub(super) renew_lease: String,
	pub(super) release_lease: String,
	pub(super) release_cleanup_targets: String,
}

/// Physical SQL identities; no protocol or store construction is implied.
pub(super) struct LeaseSqlTables<'a> {
	pub(super) claim_table: &'a PgQualifiedTableName,
	pub(super) fencing_counter_table: &'a PgQualifiedTableName,
}

/// Existing source history and a first fence inserted by this transaction are distinct.
#[derive(Clone, Copy)]
pub(super) enum SourceCounterAdvance {
	Existing,
	OwnFirstFence,
}

impl Queries {
	pub(super) fn for_tables(tables: LeaseSqlTables<'_>) -> Self {
		let quoted_table_name = tables.claim_table.quoted().to_string();
		let quoted_fencing_counter_table_name = tables.fencing_counter_table.quoted().to_string();
		Self {
			acquire_after_existing_fence: build_acquire_lease_query(
				&quoted_table_name,
				&quoted_fencing_counter_table_name,
				SourceCounterAdvance::Existing,
			),
			acquire_with_own_first_fence: build_acquire_lease_query(
				&quoted_table_name,
				&quoted_fencing_counter_table_name,
				SourceCounterAdvance::OwnFirstFence,
			),
			renew_lease: build_renew_lease_query(&quoted_table_name),
			release_lease: build_release_lease_query(&quoted_table_name),
			release_cleanup_targets: build_release_query(&quoted_table_name, ReleaseMatch::Cleanup),
		}
	}

	/// The acquisition statement for the counter row that the acquisition locked.
	pub(super) fn acquire_lease(&self, advance: SourceCounterAdvance) -> &str {
		match advance {
			SourceCounterAdvance::Existing => &self.acquire_after_existing_fence,
			SourceCounterAdvance::OwnFirstFence => &self.acquire_with_own_first_fence,
		}
	}
}

fn build_acquire_lease_query(
	quoted_table_name: &str,
	quoted_fencing_counter_table_name: &str,
	locked_counter: SourceCounterAdvance,
) -> String {
	let key = LeaseColumn::LeaseKey.name();
	let holder_id = LeaseColumn::HolderId.name();
	let fencing_token = LeaseColumn::FencingToken.name();
	let lease_token = LeaseColumn::LeaseToken.name();
	let expires_at = LeaseColumn::ExpiresAt.name();
	let updated_at = LeaseColumn::UpdatedAt.name();
	let last_fencing_token = LeaseColumn::LastFencingToken.name();
	let counter_target_last_fencing_token =
		LeaseColumn::LastFencingToken.qualified("counter_target");
	let excluded_holder_id = LeaseColumn::HolderId.qualified("EXCLUDED");
	let excluded_fencing_token = LeaseColumn::FencingToken.qualified("EXCLUDED");
	let excluded_lease_token = LeaseColumn::LeaseToken.qualified("EXCLUDED");
	let excluded_expires_at = LeaseColumn::ExpiresAt.qualified("EXCLUDED");
	let excluded_updated_at = LeaseColumn::UpdatedAt.qualified("EXCLUDED");
	let lease_target_expires_at = LeaseColumn::ExpiresAt.qualified("lease_target");
	let expires_at_unix_micros = expires_at_unix_micros_expression(None);
	let counter_advance = match locked_counter {
		SourceCounterAdvance::Existing => {
			format!("{counter_target_last_fencing_token} OPERATOR(pg_catalog.+) 1")
		}
		SourceCounterAdvance::OwnFirstFence => counter_target_last_fencing_token,
	};

	format!(
		"WITH existing_lease AS MATERIALIZED (\
             SELECT {fencing_token}, {expires_at} \
             FROM {quoted_table_name} \
             WHERE {key} OPERATOR(pg_catalog.=) $1 \
             FOR UPDATE\
         ), \
         lease_observation AS MATERIALIZED (\
             SELECT \
                 (SELECT {fencing_token} FROM existing_lease) AS existing_fencing_token, \
                 (SELECT {expires_at} FROM existing_lease) AS existing_expires_at, \
                 pg_catalog.clock_timestamp() AS decision_time \
             FROM (SELECT pg_catalog.COUNT(*) FROM existing_lease) AS locked_dependency\
         ), \
         claimable AS MATERIALIZED (\
             SELECT \
                 $1::pg_catalog.text AS {key}, \
                 COALESCE(existing_fencing_token, 0) AS existing_fencing_token \
             FROM lease_observation \
             WHERE existing_expires_at IS NULL \
                OR existing_expires_at OPERATOR(pg_catalog.<=) decision_time\
         ), \
         next_fencing_token AS (\
             INSERT INTO {quoted_fencing_counter_table_name} AS counter_target \
                 ({key}, {last_fencing_token}, {updated_at}) \
             SELECT {key}, existing_fencing_token OPERATOR(pg_catalog.+) 1, pg_catalog.clock_timestamp() \
             FROM claimable \
             ON CONFLICT ({key}) DO UPDATE SET \
                 {last_fencing_token} = {counter_advance}, \
                 {updated_at} = pg_catalog.clock_timestamp() \
             RETURNING {last_fencing_token}\
         ), \
         claim_clock AS MATERIALIZED (\
             SELECT {last_fencing_token}, pg_catalog.clock_timestamp() AS decision_time \
             FROM next_fencing_token\
         ), \
         upserted_lease AS (\
             INSERT INTO {quoted_table_name} AS lease_target \
                 ({key}, {holder_id}, {fencing_token}, {lease_token}, {expires_at}, {updated_at}) \
             SELECT \
                 $1, $2, claim_clock.{last_fencing_token}, $3, \
                 claim_clock.decision_time OPERATOR(pg_catalog.+) ($4::bigint OPERATOR(pg_catalog.*) INTERVAL '1 microsecond'), \
                 claim_clock.decision_time \
             FROM claim_clock \
             ON CONFLICT ({key}) DO UPDATE SET \
                 {holder_id} = {excluded_holder_id}, \
                 {fencing_token} = {excluded_fencing_token}, \
                 {lease_token} = {excluded_lease_token}, \
                 {expires_at} = {excluded_expires_at}, \
                 {updated_at} = {excluded_updated_at} \
             WHERE {lease_target_expires_at} OPERATOR(pg_catalog.<=) EXCLUDED.{updated_at} \
             RETURNING {holder_id}, {fencing_token}, {lease_token}, \
                 {expires_at_unix_micros} AS expires_at_unix_micros\
         ) \
         SELECT {holder_id}, {fencing_token}, {lease_token}, expires_at_unix_micros \
         FROM upserted_lease"
	)
}

fn build_renew_lease_query(quoted_table_name: &str) -> String {
	let key = LeaseColumn::LeaseKey.name();
	let holder_id = LeaseColumn::HolderId.name();
	let fencing_token = LeaseColumn::FencingToken.name();
	let lease_token = LeaseColumn::LeaseToken.name();
	let expires_at = LeaseColumn::ExpiresAt.name();
	let updated_at = LeaseColumn::UpdatedAt.name();
	let expires_at_unix_micros = expires_at_unix_micros_expression(Some("lease_target"));

	format!(
		"WITH locked_claim AS MATERIALIZED (\
             SELECT {key}, {expires_at} \
             FROM {quoted_table_name} \
             WHERE {key} OPERATOR(pg_catalog.=) $1 \
               AND {holder_id} OPERATOR(pg_catalog.=) $2 \
               AND {fencing_token} OPERATOR(pg_catalog.=) $3 \
               AND {lease_token} OPERATOR(pg_catalog.=) $4 \
             FOR UPDATE\
         ), \
         decision_clock AS MATERIALIZED (\
             SELECT pg_catalog.clock_timestamp() AS decision_time \
             FROM locked_claim\
         ) \
         UPDATE {quoted_table_name} AS lease_target \
         SET {lease_token} = $5, \
             {expires_at} = decision_clock.decision_time \
                 OPERATOR(pg_catalog.+) ($6::bigint OPERATOR(pg_catalog.*) INTERVAL '1 microsecond'), \
             {updated_at} = decision_clock.decision_time \
         FROM locked_claim, decision_clock \
         WHERE lease_target.{key} OPERATOR(pg_catalog.=) locked_claim.{key} \
           AND locked_claim.{expires_at} OPERATOR(pg_catalog.>) decision_clock.decision_time \
         RETURNING lease_target.{holder_id}, lease_target.{fencing_token}, \
             lease_target.{lease_token}, {expires_at_unix_micros}"
	)
}

fn build_release_lease_query(quoted_table_name: &str) -> String {
	build_release_query(quoted_table_name, ReleaseMatch::Claim)
}

enum ReleaseMatch {
	Claim,
	Cleanup,
}

fn build_release_query(quoted_table_name: &str, matching: ReleaseMatch) -> String {
	let key = LeaseColumn::LeaseKey.name();
	let holder_id = LeaseColumn::HolderId.name();
	let fencing_token = LeaseColumn::FencingToken.name();
	let lease_token = LeaseColumn::LeaseToken.name();
	let expires_at = LeaseColumn::ExpiresAt.name();
	let private_match = match matching {
		ReleaseMatch::Claim => format!(
			"AND {fencing_token} OPERATOR(pg_catalog.=) $3 AND {lease_token} OPERATOR(pg_catalog.=) $4"
		),
		ReleaseMatch::Cleanup => format!(
			"AND ($3::pg_catalog.int8 IS NULL OR {fencing_token} OPERATOR(pg_catalog.=) $3) AND ({lease_token} OPERATOR(pg_catalog.=) $4 OR {lease_token} OPERATOR(pg_catalog.=) $5::pg_catalog.bytea)"
		),
	};
	format!(
		"WITH locked_claim AS MATERIALIZED (\
             SELECT {key}, {expires_at} \
             FROM {quoted_table_name} \
             WHERE {key} OPERATOR(pg_catalog.=) $1 \
               AND {holder_id} OPERATOR(pg_catalog.=) $2 \
               {private_match} \
             FOR UPDATE\
         ), \
         decision_clock AS MATERIALIZED (\
             SELECT pg_catalog.clock_timestamp() AS decision_time \
             FROM locked_claim\
         ) \
         DELETE FROM {quoted_table_name} AS lease_target \
         USING locked_claim, decision_clock \
         WHERE lease_target.{key} OPERATOR(pg_catalog.=) locked_claim.{key} \
           AND locked_claim.{expires_at} OPERATOR(pg_catalog.>) decision_clock.decision_time"
	)
}
