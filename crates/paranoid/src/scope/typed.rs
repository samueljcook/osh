//! Scopes for records that hold app values of a known type.
//!
//! An app builds one value per scope, declares the scope from that value, and builds the
//! scope's handles from the same value. A handle uses the code that the runtime's
//! installation declares for the scope, so the code in a handle always equals the declared
//! code.

use std::fmt;
use std::sync::Arc;

use crate::kv::document::schema::owned::DocumentCode;
use crate::records::state_schema::StateSchema;

use super::ScopeIdentity;

/// The inputs of a document scope.
pub struct DocumentScopeFields<T, C, P> {
	/// The scope's identity.
	pub identity: ScopeIdentity,
	/// The document code: the record codecs, the indexes, and the index projectors.
	pub code: Arc<DocumentCode<T, C, P>>,
}

/// One scope of documents of type `T`.
pub struct DocumentScope<T, C, P> {
	identity: ScopeIdentity,
	code: Arc<DocumentCode<T, C, P>>,
}

impl<T, C, P> DocumentScope<T, C, P> {
	/// Builds a document scope without database work.
	pub fn new(fields: DocumentScopeFields<T, C, P>) -> Self {
		Self {
			identity: fields.identity,
			code: fields.code,
		}
	}
	/// The scope's identity.
	pub fn identity(&self) -> &ScopeIdentity {
		&self.identity
	}
	pub(crate) fn code(&self) -> &Arc<DocumentCode<T, C, P>> {
		&self.code
	}
}

impl<T, C, P> Clone for DocumentScope<T, C, P> {
	fn clone(&self) -> Self {
		Self {
			identity: self.identity.clone(),
			code: Arc::clone(&self.code),
		}
	}
}

impl<T, C, P> fmt::Debug for DocumentScope<T, C, P> {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("DocumentScope")
			.field("identity", &self.identity)
			.finish_non_exhaustive()
	}
}

/// The inputs of a cache scope or a Topic scope.
pub struct PayloadScopeFields<T, C> {
	/// The scope's identity.
	pub identity: ScopeIdentity,
	/// The codecs of the scope's payload values.
	pub schema: Arc<StateSchema<T, C>>,
}

macro_rules! payload_scope {
	($(#[$doc:meta])* $name:ident) => {
		$(#[$doc])*
		pub struct $name<T, C> {
			identity: ScopeIdentity,
			schema: Arc<StateSchema<T, C>>,
		}

		impl<T, C> $name<T, C> {
			/// Builds the scope without database work.
			pub fn new(fields: PayloadScopeFields<T, C>) -> Self {
				Self {
					identity: fields.identity,
					schema: fields.schema,
				}
			}
			/// The scope's identity.
			pub fn identity(&self) -> &ScopeIdentity {
				&self.identity
			}
			pub(crate) fn schema(&self) -> &Arc<StateSchema<T, C>> {
				&self.schema
			}
		}

		impl<T, C> Clone for $name<T, C> {
			fn clone(&self) -> Self {
				Self {
					identity: self.identity.clone(),
					schema: Arc::clone(&self.schema),
				}
			}
		}

		impl<T, C> fmt::Debug for $name<T, C> {
			fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
				formatter
					.debug_struct(stringify!($name))
					.field("identity", &self.identity)
					.finish_non_exhaustive()
			}
		}
	};
}

payload_scope!(
	/// One scope of coalescing cache values of type `T`.
	CacheScope
);
payload_scope!(
	/// One scope of Topic events with payloads of type `T`.
	TopicScope
);
