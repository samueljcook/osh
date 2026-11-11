//! Byte identities and key ranges without a database-driver dependency.

mod identity;
pub use identity::{RecordIdentity, RecordIdentityError, RecordKey, RecordNamespace};

#[cfg(feature = "record-key-range")]
mod key_range;
#[cfg(feature = "record-key-range")]
pub use key_range::{RecordKeyRange, RecordKeyRangeBounds, RecordKeyRangeError};
