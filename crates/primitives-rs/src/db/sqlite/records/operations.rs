use super::authorization::TableAccessGuard;
use super::{RecordAccessLimits, RecordError, RecordField, RecordStore};
use crate::db::records::RecordIdentity;
use crate::db::sqlite::QueryOptions;
use rusqlite::params;

/// A condition evaluated before applying a mutation or identifying a no-op.
#[derive(Clone, Copy)]
pub enum RecordCondition<'a> {
	/// Accepts both present and absent records.
	Any,
	/// Requires an absent record. A present empty value does not satisfy this.
	Absent,
	/// Requires a present record, including a present empty value.
	Present,
	/// Requires a present value equal to these exact bytes. This comparison does
	/// not detect an intervening change from A to B and back to A.
	ValueEquals(&'a [u8]),
}

/// The requested state of one record.
#[derive(Clone, Copy)]
pub enum RecordMutation<'a> {
	/// Stores these exact bytes, including an empty value.
	Set(&'a [u8]),
	/// Removes the record when the condition is satisfied.
	Delete,
}

impl std::fmt::Debug for RecordCondition<'_> {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::Any => formatter.write_str("Any"),
			Self::Absent => formatter.write_str("Absent"),
			Self::Present => formatter.write_str("Present"),
			Self::ValueEquals(value) => formatter
				.debug_struct("ValueEquals")
				.field("value_size", &value.len())
				.finish(),
		}
	}
}

impl std::fmt::Debug for RecordMutation<'_> {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::Delete => formatter.write_str("Delete"),
			Self::Set(value) => formatter
				.debug_struct("Set")
				.field("value_size", &value.len())
				.finish(),
		}
	}
}

/// One conditional mutation within the enclosing transaction.
#[derive(Clone, Copy, Debug)]
pub struct RecordMutationRequest<'a> {
	/// The record to inspect and change.
	pub identity: &'a RecordIdentity,
	/// The condition that must hold before applying the mutation.
	pub condition: RecordCondition<'a>,
	/// The requested state.
	pub mutation: RecordMutation<'a>,
}

/// A successful operation's provisional result. The outer transaction must
/// commit before a change is durable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordMutationOutcome {
	/// The requested mutation changed the record.
	Changed,
	/// The condition held, but the record already had the requested state.
	Unchanged,
	/// The condition failed, so the record was not changed. Other transaction
	/// operations remain provisional and may still commit.
	ConditionNotMet,
}

impl RecordStore<'_, '_> {
	/// Reads a present value or returns `None` for an absent record. A present
	/// empty value returns `Some(Vec::new())`. Oversized values fail before copying
	/// their bytes into Rust; the limit does not bound SQLite's internal memory.
	pub fn get_value(&mut self, identity: &RecordIdentity) -> Result<Option<Vec<u8>>, RecordError> {
		let limits = self.limits;
		self.transaction.run_operation(|tx| {
			check_identity(identity, limits)?;
			let rows = tx.query(
				"SELECT typeof(value) = 'blob', length(value), CASE WHEN typeof(value) = 'blob' AND length(value) <= ?3 THEN value ELSE NULL END
				 FROM main.primitives_records WHERE namespace = ?1 AND key = ?2",
				params![identity.namespace().as_bytes(), identity.key().as_bytes(), sql_size_limit(limits.max_value_size)],
				QueryOptions { max_rows: 1 },
				|row| Ok((row.get::<_, bool>(0)?, row.get::<_, i64>(1)?, row.get::<_, Option<Vec<u8>>>(2)?)),
			)?;
			let Some((is_blob, size, value)) = rows.into_iter().next() else { return Ok(None); };
			if !is_blob { return Err(RecordError::InvalidRecord); }
			let size = usize::try_from(size).map_err(|_| RecordError::InvalidRecord)?;
			check_size(RecordField::Value, size, limits.max_value_size)?;
			Ok(Some(value.ok_or(RecordError::InvalidRecord)?))
		})
	}

	/// Evaluates a condition and applies a change atomically within a write
	/// transaction. No-op writes and failed conditions issue no DML. Comparison
	/// happens in SQLite without transferring the old value into Rust. A delete or
	/// replacement can remove an old value exceeding this view's value-size limit.
	pub fn mutate(
		&mut self,
		request: RecordMutationRequest<'_>,
	) -> Result<RecordMutationOutcome, RecordError> {
		let limits = self.limits;
		self.transaction.run_operation(|tx| {
			if tx.is_read_only() {
				return Err(RecordError::WriteTransactionRequired);
			}
			check_identity(request.identity, limits)?;
			let expected = match request.condition {
				RecordCondition::ValueEquals(value) => {
					check_size(RecordField::Value, value.len(), limits.max_value_size)?;
					Some(value)
				}
				_ => None,
			};
			let replacement = match request.mutation {
				RecordMutation::Set(value) => {
					check_size(RecordField::Value, value.len(), limits.max_value_size)?;
					Some(value)
				}
				RecordMutation::Delete => None,
			};
			let namespace = request.identity.namespace().as_bytes();
			let key = request.identity.key().as_bytes();
			let rows = tx.query(
				"SELECT typeof(value) = 'blob',
				 CASE WHEN ?3 IS NOT NULL AND length(value) = length(?3) THEN value = ?3 ELSE 0 END,
				 CASE WHEN ?4 IS NOT NULL AND length(value) = length(?4) THEN value = ?4 ELSE 0 END
				 FROM main.primitives_records WHERE namespace = ?1 AND key = ?2",
				params![namespace, key, expected, replacement],
				QueryOptions { max_rows: 1 },
				|row| {
					Ok((
						row.get::<_, bool>(0)?,
						row.get::<_, bool>(1)?,
						row.get::<_, bool>(2)?,
					))
				},
			)?;
			let existing = rows.first();
			if existing.is_some_and(|(is_blob, _, _)| !is_blob) {
				return Err(RecordError::InvalidRecord);
			}
			let condition_met = match request.condition {
				RecordCondition::Any => true,
				RecordCondition::Absent => existing.is_none(),
				RecordCondition::Present => existing.is_some(),
				RecordCondition::ValueEquals(_) => existing.is_some_and(|(_, equals, _)| *equals),
			};
			if !condition_met {
				return Ok(RecordMutationOutcome::ConditionNotMet);
			}
			if match request.mutation {
				RecordMutation::Delete => existing.is_none(),
				RecordMutation::Set(_) => existing.is_some_and(|(_, _, equals)| *equals),
			} {
				return Ok(RecordMutationOutcome::Unchanged);
			}
			let _permission = TableAccessGuard::new(&tx.owner.record_authorization);
			match request.mutation {
				RecordMutation::Delete => {
					tx.execute(
						"DELETE FROM main.primitives_records WHERE namespace = ?1 AND key = ?2",
						params![namespace, key],
					)?;
				}
				RecordMutation::Set(value) => {
					if existing.is_some() {
						tx.execute(
							"UPDATE main.primitives_records SET value = ?3 WHERE namespace = ?1 AND key = ?2",
							params![namespace, key, value],
						)?;
					} else {
						tx.execute(
							"INSERT INTO main.primitives_records (namespace, key, value) VALUES (?1, ?2, ?3)",
							params![namespace, key, value],
						)?;
					}
				}
			}
			Ok(RecordMutationOutcome::Changed)
		})
	}
}

pub(super) fn check_identity(
	identity: &RecordIdentity,
	limits: RecordAccessLimits,
) -> Result<(), RecordError> {
	check_size(
		RecordField::Namespace,
		identity.namespace().as_bytes().len(),
		limits.max_namespace_size,
	)?;
	check_size(
		RecordField::Key,
		identity.key().as_bytes().len(),
		limits.max_key_size,
	)
}

pub(super) fn check_size(
	field: RecordField,
	actual_size: usize,
	max_size: usize,
) -> Result<(), RecordError> {
	if actual_size > max_size {
		return Err(RecordError::SizeLimitExceeded {
			field,
			actual_size,
			max_size,
		});
	}
	Ok(())
}

pub(super) fn sql_size_limit(size: usize) -> i64 {
	i64::try_from(size).unwrap_or(i64::MAX)
}
