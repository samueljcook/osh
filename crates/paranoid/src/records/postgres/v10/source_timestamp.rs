//! Exact physical TIMESTAMPTZ observations before a primitive selects its destination domain.

use crate::db::AuditedSqlBuilder;

pub(crate) enum PgSourceTimestamp {
	NegativeInfinity,
	FiniteUnixMicros(i128),
	PositiveInfinity,
}

#[derive(Debug, thiserror::Error)]
#[error("physical source timestamp is not an integer microsecond observation")]
pub(crate) struct InvalidSourceTimestampError;

/// Only fixed backend column expressions reach this builder; no application SQL is accepted.
pub(crate) fn append_timestamp_projection(sql: &mut AuditedSqlBuilder, column: &'static str) {
	sql.push_text("CASE WHEN ");
	sql.push_text(column);
	sql.push_text(" IS NULL THEN NULL WHEN pg_catalog.isfinite(");
	sql.push_text(column);
	sql.push_text(") THEN (EXTRACT(EPOCH FROM ");
	sql.push_text(column);
	sql.push_text(") OPERATOR(pg_catalog.*) 1000000)::pg_catalog.text WHEN ");
	sql.push_text(column);
	sql.push_text(" OPERATOR(pg_catalog.=) '-infinity'::pg_catalog.timestamptz THEN '-infinity' ELSE 'infinity' END");
}

/// Numeric output can have zero fractional digits; a nonzero remainder is never rounded.
pub(crate) fn parse_source_timestamp(
	value: &str,
) -> Result<PgSourceTimestamp, InvalidSourceTimestampError> {
	match value {
		"-infinity" => return Ok(PgSourceTimestamp::NegativeInfinity),
		"infinity" => return Ok(PgSourceTimestamp::PositiveInfinity),
		_ => {}
	}
	let (integer, fraction) = value
		.split_once('.')
		.map_or((value, None), |(a, b)| (a, Some(b)));
	let digits = integer.strip_prefix('-').unwrap_or(integer);
	if digits.is_empty()
		|| !digits.bytes().all(|b| b.is_ascii_digit())
		|| fraction.is_some_and(|part| part.is_empty() || !part.bytes().all(|b| b == b'0'))
	{
		return Err(InvalidSourceTimestampError);
	}
	integer
		.parse::<i128>()
		.map(PgSourceTimestamp::FiniteUnixMicros)
		.map_err(|_| InvalidSourceTimestampError)
}
