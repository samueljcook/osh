use super::{PgIdentifier, PgQualifiedTableName};
use primitives_rs::crypto::algorithms::blake3;
use primitives_rs::encoding::binary_to_text::ByteEncoding;

/// The columns of one table, by catalog identity.
///
/// The type column names a built-in type only when the column's type is that built-in type
/// itself, by the identity that PostgreSQL assigns in `pg_type.dat`, with no type modifier.
/// A domain, an extension type, or a type that merely has the same name in the session's
/// `search_path` produces text that no required type matches. The collation column names a
/// collation only when it is deterministic, from the `libc` provider, and compares bytes; a
/// collation of another provider, a nondeterministic collation, or a collation of another
/// locale produces text that no required collation matches. The last column reports whether
/// the column accepts the values that Paranoid writes: a stored generated column and an
/// identity column that always generates do not.
pub(crate) const PG_COLUMN_METADATA_SQL: &str = r#"
	SELECT
		attr.attname,
		CASE WHEN attr.atttypmod OPERATOR(pg_catalog.=) -1 AND attr.atttypid OPERATOR(pg_catalog.=) ANY (ARRAY[17, 20, 23, 25, 1184, 3802]::pg_catalog.oid[])
			THEN CASE attr.atttypid
				WHEN 17 THEN 'bytea'
				WHEN 20 THEN 'bigint'
				WHEN 23 THEN 'integer'
				WHEN 25 THEN 'text'
				WHEN 1184 THEN 'timestamp with time zone'
				ELSE 'jsonb'
			END
			ELSE 'a type that is not the required built-in type (type identity ' OPERATOR(pg_catalog.||) attr.atttypid::pg_catalog.text
				OPERATOR(pg_catalog.||) ', modifier ' OPERATOR(pg_catalog.||) attr.atttypmod::pg_catalog.text OPERATOR(pg_catalog.||) ')'
		END,
		attr.attnotnull,
		CASE
			WHEN coll.oid IS NULL THEN NULL
			WHEN coll.collprovider OPERATOR(pg_catalog.=) 'c'
			 AND coll.collisdeterministic
			 AND coll.collcollate OPERATOR(pg_catalog.=) ANY (ARRAY['C', 'POSIX']::pg_catalog.text[])
			 AND coll.collctype OPERATOR(pg_catalog.=) ANY (ARRAY['C', 'POSIX']::pg_catalog.text[])
			THEN coll.collname::pg_catalog.text
			ELSE 'a collation that does not compare bytes (' OPERATOR(pg_catalog.||) coll.collname::pg_catalog.text OPERATOR(pg_catalog.||) ')'
		END,
		(attr.attgenerated OPERATOR(pg_catalog.=) '' AND attr.attidentity OPERATOR(pg_catalog.<>) 'a')
	FROM pg_catalog.pg_attribute attr
	LEFT JOIN pg_catalog.pg_collation coll ON coll.oid OPERATOR(pg_catalog.=) attr.attcollation
	WHERE attr.attrelid OPERATOR(pg_catalog.=) pg_catalog.to_regclass($1)
	  AND attr.attnum OPERATOR(pg_catalog.>) 0
	  AND NOT attr.attisdropped
	"#;

/// Conditions on the `idx` and `index_class` aliases of a catalog query that make an index a
/// btree that is ready for use and that depends on nothing beyond its table, its key
/// constraint, and the collations of its columns.
///
/// PostgreSQL omits dependencies on pinned objects, so any other dependency, such as one on a
/// look-alike operator class or function, is an object that is not built in. The identity 403 is
/// the btree access method that PostgreSQL assigns in `pg_am.dat`.
macro_rules! builtin_btree_index_conditions {
	() => {
		r#"
		  AND idx.indisvalid
		  AND idx.indisready
		  AND idx.indislive
		  AND index_class.relam OPERATOR(pg_catalog.=) 403
		  AND NOT EXISTS (
			SELECT 1
			FROM pg_catalog.pg_depend dep
			WHERE dep.classid OPERATOR(pg_catalog.=) 'pg_catalog.pg_class'::pg_catalog.regclass
			  AND dep.objid OPERATOR(pg_catalog.=) idx.indexrelid
			  AND NOT (
				(dep.refclassid OPERATOR(pg_catalog.=) 'pg_catalog.pg_class'::pg_catalog.regclass
				 AND dep.refobjid OPERATOR(pg_catalog.=) idx.indrelid)
				OR (dep.refclassid OPERATOR(pg_catalog.=) 'pg_catalog.pg_collation'::pg_catalog.regclass
				 AND dep.refobjid OPERATOR(pg_catalog.=) ANY (idx.indcollation))
				OR (dep.refclassid OPERATOR(pg_catalog.=) 'pg_catalog.pg_constraint'::pg_catalog.regclass
				 AND EXISTS (
					SELECT 1
					FROM pg_catalog.pg_constraint con
					WHERE con.oid OPERATOR(pg_catalog.=) dep.refobjid
					  AND con.conindid OPERATOR(pg_catalog.=) idx.indexrelid
					  AND con.conrelid OPERATOR(pg_catalog.=) idx.indrelid
					  AND con.contype OPERATOR(pg_catalog.=) ANY (ARRAY['p', 'u']::pg_catalog."char"[])
				))
			  )
		  )
		"#
	};
}
pub(crate) use builtin_btree_index_conditions;

/// The operator class that `timestamptz` indexes use.
pub(crate) const TIMESTAMPTZ_OPERATOR_CLASS: &str = "timestamptz_ops";

/// An ascending single-column btree index with a name, no predicate, and no expression.
/// Its key uses the column's own collation and the operator class `$4` from `pg_catalog`.
/// The bound values are the table, the index name, the column, and the operator class.
pub(crate) const PG_SINGLE_COLUMN_INDEX_EXISTS_SQL: &str = concat!(
	r#"
	SELECT EXISTS (
		SELECT 1
		FROM pg_catalog.pg_index idx
		JOIN pg_catalog.pg_class index_class ON index_class.oid OPERATOR(pg_catalog.=) idx.indexrelid
		JOIN pg_catalog.pg_attribute attr
		  ON attr.attrelid OPERATOR(pg_catalog.=) idx.indrelid
		 AND attr.attnum OPERATOR(pg_catalog.=) idx.indkey[0]
		 AND NOT attr.attisdropped
		JOIN pg_catalog.pg_opclass opclass
		  ON opclass.oid OPERATOR(pg_catalog.=) idx.indclass[0]
		 AND opclass.opcmethod OPERATOR(pg_catalog.=) index_class.relam
		 AND opclass.opcnamespace OPERATOR(pg_catalog.=) 'pg_catalog'::pg_catalog.regnamespace
		WHERE idx.indrelid OPERATOR(pg_catalog.=) pg_catalog.to_regclass($1)
		  AND index_class.relname OPERATOR(pg_catalog.=) $2
		  AND idx.indnkeyatts OPERATOR(pg_catalog.=) 1
		  AND idx.indpred IS NULL
		  AND idx.indexprs IS NULL
		  AND attr.attname OPERATOR(pg_catalog.=) $3
		  AND opclass.opcname OPERATOR(pg_catalog.=) $4
		  AND idx.indoption[0] OPERATOR(pg_catalog.=) 0
		  AND idx.indcollation[0] OPERATOR(pg_catalog.=) attr.attcollation"#,
	builtin_btree_index_conditions!(),
	r#"
	)
	"#
);

/// A unique, immediate, single-column btree index over a text key that `ON CONFLICT (key)` can
/// use. Its key uses the column's own collation and a text operator class from
/// `pg_catalog`. The ordering of the key does not matter to the arbiter. The bound values are
/// the table and the column.
pub(crate) const PG_USABLE_SINGLE_COLUMN_UNIQUE_INDEX_EXISTS_SQL: &str = concat!(
	r#"
	SELECT EXISTS (
		SELECT 1
		FROM pg_catalog.pg_index idx
		JOIN pg_catalog.pg_class index_class ON index_class.oid OPERATOR(pg_catalog.=) idx.indexrelid
		JOIN pg_catalog.pg_attribute attr
		  ON attr.attrelid OPERATOR(pg_catalog.=) idx.indrelid
		 AND attr.attname OPERATOR(pg_catalog.=) $2
		 AND NOT attr.attisdropped
		JOIN pg_catalog.pg_opclass opclass
		  ON opclass.oid OPERATOR(pg_catalog.=) idx.indclass[0]
		 AND opclass.opcmethod OPERATOR(pg_catalog.=) index_class.relam
		 AND opclass.opcnamespace OPERATOR(pg_catalog.=) 'pg_catalog'::pg_catalog.regnamespace
		WHERE idx.indrelid OPERATOR(pg_catalog.=) pg_catalog.to_regclass($1)
		  AND idx.indisunique
		  AND idx.indimmediate
		  AND idx.indnkeyatts OPERATOR(pg_catalog.=) 1
		  AND idx.indpred IS NULL
		  AND idx.indexprs IS NULL
		  AND idx.indkey[0] OPERATOR(pg_catalog.=) attr.attnum
		  AND opclass.opcname OPERATOR(pg_catalog.=) ANY (ARRAY['text_ops', 'text_pattern_ops']::pg_catalog.text[])
		  AND idx.indcollation[0] OPERATOR(pg_catalog.=) attr.attcollation"#,
	builtin_btree_index_conditions!(),
	r#"
	)
	"#
);

#[doc(hidden)]
pub fn normalize_check_constraint_expression(expression: &str) -> String {
	let mut normalized = expression
		.chars()
		.filter(|character| !character.is_whitespace())
		.collect::<String>();

	while expression_has_single_outer_parentheses(&normalized) {
		normalized = normalized[1..normalized.len() - 1].to_owned();
	}

	normalized
}

fn expression_has_single_outer_parentheses(expression: &str) -> bool {
	if !expression.starts_with('(') || !expression.ends_with(')') {
		return false;
	}

	let mut depth = 0_i32;
	let final_index = expression.len() - 1;
	for (index, character) in expression.char_indices() {
		match character {
			'(' => depth += 1,
			')' => {
				depth -= 1;
				if depth == 0 && index != final_index {
					return false;
				}
			}
			_ => {}
		}
	}

	depth == 0
}

/// Catalog expressions with no dependencies beyond their own relation and pinned built-ins.
/// PostgreSQL omits dependencies on pinned objects; custom functions, operators,
/// types, or collations cannot supply a required built-in constraint.
pub(crate) const BUILTIN_CHECK_EXPRESSIONS_SQL: &str = r#"
SELECT pg_catalog.pg_get_expr(con.conbin, con.conrelid)
FROM pg_catalog.pg_constraint con
WHERE con.conrelid OPERATOR(pg_catalog.=) pg_catalog.to_regclass($1)
    AND con.contype OPERATOR(pg_catalog.=) 'c'
    AND con.convalidated
    AND NOT EXISTS (
        SELECT 1 FROM pg_catalog.pg_depend dep
        WHERE dep.classid OPERATOR(pg_catalog.=) 'pg_catalog.pg_constraint'::pg_catalog.regclass
            AND dep.objid OPERATOR(pg_catalog.=) con.oid
            AND NOT (
                dep.refclassid OPERATOR(pg_catalog.=) 'pg_catalog.pg_class'::pg_catalog.regclass
                AND dep.refobjid OPERATOR(pg_catalog.=) con.conrelid
            )
    )
"#;

/// Normalizes only catalog expressions selected by `BUILTIN_CHECK_EXPRESSIONS_SQL`.
pub(crate) fn normalize_builtin_check_expression(expression: &str) -> String {
	let mut normalized = normalize_check_constraint_expression(expression)
		.replace("pg_catalog.octet_length(", "octet_length(")
		.replace("::pg_catalog.text", "::text");
	for operator in ["=", ">", "<="] {
		normalized = normalized.replace(&format!("OPERATOR(pg_catalog.{operator})"), operator);
	}
	normalized
}

pub(crate) fn migration_object_identifier(
	kind: &str,
	table_name: &PgQualifiedTableName,
	suffix: &str,
) -> PgIdentifier {
	let quoted_table_name = table_name.quoted().to_string();
	let hash_input = [kind, quoted_table_name.as_str(), suffix].join("\0");
	let hash = blake3::hash(hash_input.as_bytes());
	PgIdentifier::new(format!(
		"{}_{}_{}",
		kind,
		suffix,
		ByteEncoding::HexLower.encode(&hash.as_bytes()[..8]),
	))
	.expect("generated migration object name must be valid")
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::db::PgSchemaName;

	const KEY_BOUNDS_SQL: &str = "(octet_length(key)>0)AND(octet_length(key)<=2048)";

	#[test]
	fn qualifications_that_pg_prints_for_built_in_objects_are_removed() {
		assert_eq!(
			normalize_builtin_check_expression(
				"((pg_catalog.octet_length(key) OPERATOR(pg_catalog.>) 0) AND (pg_catalog.octet_length(key) OPERATOR(pg_catalog.<=) 2048))"
			),
			KEY_BOUNDS_SQL
		);
		assert_eq!(
			normalize_builtin_check_expression(
				"(pg_catalog.octet_length(lease_token) OPERATOR(pg_catalog.=) 32)"
			),
			"octet_length(lease_token)=32"
		);
	}

	#[test]
	fn structure_that_differs_from_the_required_check_is_kept() {
		for expression in [
			"((octet_length(key) > 0) AND (octet_length(key) <= 2048)) OR true",
			"((octet_length(key) > 0) OR (octet_length(key) <= 2048))",
			"((application.octet_length(key) > 0) AND (application.octet_length(key) <= 2048))",
			"((octet_length(key) OPERATOR(application.>) 0) AND (octet_length(key) <= 2048))",
			"((octet_length(key) > 0) AND (octet_length(key) <= 4096))",
		] {
			assert_ne!(
				normalize_builtin_check_expression(expression),
				KEY_BOUNDS_SQL,
				"{expression}"
			);
		}
	}

	#[test]
	fn the_ledger_and_lease_checks_do_not_accept_an_or_true_weakening() {
		for (expression, expected) in [
			("(( fencing_token > 0 ))", "fencing_token>0"),
			(
				"(octet_length(lease_token) = 32)",
				"octet_length(lease_token)=32",
			),
			(
				"(octet_length(key) > 0 AND octet_length(key) <= 2048)",
				"octet_length(key)>0ANDoctet_length(key)<=2048",
			),
			(
				"(octet_length(holder_id) > 0 AND octet_length(holder_id) <= 512)",
				"octet_length(holder_id)>0ANDoctet_length(holder_id)<=512",
			),
		] {
			assert_eq!(normalize_builtin_check_expression(expression), expected);
			assert_ne!(
				normalize_builtin_check_expression(&format!("{expression} OR true")),
				expected,
				"{expression}"
			);
		}
	}

	#[test]
	fn only_whitespace_and_whole_expression_parentheses_are_normalized() {
		assert_eq!(
			normalize_check_constraint_expression("((a > 0) AND (b > 0))"),
			"(a>0)AND(b>0)"
		);
		assert_eq!(
			normalize_check_constraint_expression("(a > 0) AND (b > 0)"),
			"(a>0)AND(b>0)"
		);
	}

	fn table(schema: &str) -> PgQualifiedTableName {
		PgQualifiedTableName::new(
			PgSchemaName::from_identifier_text(schema).expect("table"),
			PgIdentifier::new("kv_store").expect("table"),
		)
	}

	#[test]
	fn migration_object_names_follow_the_blake3_naming_of_the_published_releases() {
		for (suffix, expected) in [
			("expires_at", "idx_expires_at_0c71bc52b7b36c08"),
			("key_pattern", "idx_key_pattern_8e5a693c9fe9fde7"),
			("updated_at", "idx_updated_at_d3e1044036db5058"),
		] {
			assert_eq!(
				migration_object_identifier("idx", &table("app"), suffix).as_str(),
				expected
			);
		}
	}

	#[test]
	fn migration_object_names_include_the_qualified_table_identity() {
		assert_ne!(
			migration_object_identifier("idx", &table("one"), "expires_at"),
			migration_object_identifier("idx", &table("two"), "expires_at")
		);
	}
}
