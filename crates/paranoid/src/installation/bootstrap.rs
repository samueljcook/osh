use crate::db::DatabaseError;

/// Taking the installation lock failed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum BootstrapError {
	/// A database operation failed.
	#[error("database work failed")]
	Database(#[from] DatabaseError),
}
