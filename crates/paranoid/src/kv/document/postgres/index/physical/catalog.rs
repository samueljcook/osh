//! One catalog observation of the exact owner-chosen compound index.

use serde::Deserialize;

use crate::db::{DatabaseOperationKind, Tx, owned_portable_query_as};
use crate::kv::document::schema::IndexScalarType;

use super::grammar::{check_expressions, check_predicate};
use super::{
	PgDocumentIndexCatalogMismatch as Mismatch, PgDocumentIndexCatalogObservation,
	PgDocumentIndexExpectedCatalog, PgDocumentIndexPhysicalError, PgIndexKeyCapacity, scalar_facts,
};

const INSPECT_LABEL: &str = "db.state_storage.document_index.inspect";

// Settings and deparsed expressions belong to this same catalog request. Built-in objects
// have pinned identities; other expression dependencies must not substitute application code.
const CATALOG_SQL: &str = r#"
SELECT pg_catalog.current_setting('max_index_keys')::pg_catalog.int4,
       pg_catalog.current_setting('standard_conforming_strings') OPERATOR(pg_catalog.=) 'off',
       pg_catalog.jsonb_build_object(
         'parent_exists', parent.oid IS NOT NULL,
         'builtin_functions_match', NOT EXISTS (SELECT 1 FROM pg_catalog.pg_proc p
           WHERE p.oid OPERATOR(pg_catalog.=) ANY (ARRAY[
             'pg_catalog.encode(bytea,text)'::pg_catalog.regprocedure,
             'pg_catalog.decode(text,text)'::pg_catalog.regprocedure,
             'pg_catalog.jsonb_extract_path(jsonb,text[])'::pg_catalog.regprocedure,
             'pg_catalog.jsonb_extract_path_text(jsonb,text[])'::pg_catalog.regprocedure
           ]) AND p.provolatile OPERATOR(pg_catalog.<>) 'i'),
         'parent_shape_matches', COALESCE(parent.relkind OPERATOR(pg_catalog.=) 'r'
           AND parent.relpersistence OPERATOR(pg_catalog.=) 'p'
           AND NOT parent.relrowsecurity AND NOT parent.relforcerowsecurity
           AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_inherits h
             WHERE h.inhrelid OPERATOR(pg_catalog.=) parent.oid OR h.inhparent OPERATOR(pg_catalog.=) parent.oid)
           AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_rewrite r WHERE r.ev_class OPERATOR(pg_catalog.=) parent.oid)
           AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_trigger t WHERE t.tgrelid OPERATOR(pg_catalog.=) parent.oid
             AND t.tgenabled OPERATOR(pg_catalog.<>) 'D')
           AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_constraint c WHERE c.contype OPERATOR(pg_catalog.=) 'f'
             AND (c.conrelid OPERATOR(pg_catalog.=) parent.oid OR c.confrelid OPERATOR(pg_catalog.=) parent.oid)), false),
         'named_exists', named.oid IS NOT NULL,
         'index_kind_matches', COALESCE(named.relkind OPERATOR(pg_catalog.=) 'i'
           AND named.relpersistence OPERATOR(pg_catalog.=) 'p' AND named.relam OPERATOR(pg_catalog.=) 403
           AND named.reloptions IS NULL, false),
         'index_state_matches', COALESCE(idx.indisvalid AND idx.indisready AND idx.indislive
           AND NOT idx.indisunique AND NOT idx.indisprimary AND NOT idx.indisexclusion
           AND NOT idx.indisclustered AND NOT idx.indisreplident, false),
         'parent_association_matches', COALESCE(idx.indrelid OPERATOR(pg_catalog.=) parent.oid, false),
         'key_count', idx.indnkeyatts,
         'attr_count', idx.indnatts,
         'dependencies_match', idx.indexrelid IS NOT NULL
           AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_depend d
             WHERE d.classid OPERATOR(pg_catalog.=) 'pg_catalog.pg_class'::pg_catalog.regclass
               AND d.objid OPERATOR(pg_catalog.=) idx.indexrelid
               AND NOT ((d.refclassid OPERATOR(pg_catalog.=) 'pg_catalog.pg_class'::pg_catalog.regclass
                         AND d.refobjid OPERATOR(pg_catalog.=) parent.oid)
                     OR (d.refclassid OPERATOR(pg_catalog.=) 'pg_catalog.pg_collation'::pg_catalog.regclass
                         AND d.refobjid OPERATOR(pg_catalog.=) 'pg_catalog."C"'::pg_catalog.regcollation)))
           AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_constraint c WHERE c.conindid OPERATOR(pg_catalog.=) idx.indexrelid)
           AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_inherits h
             WHERE h.inhrelid OPERATOR(pg_catalog.=) named.oid OR h.inhparent OPERATOR(pg_catalog.=) named.oid),
         'fields', COALESCE((SELECT pg_catalog.jsonb_agg(pg_catalog.jsonb_build_object(
             'ordinal', n.ordinal,
             'table_attr', idx.indkey[n.ordinal OPERATOR(pg_catalog.-) 1],
             'is_final_key', COALESCE(idx.indkey[n.ordinal OPERATOR(pg_catalog.-) 1] OPERATOR(pg_catalog.=) key_column.attnum, false),
             'type_id', a.atttypid::pg_catalog.int8,
             'type_name', ty.typname,
             'type_namespace', tn.nspname,
             'opclass_name', oc.opcname,
             'opclass_namespace', onsp.nspname,
             'opclass_input_type', oc.opcintype::pg_catalog.int8,
             'opclass_is_btree', COALESCE(oc.opcmethod OPERATOR(pg_catalog.=) 403, false),
             'collation_is_c', COALESCE(coll.oid OPERATOR(pg_catalog.=) 'pg_catalog."C"'::pg_catalog.regcollation
               AND coll.collprovider OPERATOR(pg_catalog.=) 'c' AND coll.collisdeterministic
               AND coll.collencoding OPERATOR(pg_catalog.=) -1, false),
             'no_collation', idx.indcollation[n.ordinal OPERATOR(pg_catalog.-) 1] OPERATOR(pg_catalog.=) 0,
             'ordering', idx.indoption[n.ordinal OPERATOR(pg_catalog.-) 1],
             'attr_is_plain', COALESCE(NOT a.attisdropped AND a.attoptions IS NULL
               AND a.attidentity OPERATOR(pg_catalog.=) '' AND a.attgenerated OPERATOR(pg_catalog.=) ''
               AND a.atttypmod OPERATOR(pg_catalog.=) -1, false)
           ) ORDER BY n.ordinal)
           FROM pg_catalog.generate_series(1, idx.indnatts::pg_catalog.int4) n(ordinal)
           LEFT JOIN pg_catalog.pg_attribute a ON a.attrelid OPERATOR(pg_catalog.=) idx.indexrelid
             AND a.attnum OPERATOR(pg_catalog.=) n.ordinal
           LEFT JOIN pg_catalog.pg_type ty ON ty.oid OPERATOR(pg_catalog.=) a.atttypid
           LEFT JOIN pg_catalog.pg_namespace tn ON tn.oid OPERATOR(pg_catalog.=) ty.typnamespace
           LEFT JOIN pg_catalog.pg_opclass oc ON oc.oid OPERATOR(pg_catalog.=) idx.indclass[n.ordinal OPERATOR(pg_catalog.-) 1]
           LEFT JOIN pg_catalog.pg_namespace onsp ON onsp.oid OPERATOR(pg_catalog.=) oc.opcnamespace
           LEFT JOIN pg_catalog.pg_collation coll ON coll.oid OPERATOR(pg_catalog.=) idx.indcollation[n.ordinal OPERATOR(pg_catalog.-) 1]
         ), '[]'::pg_catalog.jsonb)
       )::pg_catalog.text,
       pg_catalog.pg_get_expr(idx.indexprs, idx.indrelid, false),
       pg_catalog.pg_get_expr(idx.indpred, idx.indrelid, false)
FROM (SELECT 1) observation
LEFT JOIN pg_catalog.pg_namespace ns ON ns.nspname OPERATOR(pg_catalog.=) $1
LEFT JOIN pg_catalog.pg_class parent ON parent.relnamespace OPERATOR(pg_catalog.=) ns.oid
  AND parent.relname OPERATOR(pg_catalog.=) $3
LEFT JOIN pg_catalog.pg_attribute key_column ON key_column.attrelid OPERATOR(pg_catalog.=) parent.oid
  AND key_column.attname OPERATOR(pg_catalog.=) 'key' AND NOT key_column.attisdropped
  AND key_column.atttypid OPERATOR(pg_catalog.=) 'pg_catalog.bytea'::pg_catalog.regtype
LEFT JOIN pg_catalog.pg_class named ON named.relnamespace OPERATOR(pg_catalog.=) ns.oid
  AND named.relname OPERATOR(pg_catalog.=) $2
LEFT JOIN pg_catalog.pg_index idx ON idx.indexrelid OPERATOR(pg_catalog.=) named.oid
"#;

type CatalogRow = (i32, bool, String, Option<String>, Option<String>);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogFacts {
	parent_exists: bool,
	builtin_functions_match: bool,
	parent_shape_matches: bool,
	named_exists: bool,
	index_kind_matches: bool,
	index_state_matches: bool,
	parent_association_matches: bool,
	key_count: Option<i32>,
	attr_count: Option<i32>,
	dependencies_match: bool,
	fields: Vec<AttrFacts>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AttrFacts {
	ordinal: i32,
	table_attr: Option<i32>,
	is_final_key: bool,
	type_id: Option<i64>,
	type_name: Option<String>,
	type_namespace: Option<String>,
	opclass_name: Option<String>,
	opclass_namespace: Option<String>,
	opclass_input_type: Option<i64>,
	opclass_is_btree: bool,
	collation_is_c: bool,
	no_collation: Option<bool>,
	ordering: Option<i32>,
	attr_is_plain: bool,
}

/// Compares one actual catalog snapshot. The caller owns serialization and any required
/// deadline. This function creates no relation, ledger entry, admission or readiness proof.
pub(crate) async fn inspect_document_index(
	tx: &mut Tx<'_>,
	expected: &PgDocumentIndexExpectedCatalog,
) -> Result<PgDocumentIndexCatalogObservation, PgDocumentIndexPhysicalError> {
	let mut completion = tx.track_state_mutation();
	tx.record_database_operation(
		DatabaseOperationKind::FetchAll,
		INSPECT_LABEL,
		Some(CATALOG_SQL),
	);
	let rows = owned_portable_query_as::<CatalogRow>(CATALOG_SQL)
		.bind_validated(expected.descriptor.table().schema().as_str())
		.bind_validated(expected.descriptor.index_name().as_str())
		.bind_validated(expected.descriptor.table().table().as_str())
		.fetch_all(tx.inner.as_mut())
		.await
		.map_err(|error| {
			PgDocumentIndexPhysicalError::Database(error.with_operation_label(INSPECT_LABEL))
		})?;
	let [row] = rows.as_slice() else {
		return Err(mismatch(Mismatch::CatalogTransport));
	};
	let observation = compare(row, expected)?;
	completion.complete();
	Ok(observation)
}

fn mismatch(issue: Mismatch) -> PgDocumentIndexPhysicalError {
	PgDocumentIndexPhysicalError::Catalog { issue }
}
fn require(value: bool, issue: Mismatch) -> Result<(), PgDocumentIndexPhysicalError> {
	if value { Ok(()) } else { Err(mismatch(issue)) }
}
fn compare(
	row: &CatalogRow,
	expected: &PgDocumentIndexExpectedCatalog,
) -> Result<PgDocumentIndexCatalogObservation, PgDocumentIndexPhysicalError> {
	let capacity = PgIndexKeyCapacity::from_server_report(row.0)?;
	let facts: CatalogFacts =
		serde_json::from_str(&row.2).map_err(|_| mismatch(Mismatch::CatalogTransport))?;
	require(facts.parent_exists, Mismatch::ParentAbsent)?;
	require(facts.parent_shape_matches, Mismatch::ParentShape)?;
	require(facts.builtin_functions_match, Mismatch::BuiltinFunctions)?;
	if !facts.named_exists {
		return Ok(PgDocumentIndexCatalogObservation::Absent { capacity });
	}
	require(facts.index_kind_matches, Mismatch::OccupiedName)?;
	require(facts.index_state_matches, Mismatch::IndexState)?;
	require(
		facts.parent_association_matches,
		Mismatch::ParentAssociation,
	)?;
	let count = expected.key_count()?;
	capacity.require_count(count)?;
	require(
		facts.key_count.and_then(|n| usize::try_from(n).ok()) == Some(count)
			&& facts.attr_count == facts.key_count
			&& facts.fields.len() == count,
		Mismatch::AttrCount,
	)?;
	require(facts.dependencies_match, Mismatch::Dependencies)?;
	for (ordinal, actual) in facts.fields.iter().enumerate() {
		let final_key = ordinal == expected.fields.len();
		let kind = if final_key {
			IndexScalarType::Bytes
		} else {
			expected.fields[ordinal].kind
		};
		let scalar = scalar_facts(kind);
		let type_id = scalar.type_id;
		require(
			usize::try_from(actual.ordinal).ok() == ordinal.checked_add(1),
			Mismatch::AttrCount,
		)?;
		require(
			if final_key {
				actual.is_final_key
			} else {
				actual.table_attr == Some(0)
			},
			Mismatch::ExpressionPositions,
		)?;
		require(
			actual.type_id == Some(type_id)
				&& actual.type_namespace.as_deref() == Some("pg_catalog")
				&& actual.type_name.as_deref() == Some(scalar.type_name),
			Mismatch::AttrType,
		)?;
		require(
			actual.opclass_input_type == Some(type_id)
				&& actual.opclass_is_btree
				&& actual.opclass_namespace.as_deref() == Some("pg_catalog")
				&& actual.opclass_name.as_deref() == Some(scalar.opclass_name),
			Mismatch::OperatorClass,
		)?;
		require(
			if scalar.collatable {
				actual.collation_is_c
			} else {
				actual.no_collation == Some(true)
			},
			Mismatch::Collation,
		)?;
		require(actual.ordering == Some(0), Mismatch::Ordering)?;
		require(actual.attr_is_plain, Mismatch::AttrOptions)?;
	}
	check_expressions(row.3.as_deref(), expected, row.1).map_err(|source| {
		PgDocumentIndexPhysicalError::CatalogExpression {
			field_index: None,
			source,
		}
	})?;
	let predicate = row
		.4
		.as_deref()
		.ok_or_else(|| mismatch(Mismatch::PredicateAbsent))?;
	check_predicate(predicate, expected, row.1).map_err(|source| {
		PgDocumentIndexPhysicalError::CatalogExpression {
			field_index: None,
			source,
		}
	})?;
	Ok(PgDocumentIndexCatalogObservation::Present)
}
