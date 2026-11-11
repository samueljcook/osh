//! Fixed descriptor-only candidate queries with portable bytewise cursors.

use crate::db::{AuditedSql, AuditedSqlBuilder, PgQualifiedTableName};
use crate::queue::{QueueColumn, QueueSchemaLayout};

use super::{PgV10SourceDescriptor, PgV10SourceRole, SourceCandidateRole};

pub(super) struct SourceQueries {
	raw: AuditedSql,
	fleet: AuditedSql,
	lease: AuditedSql,
	jobs: AuditedSql,
	dead_letters: AuditedSql,
	pauses: AuditedSql,
}
impl SourceQueries {
	pub(super) fn new(descriptor: &PgV10SourceDescriptor) -> Self {
		Self {
			raw: text_key_query(descriptor.table(PgV10SourceRole::RawKv)),
			fleet: text_key_query(descriptor.table(PgV10SourceRole::FleetState)),
			lease: lease_key_query(descriptor),
			jobs: job_id_query(descriptor.table(PgV10SourceRole::QueueJobs)),
			dead_letters: job_id_query(descriptor.table(PgV10SourceRole::QueueDeadLetters)),
			pauses: pause_key_query(descriptor.table(PgV10SourceRole::QueuePauses)),
		}
	}
	pub(super) fn for_role(&self, role: SourceCandidateRole) -> &AuditedSql {
		match role {
			SourceCandidateRole::RawKv => &self.raw,
			SourceCandidateRole::FleetState => &self.fleet,
			SourceCandidateRole::LeaseAggregate => &self.lease,
			SourceCandidateRole::QueueJobs => &self.jobs,
			SourceCandidateRole::QueueDeadLetters => &self.dead_letters,
			SourceCandidateRole::QueuePauses => &self.pauses,
		}
	}
}

fn text_key_query(table: &PgQualifiedTableName) -> AuditedSql {
	crate::audited_sql!(
		"SELECT pg_catalog.convert_to(key, 'UTF8'), NULL::pg_catalog.bytea AS task FROM ONLY {table} WHERE ($1::pg_catalog.bytea IS NULL OR key OPERATOR(pg_catalog.~>~) pg_catalog.convert_from($1::pg_catalog.bytea, 'UTF8')) ORDER BY key USING OPERATOR(pg_catalog.~<~) LIMIT $2",
		table = table,
	)
}
/// Pages each lease table through its primary key and merges the two pages. The key columns
/// have the collation "C" or "POSIX", so each primary key orders keys by their bytes in the
/// server encoding, which opening requires to equal their UTF-8 byte order.
fn lease_key_query(descriptor: &PgV10SourceDescriptor) -> AuditedSql {
	crate::audited_sql!(
		"SELECT candidate.key, NULL::pg_catalog.bytea AS task FROM ((SELECT pg_catalog.convert_to(claim.key, 'UTF8') AS key FROM ONLY {claims} AS claim WHERE ($1::pg_catalog.bytea IS NULL OR claim.key OPERATOR(pg_catalog.>) pg_catalog.convert_from($1::pg_catalog.bytea, 'UTF8')) ORDER BY claim.key USING OPERATOR(pg_catalog.<) LIMIT $2) UNION (SELECT pg_catalog.convert_to(fence.key, 'UTF8') AS key FROM ONLY {fencing} AS fence WHERE ($1::pg_catalog.bytea IS NULL OR fence.key OPERATOR(pg_catalog.>) pg_catalog.convert_from($1::pg_catalog.bytea, 'UTF8')) ORDER BY fence.key USING OPERATOR(pg_catalog.<) LIMIT $2)) AS candidate ORDER BY candidate.key USING OPERATOR(pg_catalog.<) LIMIT $2",
		claims = descriptor.table(PgV10SourceRole::FleetClaims),
		fencing = descriptor.table(PgV10SourceRole::FleetFencing),
	)
}
fn job_id_query(table: &PgQualifiedTableName) -> AuditedSql {
	let id = QueueColumn::Id.name_in(QueueSchemaLayout::SourceEpoch);
	let mut sql = AuditedSqlBuilder::new();
	sql.push_text("SELECT ");
	sql.push_text(id);
	sql.push_text(" AS candidate, NULL::pg_catalog.bytea AS task FROM ONLY ");
	sql.push_argument(table);
	sql.push_text(" WHERE ($1::pg_catalog.bytea IS NULL OR ");
	sql.push_text(id);
	sql.push_text(" OPERATOR(pg_catalog.>) $1::pg_catalog.bytea) ORDER BY ");
	sql.push_text(id);
	sql.push_text(" USING OPERATOR(pg_catalog.<) LIMIT $2");
	sql.finish()
}
fn pause_key_query(table: &PgQualifiedTableName) -> AuditedSql {
	let key = QueueColumn::Key.name_in(QueueSchemaLayout::SourceEpoch);
	let task_name = QueueColumn::TaskName.name_in(QueueSchemaLayout::SourceEpoch);
	let mut sql = AuditedSqlBuilder::new();
	sql.push_text("SELECT pg_catalog.convert_to(");
	sql.push_text(key);
	sql.push_text(", 'UTF8'), pg_catalog.convert_to(");
	sql.push_text(task_name);
	sql.push_text(", 'UTF8') AS task FROM ONLY ");
	sql.push_argument(table);
	sql.push_text(" WHERE ($1::pg_catalog.bytea IS NULL OR ");
	sql.push_text(key);
	sql.push_text(
		" OPERATOR(pg_catalog.~>~) pg_catalog.convert_from($1::pg_catalog.bytea, 'UTF8')) ORDER BY ",
	);
	sql.push_text(key);
	sql.push_text(" USING OPERATOR(pg_catalog.~<~) LIMIT $2");
	sql.finish()
}

#[cfg(test)]
mod tests {
	use super::{job_id_query, pause_key_query};
	use crate::db::{PgIdentifier, PgQualifiedTableName, PgSchemaName};

	fn table(name: &str) -> PgQualifiedTableName {
		PgQualifiedTableName::new(
			PgSchemaName::new(PgIdentifier::new("v10").expect("valid schema")),
			PgIdentifier::new(name).expect("valid table"),
		)
	}

	#[test]
	fn pause_key_query_orders_by_indexed_text_key_with_pattern_operators() {
		let sql = pause_key_query(&table("queue_pauses"));
		assert_eq!(
			sql.as_str(),
			"SELECT pg_catalog.convert_to(key, 'UTF8'), pg_catalog.convert_to(task_name, 'UTF8') AS task FROM ONLY \"v10\".\"queue_pauses\" WHERE ($1::pg_catalog.bytea IS NULL OR key OPERATOR(pg_catalog.~>~) pg_catalog.convert_from($1::pg_catalog.bytea, 'UTF8')) ORDER BY key USING OPERATOR(pg_catalog.~<~) LIMIT $2"
		);
	}

	#[test]
	fn job_id_query_orders_directly_by_id_without_subquery() {
		let sql = job_id_query(&table("queue_jobs"));
		assert_eq!(
			sql.as_str(),
			"SELECT id AS candidate, NULL::pg_catalog.bytea AS task FROM ONLY \"v10\".\"queue_jobs\" WHERE ($1::pg_catalog.bytea IS NULL OR id OPERATOR(pg_catalog.>) $1::pg_catalog.bytea) ORDER BY id USING OPERATOR(pg_catalog.<) LIMIT $2"
		);
	}
}
