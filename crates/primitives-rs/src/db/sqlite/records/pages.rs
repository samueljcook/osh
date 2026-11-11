use super::operations::{check_size, sql_size_limit};
use super::{RecordError, RecordField, RecordStore};
use crate::db::records::{RecordKey, RecordKeyRange, RecordNamespace};
use crate::db::sqlite::QueryOptions;
use rusqlite::types::{Value, ValueRef};
use std::num::NonZeroUsize;
use std::ops::{Bound, ControlFlow};

/// A returned key and its opaque value.
pub struct RecordEntry {
	/// The key within the requested namespace.
	pub key: RecordKey,
	/// The exact stored bytes.
	pub value: Vec<u8>,
}

impl std::fmt::Debug for RecordEntry {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter
			.debug_struct("RecordEntry")
			.field("key", &self.key)
			.field("value_size", &self.value.len())
			.finish()
	}
}

/// A continuation after the last returned key. It binds an instance ID,
/// namespace, range, and projection. It provides no mutation authority, revision,
/// or snapshot guarantee. Physical database copies retain the instance ID.
#[derive(Clone, Debug)]
pub struct RecordPageCursor {
	instance_id: [u8; 32],
	namespace: RecordNamespace,
	range: RecordKeyRange,
	with_values: bool,
	after_key: RecordKey,
}

impl RecordPageCursor {
	/// Borrows the last returned key. The next page starts strictly after it.
	pub fn after_key(&self) -> &RecordKey {
		&self.after_key
	}
}

/// A bounded page request. Budgets may change when continuing a page.
pub struct RecordPageRequest<'a> {
	/// The namespace to read.
	pub namespace: &'a RecordNamespace,
	/// The byte-key range to read in ascending order.
	pub range: &'a RecordKeyRange,
	/// A continuation from the same store, namespace, range, and projection.
	pub cursor: Option<&'a RecordPageCursor>,
	/// The maximum number of returned records.
	pub max_records: NonZeroUsize,
	/// The maximum sum of returned key and value sizes in bytes, or key sizes for
	/// key-only pages. This excludes namespace, cursor, and allocation overhead
	/// and does not bound SQLite memory or SQL work. Zero permits empty payloads.
	pub max_payload_size: usize,
}

/// Accepted records and an optional continuation after their last key.
#[derive(Debug)]
pub struct RecordPage<T> {
	/// Records in ascending byte-key order.
	pub records: Vec<T>,
	/// The sum of returned key and value sizes, or key sizes for a key-only page.
	pub payload_size: usize,
	/// A continuation when at least one more eligible row existed in this
	/// transaction's snapshot. A later transaction can observe a different state.
	pub continuation: Option<RecordPageCursor>,
}

/// A successful page operation, including an insufficient whole-page budget.
#[derive(Debug)]
pub enum RecordPageOutcome<T> {
	/// A page within both budgets. An empty page has no continuation.
	Page(RecordPage<T>),
	/// The first eligible record exceeds the whole-page budget. No key was
	/// advanced. The caller can retry with a larger budget in the same transaction.
	RecordDoesNotFit {
		/// The key and value size, or key size for a key-only page, in bytes.
		required_payload_size: usize,
	},
}

enum PageStop {
	MoreRecords,
	DoesNotFit(usize),
	Failed(RecordError),
}

impl RecordStore<'_, '_> {
	/// Reads bounded key/value records in byte order. Reads in the same transaction
	/// share its snapshot; a cursor resumed in another transaction resumes ordering
	/// against that transaction's snapshot. An excluded record is never skipped.
	pub fn read_page(
		&mut self,
		request: RecordPageRequest<'_>,
	) -> Result<RecordPageOutcome<RecordEntry>, RecordError> {
		self.read_projected_page(
			request,
			true,
			|key, value| RecordEntry { key, value },
			|entry| &entry.key,
		)
	}

	/// Reads keys without loading or applying the value-size limit to their values.
	pub fn read_key_page(
		&mut self,
		request: RecordPageRequest<'_>,
	) -> Result<RecordPageOutcome<RecordKey>, RecordError> {
		self.read_projected_page(request, false, |key, _| key, |key| key)
	}

	fn read_projected_page<T>(
		&mut self,
		request: RecordPageRequest<'_>,
		with_values: bool,
		make_entry_fn: fn(RecordKey, Vec<u8>) -> T,
		entry_key_fn: fn(&T) -> &RecordKey,
	) -> Result<RecordPageOutcome<T>, RecordError> {
		let limits = self.limits;
		let instance_id = self.instance_id;
		self.transaction.run_operation(|tx| {
			check_size(
				RecordField::Namespace,
				request.namespace.as_bytes().len(),
				limits.max_namespace_size,
			)?;
			for bound in [request.range.lower_bound(), request.range.upper_bound()] {
				if let Bound::Included(key) | Bound::Excluded(key) = bound {
					check_size(RecordField::Key, key.as_bytes().len(), limits.max_key_size)?;
				}
			}
			if let Some(cursor) = request.cursor {
				if cursor.instance_id != instance_id
					|| &cursor.namespace != request.namespace
					|| &cursor.range != request.range
					|| cursor.with_values != with_values
				{
					return Err(RecordError::CursorContextMismatch);
				}
				check_size(
					RecordField::Key,
					cursor.after_key.as_bytes().len(),
					limits.max_key_size,
				)?;
			}
			let mut sql = String::from(
				"SELECT length(key), typeof(key) = 'blob',
				 CASE WHEN length(key) <= ?2 AND length(key) <= ?4 THEN key ELSE NULL END",
			);
			if with_values {
				sql.push_str(
					", length(value), typeof(value) = 'blob',
					 CASE WHEN length(key) <= ?2 AND length(value) <= ?3
					 AND length(key) + length(value) <= ?4 THEN value ELSE NULL END",
				);
			}
			sql.push_str(" FROM main.primitives_records WHERE namespace = ?1");
			let mut params = vec![
				Value::Blob(request.namespace.as_bytes().to_vec()),
				Value::Integer(sql_size_limit(limits.max_key_size)),
				Value::Integer(sql_size_limit(limits.max_value_size)),
				Value::Integer(sql_size_limit(request.max_payload_size)),
			];
			let lower = request
				.cursor
				.map_or(request.range.lower_bound(), |cursor| {
					Bound::Excluded(&cursor.after_key)
				});
			append_bound(&mut sql, &mut params, lower, true);
			append_bound(&mut sql, &mut params, request.range.upper_bound(), false);
			let row_limit = request.max_records.get().saturating_add(1);
			params.push(Value::Integer(sql_size_limit(row_limit)));
			sql.push_str(&format!(" ORDER BY key LIMIT ?{}", params.len()));
			let mut records = Vec::new();
			let mut payload_size = 0usize;
			let completion = tx.visit_rows(
				&sql,
				rusqlite::params_from_iter(params),
				QueryOptions {
					max_rows: row_limit,
				},
				|row| {
					if records.len() == request.max_records.get() {
						return Ok(ControlFlow::Break(PageStop::MoreRecords));
					}
					let key_size = row.get::<_, i64>(0)?;
					let is_key_blob = row.get::<_, bool>(1)?;
					let (value_size, is_value_blob) = if with_values {
						(row.get::<_, i64>(3)?, row.get::<_, bool>(4)?)
					} else {
						(0, true)
					};
					let accept = (|| {
						if !is_key_blob || !is_value_blob {
							return Err(RecordError::InvalidRecord);
						}
						let key_size =
							usize::try_from(key_size).map_err(|_| RecordError::InvalidRecord)?;
						let value_size =
							usize::try_from(value_size).map_err(|_| RecordError::InvalidRecord)?;
						check_size(RecordField::Key, key_size, limits.max_key_size)?;
						check_size(RecordField::Value, value_size, limits.max_value_size)?;
						let size = key_size
							.checked_add(value_size)
							.ok_or(RecordError::InvalidRecord)?;
						if size > request.max_payload_size - payload_size {
							return Ok(Some(if records.is_empty() {
								PageStop::DoesNotFit(size)
							} else {
								PageStop::MoreRecords
							}));
						}
						let key = match row
							.get_ref(2)
							.map_err(crate::db::sqlite::OperationError::from)?
						{
							ValueRef::Blob(bytes) if bytes.len() == key_size => copy_bytes(bytes)?,
							_ => return Err(RecordError::InvalidRecord),
						};
						let value = if with_values {
							match row
								.get_ref(5)
								.map_err(crate::db::sqlite::OperationError::from)?
							{
								ValueRef::Blob(bytes) if bytes.len() == value_size => {
									copy_bytes(bytes)?
								}
								_ => return Err(RecordError::InvalidRecord),
							}
						} else {
							Vec::new()
						};
						records
							.try_reserve(1)
							.map_err(|_| RecordError::AllocationFailed)?;
						records.push(make_entry_fn(RecordKey::new(key), value));
						payload_size += size;
						Ok(None)
					})();
					Ok(match accept {
						Ok(None) => ControlFlow::Continue(()),
						Ok(Some(stop)) => ControlFlow::Break(stop),
						Err(error) => ControlFlow::Break(PageStop::Failed(error)),
					})
				},
			)?;
			let continuation = match completion {
				ControlFlow::Break(PageStop::Failed(error)) => return Err(error),
				ControlFlow::Break(PageStop::DoesNotFit(required_payload_size)) => {
					return Ok(RecordPageOutcome::RecordDoesNotFit {
						required_payload_size,
					});
				}
				ControlFlow::Break(PageStop::MoreRecords) => {
					let last_record = records
						.last()
						.expect("a continuation follows at least one accepted record");
					Some(RecordPageCursor {
						instance_id,
						namespace: request.namespace.clone(),
						range: request.range.clone(),
						with_values,
						after_key: RecordKey::new(copy_bytes(
							entry_key_fn(last_record).as_bytes(),
						)?),
					})
				}
				ControlFlow::Continue(()) => None,
			};
			Ok(RecordPageOutcome::Page(RecordPage {
				records,
				payload_size,
				continuation,
			}))
		})
	}
}

fn append_bound(sql: &mut String, params: &mut Vec<Value>, bound: Bound<&RecordKey>, lower: bool) {
	let (key, inclusive) = match bound {
		Bound::Included(key) => (key, true),
		Bound::Excluded(key) => (key, false),
		Bound::Unbounded => return,
	};
	params.push(Value::Blob(key.as_bytes().to_vec()));
	let comparison = match (lower, inclusive) {
		(true, true) => ">=",
		(true, false) => ">",
		(false, true) => "<=",
		(false, false) => "<",
	};
	sql.push_str(&format!(" AND key {comparison} ?{}", params.len()));
}

fn copy_bytes(bytes: &[u8]) -> Result<Vec<u8>, RecordError> {
	let mut copy = Vec::new();
	copy.try_reserve_exact(bytes.len())
		.map_err(|_| RecordError::AllocationFailed)?;
	copy.extend_from_slice(bytes);
	Ok(copy)
}
