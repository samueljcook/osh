//! Operation names that the KV primitive reports to a database-operation observer.

pub const KV_SCHEMA_VALIDATE_COLUMNS_LABEL: &str = "kv.schema.validate_columns";
pub const KV_SCHEMA_VALIDATE_KEY_CONFLICT_ARBITER_LABEL: &str =
	"kv.schema.validate_key_conflict_arbiter";
pub const KV_SCHEMA_VALIDATE_CHECK_CONSTRAINTS_LABEL: &str = "kv.schema.validate_check_constraints";
pub const KV_SCHEMA_VALIDATE_EXPIRES_AT_INDEX_LABEL: &str = "kv.schema.validate_expires_at_index";
pub const KV_SCHEMA_VALIDATE_KEY_PATTERN_INDEX_LABEL: &str = "kv.schema.validate_key_pattern_index";
pub const KV_SCHEMA_VALIDATE_UPDATED_AT_INDEX_LABEL: &str = "kv.schema.validate_updated_at_index";
