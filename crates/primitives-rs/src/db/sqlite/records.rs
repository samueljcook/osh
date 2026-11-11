//! Opaque byte records in a transaction-owned SQLite store.
//!
//! A store view borrows an existing transaction. Record operations and application
//! SQL therefore commit or roll back together. Values have no implicit codec,
//! encryption, expiry, or revision. Access limits bound data returned by this
//! facility; they do not change the persistent format.
//!
//! The store owns `main.primitives_record_store` and `main.primitives_records`.
//! Binding verifies their exact physical schema and metadata. Application SQL
//! cannot mutate or alter those tables for the remainder of that transaction,
//! even after the view is dropped. TEMP triggers targeting either table name are
//! also rejected, including when a same-named TEMP table is the intended target.
//! Other application tables remain available. This protection does not constrain
//! other database connections or direct filesystem access.
//!
//! ```no_run
//! use primitives_rs::db::records::{RecordIdentity, RecordKey, RecordNamespace};
//! use primitives_rs::db::sqlite::{Connection, JournalMode, OpenMode, OpenOptions};
//! use primitives_rs::db::sqlite::records::{
//!     RecordAccessLimits, RecordCondition, RecordMutation,
//!     RecordMutationOutcome, RecordMutationRequest, RecordStore,
//!     RecordStoreOpenMode, RecordStoreOptions,
//! };
//! use std::time::Duration;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let mut db = Connection::open("./app.db", OpenOptions {
//!     mode: OpenMode::CreateIfMissing,
//!     journal_mode: JournalMode::Wal,
//!     auto_vacuum: None,
//!     foreign_keys: true,
//!     busy_timeout: Duration::from_secs(2),
//! })?;
//! let identity = RecordIdentity::new(
//!     RecordNamespace::new(b"settings".to_vec())?,
//!     RecordKey::new(b"theme".to_vec()),
//! );
//! let outcome = db.write(|tx| {
//!     let mut records = RecordStore::open(tx, RecordStoreOptions {
//!         mode: RecordStoreOpenMode::CreateIfMissing,
//!         limits: RecordAccessLimits {
//!             max_namespace_size: 128,
//!             max_key_size: 1024,
//!             max_value_size: 1024 * 1024,
//!         },
//!     })?;
//!     records.mutate(RecordMutationRequest {
//!         identity: &identity,
//!         condition: RecordCondition::Absent,
//!         mutation: RecordMutation::Set(b"dark"),
//!     })
//! })?;
//! match outcome {
//!     RecordMutationOutcome::Changed => println!("The setting was created."),
//!     RecordMutationOutcome::ConditionNotMet => println!("The setting already exists."),
//!     RecordMutationOutcome::Unchanged => unreachable!("an absent value cannot already equal a present value"),
//! }
//! # Ok(())
//! # }
//! ```

mod authorization;
mod operations;
mod pages;
mod schema;
pub(in crate::db::sqlite) use authorization::TableAuthorization;
pub use operations::{
	RecordCondition, RecordMutation, RecordMutationOutcome, RecordMutationRequest,
};
pub use pages::{RecordEntry, RecordPage, RecordPageCursor, RecordPageOutcome, RecordPageRequest};

use super::{OperationError, Transaction};
use crate::random::secure_bytes::RandomError;

/// Whether binding may create the store's tables and metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordStoreOpenMode {
	/// Requires a complete, compatible store.
	Existing,
	/// Requires both reserved tables to be absent.
	CreateNew,
	/// Opens a compatible store or creates one when both tables are absent.
	CreateIfMissing,
}

/// Runtime limits in bytes. These limits are not persistent schema invariants.
#[derive(Clone, Copy, Debug)]
pub struct RecordAccessLimits {
	/// Maximum namespace size, which must be positive.
	pub max_namespace_size: usize,
	/// Maximum key size. Zero permits only the empty key.
	pub max_key_size: usize,
	/// Maximum value size. Zero permits only the empty value.
	pub max_value_size: usize,
}

/// Explicit options for binding a store to a transaction.
#[derive(Clone, Copy, Debug)]
pub struct RecordStoreOptions {
	/// Whether absent tables may be created.
	pub mode: RecordStoreOpenMode,
	/// The access limits to enforce for this view.
	pub limits: RecordAccessLimits,
}

/// The part of a record that exceeded an access limit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordField {
	/// A namespace's bytes.
	Namespace,
	/// A key's bytes.
	Key,
	/// A value's bytes, including an expected value in a condition.
	Value,
}

/// A record operation failed. The enclosing transaction cannot commit.
#[derive(Debug, thiserror::Error)]
pub enum RecordError {
	/// An owned SQLite operation failed.
	#[error("record storage operation failed: {0}")]
	Operation(#[from] OperationError),
	/// No reserved store objects exist.
	#[error("record store does not exist")]
	StoreMissing,
	/// At least one reserved object exists during create-only binding.
	#[error("a reserved record store object already exists")]
	StoreAlreadyExists,
	/// Reserved objects are incomplete, have a different structure, or have triggers.
	#[error("record store schema is incompatible")]
	IncompatibleSchema,
	/// Store metadata is missing or malformed.
	#[error("record store metadata is invalid")]
	InvalidMetadata,
	/// A selected row contains malformed field types or sizes.
	#[error("stored record fields are invalid")]
	InvalidRecord,
	/// A page cursor belongs to another store instance, namespace, range, or projection.
	#[error("record page cursor does not match the requested context")]
	CursorContextMismatch,
	/// The result could not reserve memory for its bounded output.
	#[error("record result allocation failed")]
	AllocationFailed,
	/// An input or returned field exceeds this view's access policy.
	#[error("record {field:?} size {actual_size} exceeds access limit {max_size}")]
	SizeLimitExceeded {
		/// The oversized field.
		field: RecordField,
		/// The field's size in bytes.
		actual_size: usize,
		/// The view's limit in bytes.
		max_size: usize,
	},
	/// This binary cannot interpret the physical format.
	#[error("unsupported record store format version {version}")]
	UnsupportedFormat {
		/// The physical format version in the store's metadata.
		version: i64,
	},
	/// Creating or mutating the store requires a write transaction.
	#[error("record mutation requires a write transaction")]
	WriteTransactionRequired,
	/// A zero namespace limit cannot admit any valid namespace.
	#[error("record namespace size limit must be positive")]
	InvalidNamespaceLimit,
	/// The platform's random source failed during store creation.
	#[error("record store instance ID generation failed: {0}")]
	Random(#[from] RandomError),
}

/// A store view bound to one transaction. Dropping the view does not complete
/// the transaction or remove its reserved-table protection.
pub struct RecordStore<'tx, 'connection> {
	transaction: &'tx mut Transaction<'connection>,
	instance_id: [u8; 32],
	limits: RecordAccessLimits,
}

impl<'tx, 'connection> RecordStore<'tx, 'connection> {
	/// Verifies or creates the store within the caller's transaction. Creation is
	/// provisional until that transaction commits. Partial or incompatible stores
	/// fail without adoption, reset, or deletion.
	pub fn open(
		transaction: &'tx mut Transaction<'connection>,
		options: RecordStoreOptions,
	) -> Result<Self, RecordError> {
		let instance_id = transaction.run_operation(|tx| {
			if options.limits.max_namespace_size == 0 {
				return Err(RecordError::InvalidNamespaceLimit);
			}
			tx.owner.record_authorization.protect();
			schema::open(tx, options.mode)
		})?;
		Ok(Self {
			transaction,
			instance_id,
			limits: options.limits,
		})
	}

	/// Borrows the same owned transaction for application SQL.
	pub fn transaction(&mut self) -> &mut Transaction<'connection> {
		self.transaction
	}

	/// Returns the persistent cursor-context ID. Physical database copies retain
	/// this ID; it is not a device identity, snapshot identity, or capability.
	pub fn instance_id(&self) -> &[u8; 32] {
		&self.instance_id
	}

	/// Returns this view's runtime access limits.
	pub fn limits(&self) -> RecordAccessLimits {
		self.limits
	}
}
