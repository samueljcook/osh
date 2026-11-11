//! Raw handles: byte values in the core Raw namespace or in one declared Raw scope.

use crate::kv::raw::engine::scope::PgNativeRawScope;
use crate::kv::raw::engine::store::{NativeRawKvError, PgNativeRawKvStore};
use crate::records::{RecordIdentity, RecordKey, RecordNamespace};
use crate::scope::ScopeIdentity;

mod encrypted;
mod error;
pub use encrypted::{
	PgEncryptedRaw, PgEncryptedRawError, PgEncryptedRawErrorKind, PgEncryptedRawFields,
	PgEncryptedRawMutationError, PgEncryptedRawMutationErrorKind,
};
mod maintenance;
mod many;
mod page;
mod point;
mod slot;
pub use error::{PgRawError, PgRawErrorKind};

/// The inputs of a handle for one declared Raw scope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PgRawFields {
	/// The declared Raw scope that the handle addresses.
	pub scope: ScopeIdentity,
}

enum RawHandleOwner<'scope> {
	Core(PgNativeRawKvStore),
	Scope(Box<PgNativeRawScope<'scope>>),
}

/// A handle for byte values in the installation's core Raw namespace or in one declared Raw
/// scope. Paranoid stores and returns the bytes without interpreting them.
///
/// Before each operation, Paranoid checks that the installation's recorded policy allows it.
/// In an app Raw scope, reads also need this runtime's declared reader role and writes need
/// the declared writer role. An operation on a pool runs in its own transaction. An operation
/// inside the caller's transaction becomes durable only when the caller commits. An empty
/// many-record or slot request returns at once without database work.
///
/// While a namespace is still in v10 storage, every key must be a valid v10 key.
pub struct PgRaw<'scope> {
	owner: RawHandleOwner<'scope>,
}

impl PgRaw<'static> {
	pub(crate) fn core(store: PgNativeRawKvStore) -> Self {
		Self {
			owner: RawHandleOwner::Core(store),
		}
	}
}

impl<'scope> PgRaw<'scope> {
	pub(crate) fn scoped(store: PgNativeRawScope<'scope>) -> Self {
		Self {
			owner: RawHandleOwner::Scope(Box::new(store)),
		}
	}

	/// The handle's namespace.
	pub fn namespace(&self) -> &RecordNamespace {
		match &self.owner {
			RawHandleOwner::Core(store) => store.namespace(),
			RawHandleOwner::Scope(store) => store.scope().namespace(),
		}
	}

	/// The handle's app scope, or `None` for the core Raw namespace.
	pub fn scope(&self) -> Option<&ScopeIdentity> {
		match &self.owner {
			RawHandleOwner::Core(_) => None,
			RawHandleOwner::Scope(store) => Some(store.scope()),
		}
	}

	fn selection_error(
		&self,
		source: error::RawOperationCause<std::convert::Infallible>,
	) -> PgRawError {
		PgRawError::selection(error::PgRawSelectionErrorFields {
			scope: self.scope(),
			namespace: self.namespace(),
			source,
		})
	}

	fn point_error<C>(&self, key: &RecordKey, source: NativeRawKvError<C>) -> PgRawError<C> {
		PgRawError::new(error::PgRawErrorFields {
			scope: self.scope(),
			record: RecordIdentity::new(self.namespace().clone(), key.clone()),
			source,
		})
	}
}

impl std::fmt::Debug for PgRaw<'_> {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter.debug_struct("PgRaw").finish_non_exhaustive()
	}
}
