const KV_SCHEMA_COMPONENT: &str = "kv";
const KV_SCHEMA_VERSION: i32 = 1;
const KV_SCHEMA_FINGERPRINT: &str = "paranoid.kv.v1";
mod operation_names;

#[derive(Clone, Debug, Eq, PartialEq)]
struct ActualColumn {
	name: String,
	data_type: String,
	not_null: bool,
	collation: Option<String>,
	writable: bool,
}

mod catalog;
mod schema;
mod sql;

pub(crate) use schema::{
	KvPhysicalSchemaBinding, RecordedKvSchemaBinding,
	validate_kv_physical_schema_in_current_transaction,
	validate_recorded_kv_schema_in_current_transaction,
};

pub(crate) use sql::slot::{append_source_slot_candidate, append_source_slot_seed};
