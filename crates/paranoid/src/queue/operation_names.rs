//! Operation names and fixed statements that the queue reports to a
//! database-operation observer.

pub const QUEUE_SET_LOCAL_STATEMENT_TIMEOUT_LABEL: &str = "queue.set_local_statement_timeout";
pub const QUEUE_SCHEMA_VALIDATE_TABLE_COLUMNS_LABEL: &str = "queue.schema.validate_table_columns";
pub const QUEUE_SCHEMA_VALIDATE_TABLE_CHECKS_LABEL: &str = "queue.schema.validate_table_checks";
pub const QUEUE_SCHEMA_VALIDATE_PRIMARY_KEY_LABEL: &str = "queue.schema.validate_primary_key";
pub const QUEUE_SCHEMA_VALIDATE_NAMED_INDEX_LABEL: &str = "queue.schema.validate_named_index";
