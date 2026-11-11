use std::error::Error;
use std::fmt;

/// The step of the harness's lifecycle that failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PgTestHarnessPhase {
	/// The configuration is outside what the harness accepts.
	Config,
	/// The process environment rejects the harness: the local container runtime has no address
	/// or does not answer, the platform's random source fails, an environment variable that the
	/// harness rejects is set, or leftover recovery exceeds the startup deadline.
	Environment,
	/// A loopback TCP port could not be reserved.
	PortReserve,
	/// A loopback TCP port reservation could not be released.
	PortRelease,
	/// The PostgreSQL container did not start or did not become ready.
	PgStart,
	/// The harness's database roles could not be provisioned.
	PgRoles,
	/// A PgBouncer container did not start or did not become ready.
	PgBouncerStart,
	/// A PgBouncer does not report the configured version, its pooling mode, the bounds, and the
	/// prepared statement limit.
	PgBouncerVerify,
	/// A connection to PostgreSQL or to a PgBouncer's admin console could not be opened.
	ClientConnect,
	/// A statement that the harness ran on one of its own connections failed.
	ClientQuery,
	/// A connection that the harness opened could not be closed.
	ClientClose,
	/// A database could not be created.
	DatabaseCreate,
	/// A database could not be removed.
	DatabaseRemove,
	/// A PgBouncer container could not be stopped or removed.
	PgBouncerStop,
	/// The PostgreSQL container could not be stopped or removed.
	PgStop,
}

impl fmt::Display for PgTestHarnessPhase {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str(match self {
			Self::Config => "configuration",
			Self::Environment => "environment",
			Self::PortReserve => "port-reserve",
			Self::PortRelease => "port-release",
			Self::PgStart => "pg-start",
			Self::PgRoles => "pg-roles",
			Self::PgBouncerStart => "pgbouncer-start",
			Self::PgBouncerVerify => "pgbouncer-verify",
			Self::ClientConnect => "client-connect",
			Self::ClientQuery => "client-query",
			Self::ClientClose => "client-close",
			Self::DatabaseCreate => "database-create",
			Self::DatabaseRemove => "database-remove",
			Self::PgBouncerStop => "pgbouncer-stop",
			Self::PgStop => "pg-stop",
		})
	}
}

/// A failure of the test harness, with the phase that failed.
///
/// When a failure during startup is followed by failures while the harness removes what had
/// started, [`cleanup_failures`](Self::cleanup_failures) lists them.
#[derive(Debug)]
pub struct PgTestHarnessError {
	phase: PgTestHarnessPhase,
	reason: String,
	source: Option<Box<dyn Error + Send + Sync>>,
	cleanup_failures: Vec<PgTestHarnessError>,
}

impl PgTestHarnessError {
	pub(crate) fn new(phase: PgTestHarnessPhase, reason: impl Into<String>) -> Self {
		Self {
			phase,
			reason: reason.into(),
			source: None,
			cleanup_failures: Vec::new(),
		}
	}

	pub(crate) fn with_source(
		phase: PgTestHarnessPhase,
		reason: impl Into<String>,
		source: impl Error + Send + Sync + 'static,
	) -> Self {
		Self {
			source: Some(Box::new(source)),
			..Self::new(phase, reason)
		}
	}

	/// Appends `detail` to the reason.
	pub(crate) fn with_detail(mut self, detail: &str) -> Self {
		self.reason.push_str(detail);
		self
	}

	pub(crate) fn with_cleanup_failures(mut self, cleanup_failures: Vec<Self>) -> Self {
		self.cleanup_failures = cleanup_failures;
		self
	}

	/// The step that failed.
	pub fn phase(&self) -> PgTestHarnessPhase {
		self.phase
	}

	/// A description of what failed, without the underlying error.
	pub fn reason(&self) -> &str {
		&self.reason
	}

	/// The failures that occurred while the harness removed what had started after this failure.
	pub fn cleanup_failures(&self) -> &[PgTestHarnessError] {
		&self.cleanup_failures
	}
}

impl fmt::Display for PgTestHarnessError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(formatter, "test harness {}: {}", self.phase, self.reason)?;
		if !self.cleanup_failures.is_empty() {
			write!(
				formatter,
				" ({} cleanup failures follow)",
				self.cleanup_failures.len()
			)?;
		}
		Ok(())
	}
}

impl Error for PgTestHarnessError {
	fn source(&self) -> Option<&(dyn Error + 'static)> {
		self.source
			.as_deref()
			.map(|source| source as &(dyn Error + 'static))
	}
}

/// The failures of [`PgTestHarness::shutdown`](crate::PgTestHarness::shutdown).
///
/// Shutdown attempts every step even when one fails, so this lists each failure.
#[derive(Debug, thiserror::Error)]
#[error("test harness shutdown failed in {} steps; the first: {}", .failures.len(), .failures[0])]
pub struct PgTestHarnessShutdownError {
	pub(crate) failures: Vec<PgTestHarnessError>,
}

impl PgTestHarnessShutdownError {
	/// Returns `None` for an empty list, which is a successful shutdown.
	pub(crate) fn from_failures(failures: Vec<PgTestHarnessError>) -> Option<Self> {
		(!failures.is_empty()).then_some(Self { failures })
	}

	/// Every failure, in the order the steps ran. The list is never empty.
	pub fn failures(&self) -> &[PgTestHarnessError] {
		&self.failures
	}
}
