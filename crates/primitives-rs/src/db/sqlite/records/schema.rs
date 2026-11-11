use super::authorization::TableAccessGuard;
use super::{RecordError, RecordStoreOpenMode};
use crate::db::sqlite::{QueryOptions, Transaction};
use crate::random::secure_bytes::generate_random_byte_array;
use rusqlite::params;

pub(super) const STORE_TABLE: &str = "primitives_record_store";
pub(super) const RECORD_TABLE: &str = "primitives_records";
// These definitions identify physical format 1. Preserve their SQL text when
// reformatting Rust source; changing the owned schema requires a format upgrade.
const STORE_DEFINITION: &str = "(singleton INTEGER NOT NULL PRIMARY KEY CHECK (singleton = 1), format_magic BLOB NOT NULL, format_version INTEGER NOT NULL CHECK (format_version > 0), instance_id BLOB NOT NULL CHECK (length(instance_id) = 32)) STRICT, WITHOUT ROWID";
const RECORD_DEFINITION: &str = "(namespace BLOB NOT NULL CHECK (length(namespace) > 0), key BLOB NOT NULL, value BLOB NOT NULL, PRIMARY KEY (namespace, key)) STRICT, WITHOUT ROWID";
const FORMAT_MAGIC: &[u8] = b"primitives.sqlite.records";
const FORMAT_VERSION: i64 = 1;

pub(super) fn open(
	tx: &mut Transaction<'_>,
	mode: RecordStoreOpenMode,
) -> Result<[u8; 32], RecordError> {
	let store_schema = format!("CREATE TABLE {STORE_TABLE} {STORE_DEFINITION}");
	let record_schema = format!("CREATE TABLE {RECORD_TABLE} {RECORD_DEFINITION}");
	let objects = tx.query(
		"WITH expected(name, ddl) AS (VALUES (?1, ?2), (?3, ?4))
		 SELECT count(objects.name),
		 coalesce(sum(objects.type = 'table' AND objects.name = expected.name COLLATE BINARY AND objects.sql = expected.ddl), 0),
		 EXISTS(SELECT 1 FROM temp.sqlite_schema WHERE type = 'trigger' AND tbl_name COLLATE NOCASE IN (?1, ?3))
		 FROM expected LEFT JOIN main.sqlite_schema AS objects
		 ON objects.name = expected.name COLLATE NOCASE OR objects.tbl_name = expected.name COLLATE NOCASE",
		params![STORE_TABLE, store_schema, RECORD_TABLE, record_schema],
		QueryOptions { max_rows: 1 },
		|row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, bool>(2)?)),
	)?;
	let (object_count, matching_tables, has_temp_triggers) = objects[0];
	if object_count != 0 && mode == RecordStoreOpenMode::CreateNew {
		return Err(RecordError::StoreAlreadyExists);
	}
	if has_temp_triggers {
		return Err(RecordError::IncompatibleSchema);
	}
	if object_count == 0 {
		if mode == RecordStoreOpenMode::Existing {
			return Err(RecordError::StoreMissing);
		}
		if tx.is_read_only() {
			return Err(RecordError::WriteTransactionRequired);
		}
		let instance_id: [u8; 32] = generate_random_byte_array()?;
		let _permission = TableAccessGuard::new(&tx.owner.record_authorization);
		tx.execute(
			&format!("CREATE TABLE main.{STORE_TABLE} {STORE_DEFINITION}"),
			[],
		)?;
		tx.execute(
			&format!("CREATE TABLE main.{RECORD_TABLE} {RECORD_DEFINITION}"),
			[],
		)?;
		tx.execute("INSERT INTO main.primitives_record_store (singleton, format_magic, format_version, instance_id) VALUES (1, ?1, ?2, ?3)", params![FORMAT_MAGIC, FORMAT_VERSION, instance_id.as_slice()])?;
		return Ok(instance_id);
	}
	if object_count != 2 || matching_tables != 2 {
		return Err(RecordError::IncompatibleSchema);
	}
	let metadata = tx.query(
		"SELECT singleton = 1 AND format_magic = ?1, format_version,
		 CASE WHEN length(instance_id) = 32 THEN instance_id ELSE NULL END
		 FROM main.primitives_record_store LIMIT 2",
		[FORMAT_MAGIC],
		QueryOptions { max_rows: 2 },
		|row| {
			Ok((
				row.get::<_, bool>(0)?,
				row.get::<_, i64>(1)?,
				row.get::<_, Option<Vec<u8>>>(2)?,
			))
		},
	)?;
	if metadata.len() != 1 {
		return Err(RecordError::InvalidMetadata);
	}
	let (valid_marker, version, instance_id) = metadata.into_iter().next().unwrap();
	if !valid_marker || version <= 0 {
		return Err(RecordError::InvalidMetadata);
	}
	if version != FORMAT_VERSION {
		return Err(RecordError::UnsupportedFormat { version });
	}
	instance_id
		.and_then(|id| id.try_into().ok())
		.ok_or(RecordError::InvalidMetadata)
}
