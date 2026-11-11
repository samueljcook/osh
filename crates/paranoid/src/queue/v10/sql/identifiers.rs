use super::{
	CHECK_KIND, DEAD_LETTER_NUMERIC_CONSTRAINT_SUFFIX, DEAD_LETTER_REASON_CONSTRAINT_SUFFIX,
	DEAD_LETTER_TEXT_CONSTRAINT_SUFFIX, JOB_CLAIM_TOKEN_CONSTRAINT_SUFFIX,
	JOB_LIFECYCLE_CONSTRAINT_SUFFIX, JOB_NUMERIC_CONSTRAINT_SUFFIX, JOB_STATUS_CONSTRAINT_SUFFIX,
	JOB_TEXT_CONSTRAINT_SUFFIX, PAUSE_KEY_TASK_CONSTRAINT_SUFFIX, PAUSE_TEXT_CONSTRAINT_SUFFIX,
	PgIdentifier, PgQualifiedTableName, QueueSchemaTableBinding, QueueTable,
};
use crate::db::migration_object_identifier;

pub(in crate::queue) fn migration_index_identifier(
	kind: &str,
	table_name: &PgQualifiedTableName,
	suffix: &str,
) -> PgIdentifier {
	migration_object_identifier(kind, table_name, suffix)
}

pub(in crate::queue) fn job_status_constraint_identifier(
	tables: &impl QueueSchemaTableBinding,
) -> PgIdentifier {
	migration_check_identifier(
		QueueTable::Jobs.table_name(tables),
		JOB_STATUS_CONSTRAINT_SUFFIX,
	)
}

pub(in crate::queue) fn job_lifecycle_constraint_identifier(
	tables: &impl QueueSchemaTableBinding,
) -> PgIdentifier {
	migration_check_identifier(
		QueueTable::Jobs.table_name(tables),
		JOB_LIFECYCLE_CONSTRAINT_SUFFIX,
	)
}

pub(in crate::queue) fn job_numeric_constraint_identifier(
	tables: &impl QueueSchemaTableBinding,
) -> PgIdentifier {
	migration_check_identifier(
		QueueTable::Jobs.table_name(tables),
		JOB_NUMERIC_CONSTRAINT_SUFFIX,
	)
}

pub(in crate::queue) fn job_text_constraint_identifier(
	tables: &impl QueueSchemaTableBinding,
) -> PgIdentifier {
	migration_check_identifier(
		QueueTable::Jobs.table_name(tables),
		JOB_TEXT_CONSTRAINT_SUFFIX,
	)
}

pub(in crate::queue) fn job_claim_token_constraint_identifier(
	tables: &impl QueueSchemaTableBinding,
) -> PgIdentifier {
	migration_check_identifier(
		QueueTable::Jobs.table_name(tables),
		JOB_CLAIM_TOKEN_CONSTRAINT_SUFFIX,
	)
}

pub(in crate::queue) fn dead_letter_reason_constraint_identifier(
	tables: &impl QueueSchemaTableBinding,
) -> PgIdentifier {
	migration_check_identifier(
		QueueTable::DeadLetter.table_name(tables),
		DEAD_LETTER_REASON_CONSTRAINT_SUFFIX,
	)
}

pub(in crate::queue) fn dead_letter_numeric_constraint_identifier(
	tables: &impl QueueSchemaTableBinding,
) -> PgIdentifier {
	migration_check_identifier(
		QueueTable::DeadLetter.table_name(tables),
		DEAD_LETTER_NUMERIC_CONSTRAINT_SUFFIX,
	)
}

pub(in crate::queue) fn dead_letter_text_constraint_identifier(
	tables: &impl QueueSchemaTableBinding,
) -> PgIdentifier {
	migration_check_identifier(
		QueueTable::DeadLetter.table_name(tables),
		DEAD_LETTER_TEXT_CONSTRAINT_SUFFIX,
	)
}

pub(in crate::queue) fn pause_key_task_constraint_identifier(
	tables: &impl QueueSchemaTableBinding,
) -> PgIdentifier {
	migration_check_identifier(
		QueueTable::Pause.table_name(tables),
		PAUSE_KEY_TASK_CONSTRAINT_SUFFIX,
	)
}

pub(in crate::queue) fn pause_text_constraint_identifier(
	tables: &impl QueueSchemaTableBinding,
) -> PgIdentifier {
	migration_check_identifier(
		QueueTable::Pause.table_name(tables),
		PAUSE_TEXT_CONSTRAINT_SUFFIX,
	)
}

pub(in crate::queue) fn migration_check_identifier(
	table_name: &PgQualifiedTableName,
	suffix: &str,
) -> PgIdentifier {
	migration_object_identifier(CHECK_KIND, table_name, suffix)
}
