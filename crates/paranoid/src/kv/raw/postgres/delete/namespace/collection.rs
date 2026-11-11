//! Complete valued-key observations supply identity input, never row authority.
use super::super::super::page::{RawReadLayout, append_logical_projection, text_bound};
use super::super::super::{Bytea, DatabaseOperationKind, WriteTx, owned_portable_query_as};
use super::super::{
	RawDeleteLayout, RawDeleteStorageError, RecordDeletionStep,
	discovery::{self, KeyBounds},
	records::RawDeleteRecords,
};
use crate::db::{AuditedSql, AuditedSqlBuilder};
use crate::kv::Key;
use crate::kv::raw::model::page::RecordKeySelection;
use crate::records::{RecordKey, RecordNamespace};
use std::collections::BTreeSet;
const COLLECT_LABEL: &str = "db.state_storage.raw_namespace.collect";
pub(super) async fn collect(
	tx: &mut WriteTx<'_>,
	fields: CollectionFields<'_>,
) -> Result<BTreeSet<RecordKey>, RawDeleteStorageError> {
	let CollectionFields {
		layout,
		namespace,
		selection,
	} = fields;
	let sql = build_query(&layout);
	let prefix = selection.prefix();
	let upper = prefix.and_then(|p| p.exclusive_upper_bound());
	tx.record_database_operation(
		DatabaseOperationKind::FetchAll,
		COLLECT_LABEL,
		Some(sql.as_str()),
	);
	let query = owned_portable_query_as::<(Bytea,)>(sql)
		.bind_validated(namespace.as_bytes())
		.bind_validated(prefix.map(|p| p.as_bytes()))
		.bind_validated(upper.as_deref());
	let query = if layout.source_table().is_some() {
		query
			.bind_validated(text_bound(prefix.map(|p| p.as_bytes())))
			.bind_validated(text_bound(upper.as_deref()))
	} else {
		query
	};
	let rows = query
		.fetch_all(tx.inner.as_mut())
		.await
		.map_err(|e| e.with_operation_label(COLLECT_LABEL))?;
	let mut keys = BTreeSet::new();
	let records = RawDeleteRecords::new(namespace.clone());
	for (key,) in rows {
		let key = RecordKey::new(key.0);
		let result = (|| {
			if !selection.contains(&key) || keys.last().is_some_and(|previous| previous >= &key) {
				return Err(RawDeleteStorageError::Coverage);
			}
			if layout.source_table().is_some() {
				Key::from_persisted_text(std::str::from_utf8(key.as_bytes())?.to_owned())?;
			}
			Ok(())
		})();
		result.map_err(|e: RawDeleteStorageError| {
			records.identify_unexpected(e, key.clone(), RecordDeletionStep::Discovery)
		})?;
		keys.insert(key);
	}
	Ok(keys)
}

fn build_query(layout: &RawDeleteLayout<'_>) -> AuditedSql {
	let mut sql = AuditedSqlBuilder::new();
	sql.push_text("WITH request_context AS (SELECT $1::pg_catalog.bytea AS namespace), ");
	match layout {
		RawDeleteLayout::Native(s) => {
			discovery::append_native(&mut sql, s.table(), KeyBounds::Selection)
		}
		RawDeleteLayout::Source(s) => {
			discovery::append_source(&mut sql, s.table(), KeyBounds::Selection)
		}
		RawDeleteLayout::Relocating(s) => {
			discovery::append_source(&mut sql, s.source_table(), KeyBounds::Selection);
			sql.push_text(", ");
			discovery::append_native(&mut sql, s.native_storage().table(), KeyBounds::Selection);
		}
	}
	sql.push_text(", logical_records AS (");
	match layout {
		RawDeleteLayout::Native(s) => {
			append_logical_projection(&mut sql, &RawReadLayout::Native(s))
		}
		RawDeleteLayout::Source(s) => {
			append_logical_projection(&mut sql, &RawReadLayout::Source(s))
		}
		RawDeleteLayout::Relocating(s) => {
			append_logical_projection(&mut sql, &RawReadLayout::Relocating(s))
		}
	}
	sql.push_text(
		") SELECT key_bytes FROM logical_records ORDER BY key_bytes USING OPERATOR(pg_catalog.<)",
	);
	sql.finish()
}

pub(super) struct CollectionFields<'a> {
	pub(super) layout: RawDeleteLayout<'a>,
	pub(super) namespace: &'a RecordNamespace,
	pub(super) selection: &'a RecordKeySelection,
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::db::{PgIdentifier, PgQualifiedTableName, PgSchemaName};
	use crate::records::postgres::PgPointStorage;
	use crate::records::postgres::v10::{
		relocating_point::PgRelocatingPointStorage, source_point::PgSourcePointStorage,
	};

	fn table(name: &str) -> PgQualifiedTableName {
		PgQualifiedTableName::new(
			PgSchemaName::new(PgIdentifier::new("public").expect("valid schema")),
			PgIdentifier::new(name).expect("valid table"),
		)
	}

	#[test]
	fn source_and_relocating_collection_queries_push_down_text_bounds() {
		let source = PgSourcePointStorage::from_validated_table(table("kv_v10"));
		let source_sql = build_query(&RawDeleteLayout::Source(&source));
		assert!(
			source_sql
				.as_str()
				.contains("key OPERATOR(pg_catalog.~>=~) $4::pg_catalog.text")
		);
		assert!(
			source_sql
				.as_str()
				.contains("key OPERATOR(pg_catalog.~<~) $5::pg_catalog.text")
		);

		let relocating = PgRelocatingPointStorage::for_tests(
			PgPointStorage::new(table("paranoid_records")),
			table("kv_v10"),
		);
		let relocating_sql = build_query(&RawDeleteLayout::Relocating(&relocating));
		assert!(
			relocating_sql
				.as_str()
				.contains("key OPERATOR(pg_catalog.~>=~) $4::pg_catalog.text")
		);
		assert!(
			relocating_sql
				.as_str()
				.contains("key OPERATOR(pg_catalog.~<~) $5::pg_catalog.text")
		);

		let native = PgPointStorage::new(table("paranoid_records"));
		let native_sql = build_query(&RawDeleteLayout::Native(&native));
		assert!(!native_sql.as_str().contains("OPERATOR(pg_catalog.~>=~)"));
	}
}
