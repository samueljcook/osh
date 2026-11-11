//! Parsed comparison facts and sanitized physical-definition failures.

use std::num::NonZeroU32;

use crate::db::DatabaseError;
use crate::kv::document::schema::DocumentDeclarationError;

use super::super::PgDocumentIndexError;
use super::grammar::PgDocumentIndexExpressionError;

/// One actual server report. This value grants no registration or control authority.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PgIndexKeyCapacity(NonZeroU32);
impl PgIndexKeyCapacity {
	pub(super) fn from_server_report(value: i32) -> Result<Self, PgDocumentIndexPhysicalError> {
		u32::try_from(value)
			.ok()
			.and_then(NonZeroU32::new)
			.map(Self)
			.ok_or(PgDocumentIndexPhysicalError::InvalidKeyCapacity)
	}
	pub(super) fn require_count(&self, count: usize) -> Result<(), PgDocumentIndexPhysicalError> {
		if count > self.0.get() as usize {
			return Err(PgDocumentIndexPhysicalError::KeyCapacity {
				required: count,
				maximum: self.0.get(),
			});
		}
		Ok(())
	}
}

/// Inspection is comparison only; it grants no DDL, coverage or readiness permission.
#[derive(Debug)]
pub(crate) enum PgDocumentIndexCatalogObservation {
	Absent { capacity: PgIndexKeyCapacity },
	Present,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PgDocumentIndexCatalogMismatch {
	ParentAbsent,
	ParentShape,
	BuiltinFunctions,
	OccupiedName,
	IndexState,
	ParentAssociation,
	AttrCount,
	ExpressionPositions,
	AttrType,
	OperatorClass,
	Collation,
	Ordering,
	Dependencies,
	AttrOptions,
	PredicateAbsent,
	CatalogTransport,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum PgDocumentIndexPhysicalError {
	#[error("document index expression derivation failed")]
	Expression(#[from] PgDocumentIndexError),
	#[error("document index projection-key derivation failed")]
	Declaration(#[from] DocumentDeclarationError),
	#[error("document index server key capacity is invalid")]
	InvalidKeyCapacity,
	#[error("document index requires {required} keys but this server supports {maximum}")]
	KeyCapacity { required: usize, maximum: u32 },
	#[error("document index field count cannot include its final key")]
	KeyCountOverflow,
	#[error("document index catalog differs from its recorded definition: {issue:?}")]
	Catalog {
		issue: PgDocumentIndexCatalogMismatch,
	},
	#[error("document index catalog expression is not the declared structure")]
	CatalogExpression {
		field_index: Option<usize>,
		#[source]
		source: PgDocumentIndexExpressionError,
	},
	#[error("document index catalog inspection database request failed")]
	Database(#[source] DatabaseError),
}
impl PgDocumentIndexPhysicalError {
	pub(crate) fn database_error(&self) -> Option<&DatabaseError> {
		match self {
			Self::Database(error) => Some(error),
			_ => None,
		}
	}
}
