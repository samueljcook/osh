//! Public lease operations of the native runtime.

use std::error::Error;
use std::fmt;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use crate::db::{Database, TransactionFailure};
use crate::fleet::RandomnessError;
use crate::fleet::lease::CoordinationKeyError;
use crate::fleet::lease::owned_driver::NativeLeaseDriverError;
use crate::fleet::lease::owned_driver::acquire::NativeLeaseAcquireError;
use crate::fleet::lease::owned_driver::close::{
	NativeLeaseCloseWaitError, NativeLeaseCloseWaitResult, NativeLeaseReleaseAccounting,
};
use crate::fleet::lease::owned_driver::loss::{
	NativeLeaseAuthorityObservation, NativeLeaseCallerCtx, NativeLeaseLossObserver,
};
use crate::fleet::lease::owned_driver::options::{
	NativeLeaseOwnerOptions, NativeLeaseOwnerOptionsError, NativeLeaseOwnerTiming,
	NativeLeaseRenewalPolicyFields, NativeLeaseWaitingPolicy, NativeLeaseWaitingPolicyError,
	NativeLeaseWaitingPolicyFields,
};
use crate::fleet::lease::{Claim, HolderSnapshot, NativeLeaseError, PgNativeLease};
use crate::fleet::{ClaimDuration, FencingToken, HolderId, HolderPrefix, LeaseKey};
use crate::runtime::local_admission::RuntimeAdmissionError;

/// The inputs of a lease handle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PgLeaseFields {
	/// The lease's key.
	pub key: LeaseKey,
}

/// One lease of the runtime's installation: at most one holder at a time, until its claim
/// expires or is released.
///
/// Acquisition, renewal, and release each run in their own transaction on the runtime's control
/// pool, and a claim is returned only after its transaction is known to have committed.
///
/// Nothing renews a claim of this handle. Renew it with `try_renew` before
/// [`PgLeaseClaim::usable_until`], and stop the work that the claim protects when that time
/// passes.
pub struct PgLease {
	native: PgNativeLease,
	holder_prefix: HolderPrefix,
}

impl PgLease {
	pub(crate) fn new(native: PgNativeLease, holder_prefix: HolderPrefix) -> Self {
		Self {
			native,
			holder_prefix,
		}
	}

	/// The lease's key.
	pub fn key(&self) -> &LeaseKey {
		self.native.key()
	}

	/// Reports the current live holder, if any. The report grants nothing.
	pub async fn fetch_live_holder(
		&self,
		database: impl Database,
	) -> Result<Option<PgLeaseHolder>, PgLeaseError> {
		self.native
			.fetch_holder(database)
			.await
			.map(|holder| holder.map(PgLeaseHolder))
			.map_err(PgLeaseError::new)
	}

	/// Claims the lease for `duration` when no live claim holds it. The claim's holder label
	/// starts with the runtime's holder prefix.
	/// During v10 coexistence, contention on the source acquisition locks also returns `None`.
	pub async fn try_acquire(
		&self,
		duration: ClaimDuration,
	) -> Result<Option<PgLeaseClaim>, PgLeaseError> {
		let requested = Instant::now();
		self.native
			.try_acquire(&self.holder_prefix, duration)
			.await
			.map(|claim| claim.map(|claim| PgLeaseClaim::new(claim, requested, duration)))
			.map_err(PgLeaseError::new)
	}

	/// Extends a live claim by `duration` from now and returns its replacement, which keeps the
	/// claim's fencing token. `None` means that `claim` no longer holds the lease.
	pub async fn try_renew(
		&self,
		claim: &PgLeaseClaim,
		duration: ClaimDuration,
	) -> Result<Option<PgLeaseClaim>, PgLeaseError> {
		let requested = Instant::now();
		self.native
			.try_renew(&claim.claim, duration)
			.await
			.map(|claim| claim.map(|claim| PgLeaseClaim::new(claim, requested, duration)))
			.map_err(PgLeaseError::new)
	}

	/// Releases a live claim. `false` means that `claim` no longer held the lease.
	pub async fn release(&self, claim: &PgLeaseClaim) -> Result<bool, PgLeaseError> {
		self.native
			.release(&claim.claim)
			.await
			.map_err(PgLeaseError::new)
	}
}

/// A live claim of a lease. It is the private capability that renews or releases the claim, so
/// it cannot be copied.
#[derive(Debug)]
pub struct PgLeaseClaim {
	claim: Claim,
	usable_until: Instant,
}

impl PgLeaseClaim {
	fn new(claim: Claim, requested: Instant, duration: ClaimDuration) -> Self {
		Self {
			claim,
			usable_until: requested
				.checked_add(duration.as_duration())
				.expect("a validated claim duration fits the local monotonic clock"),
		}
	}

	/// The lease's key.
	pub fn key(&self) -> &LeaseKey {
		self.claim.key()
	}

	/// The claim's observable holder label, which grants nothing.
	pub fn holder_id(&self) -> &HolderId {
		self.claim.holder_id()
	}

	/// The claim's fencing token.
	pub fn fencing_token(&self) -> FencingToken {
		self.claim.fencing_token()
	}

	/// When the claim expires, in Unix microseconds by the database clock.
	pub fn expires_at_unix_micros(&self) -> i64 {
		self.claim.expires_at_unix_micros()
	}

	/// The local time until which the claim is safe to use: the moment just before the
	/// acquisition or renewal that returned the claim began, plus the claim's duration. The
	/// database computes the claim's expiry later than that moment, so the claim is live at
	/// least until then, unless the database's clock runs faster than the local clock.
	pub fn usable_until(&self) -> Instant {
		self.usable_until
	}
}

/// A non-secret view of a lease's current live holder.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PgLeaseHolder(HolderSnapshot);

impl PgLeaseHolder {
	pub(crate) fn new(holder: HolderSnapshot) -> Self {
		Self(holder)
	}

	/// The lease's key.
	pub fn key(&self) -> &LeaseKey {
		self.0.key()
	}

	/// The holder's observable label.
	pub fn holder_id(&self) -> &HolderId {
		self.0.holder_id()
	}

	/// The holder's fencing token.
	pub fn fencing_token(&self) -> FencingToken {
		self.0.fencing_token()
	}

	/// When the holder's claim expires, in Unix microseconds by the database clock.
	pub fn expires_at_unix_micros(&self) -> i64 {
		self.0.expires_at_unix_micros()
	}
}

/// The inputs of the options of a run that holds a lease while its task runs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PgLeaseRunOptionsFields {
	/// The duration of each claim and of each renewal.
	pub claim_duration: ClaimDuration,
	/// The positive interval between automatic renewals, which must be shorter than
	/// `claim_duration`.
	pub heartbeat_interval: Duration,
	/// The number of consecutive renewals that fail with a retryable database failure before
	/// they send `COMMIT`, after which the run gives up the lease. The run retries such a renewal
	/// only while its claim is still usable. Any other renewal failure, such as a renewal that
	/// exceeds Paranoid's time limit, returns no claim, or fails while committing, ends the run's
	/// hold on the lease at once.
	pub max_consecutive_renewal_failures: NonZeroU32,
	/// The positive delay before a waiting run's second acquisition attempt.
	pub acquire_retry_interval: Duration,
	/// The largest delay between a waiting run's acquisition attempts, which must be at least
	/// `acquire_retry_interval`. Each delay doubles until it reaches this one, and each delay
	/// is jittered.
	pub max_acquire_retry_interval: Duration,
}

/// Validated options of a run that holds a lease while its task runs. Two options are equal
/// when their fields are equal.
#[derive(Clone)]
pub struct PgLeaseRunOptions {
	// Boxed so that a worker's cleanup choice, which holds these options, stays small.
	fields: Box<PgLeaseRunOptionsFields>,
	pub(super) timing: NativeLeaseOwnerTiming,
	pub(in crate::fleet) waiting: NativeLeaseWaitingPolicy,
}

impl fmt::Debug for PgLeaseRunOptions {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("PgLeaseRunOptions")
			.field("fields", &self.fields)
			.finish()
	}
}

impl PartialEq for PgLeaseRunOptions {
	fn eq(&self, other: &Self) -> bool {
		self.fields == other.fields
	}
}

impl Eq for PgLeaseRunOptions {}

impl PgLeaseRunOptions {
	/// Validates the options without database work.
	pub fn new(fields: PgLeaseRunOptionsFields) -> Result<Self, PgLeaseRunOptionsError> {
		let timing = NativeLeaseOwnerTiming::parse(
			fields.claim_duration,
			NativeLeaseRenewalPolicyFields::Automatic {
				heartbeat_interval: fields.heartbeat_interval,
				max_consecutive_renewal_failures: fields.max_consecutive_renewal_failures.get(),
			},
		)
		.map_err(|error| PgLeaseRunOptionsError::new(PgLeaseRunOptionsCause::Lease(error)))?;
		let waiting = NativeLeaseWaitingPolicy::new(NativeLeaseWaitingPolicyFields {
			acquire_retry_interval: fields.acquire_retry_interval,
			max_acquire_retry_interval: fields.max_acquire_retry_interval,
		})
		.map_err(|error| PgLeaseRunOptionsError::new(PgLeaseRunOptionsCause::Waiting(error)))?;
		Ok(Self {
			fields: Box::new(fields),
			timing,
			waiting,
		})
	}

	/// The fields that the options were validated from.
	pub fn fields(&self) -> PgLeaseRunOptionsFields {
		*self.fields
	}

	pub(in crate::fleet) fn owner_options(
		&self,
		holder_prefix: &HolderPrefix,
	) -> NativeLeaseOwnerOptions {
		self.timing.with_holder_prefix(holder_prefix.clone())
	}
}

enum PgLeaseRunOptionsCause {
	Lease(NativeLeaseOwnerOptionsError),
	Waiting(NativeLeaseWaitingPolicyError),
}

/// Invalid lease run options.
pub struct PgLeaseRunOptionsError {
	source: Box<PgLeaseRunOptionsCause>,
}

impl PgLeaseRunOptionsError {
	fn new(source: PgLeaseRunOptionsCause) -> Self {
		Self {
			source: Box::new(source),
		}
	}

	fn source_error(&self) -> &(dyn Error + 'static) {
		match self.source.as_ref() {
			PgLeaseRunOptionsCause::Lease(source) => source,
			PgLeaseRunOptionsCause::Waiting(source) => source,
		}
	}
}

impl fmt::Display for PgLeaseRunOptionsError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str("invalid lease run options")
	}
}

impl fmt::Debug for PgLeaseRunOptionsError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(
			formatter,
			"PgLeaseRunOptionsError: {self}: {}",
			self.source_error()
		)
	}
}

impl Error for PgLeaseRunOptionsError {
	fn source(&self) -> Option<&(dyn Error + 'static)> {
		Some(self.source_error())
	}
}

/// What a task receives while its run holds a lease that Paranoid renews automatically.
pub struct PgLeaseTaskCtx {
	holder: PgLeaseHolder,
	loss: NativeLeaseLossObserver,
}

impl From<NativeLeaseCallerCtx> for PgLeaseTaskCtx {
	fn from(ctx: NativeLeaseCallerCtx) -> Self {
		Self {
			holder: PgLeaseHolder(ctx.holder),
			loss: ctx.loss,
		}
	}
}

impl PgLeaseTaskCtx {
	/// The run's lease holder when the task started.
	pub fn holder(&self) -> &PgLeaseHolder {
		&self.holder
	}

	/// Reports whether the run still holds the lease, by the local record of its last confirmed
	/// claim or renewal. The report performs no database work.
	pub fn leadership(&self) -> PgLeadership {
		PgLeadership::from_observation(&self.loss.observe())
	}

	/// Waits until the run no longer holds the lease, for example because its renewals failed,
	/// its runtime stopped, or its last claim's bound passed, and returns at once when the run
	/// has already lost it. The wait performs no database work. A task that must stop when its
	/// run loses the lease can race its work against this wait.
	pub async fn wait_until_leadership_lost(&self) {
		self.loss.clone().wait_until_lost().await;
	}
}

/// Whether a run holds its lease.
///
/// No other holder acquires the lease before the `usable_until` of the run's last `Held` report
/// passes. A run that loses the lease, for example because its renewals failed, releases its
/// claim only after the task returns, so another holder can acquire the lease while the task
/// runs only once the claim expires.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PgLeadership {
	/// The run holds the lease, and its claim is safe to use until `usable_until`.
	Held {
		/// The local time until which the last confirmed claim or renewal is safe to use. The
		/// time is the earlier of the claim's bound and the bound of the runtime's
		/// participation.
		usable_until: Instant,
	},
	/// The run no longer holds the lease and renews its claim no more. Another holder can
	/// acquire the lease once the task returns or the claim expires, which happens no earlier
	/// than the `usable_until` of the run's last `Held` report.
	Lost,
}

impl PgLeadership {
	pub(super) fn from_observation(observation: &NativeLeaseAuthorityObservation) -> Self {
		match observation {
			NativeLeaseAuthorityObservation::ConfirmedLocally { usable_until, .. } => Self::Held {
				usable_until: *usable_until,
			},
			NativeLeaseAuthorityObservation::Lost { .. } => Self::Lost,
		}
	}
}

/// Whether a run confirmed the release of its lease. A run waits as long as one release attempt
/// can take; a release that needs more attempts continues after the run returns.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PgLeaseRelease {
	/// The run released the lease.
	Released,
	/// The run's claim no longer held the lease when the run released it.
	NotHeld,
	/// The run did not confirm the release. The lease is free no later than when the run's
	/// last claim expires.
	Unconfirmed,
}

impl PgLeaseRelease {
	pub(in crate::fleet) fn from_close_wait(
		wait: &Result<NativeLeaseCloseWaitResult, NativeLeaseCloseWaitError>,
	) -> Self {
		match wait {
			Ok(NativeLeaseCloseWaitResult::Settled(snapshot)) => match snapshot.driver.release {
				NativeLeaseReleaseAccounting::Released => Self::Released,
				NativeLeaseReleaseAccounting::NoMatch => Self::NotHeld,
				NativeLeaseReleaseAccounting::Pending
				| NativeLeaseReleaseAccounting::NotAcquired
				| NativeLeaseReleaseAccounting::Unresolved => Self::Unconfirmed,
			},
			Ok(NativeLeaseCloseWaitResult::Pending) | Err(_) => Self::Unconfirmed,
		}
	}
}

pub(in crate::fleet) enum PgLeaseRunCause {
	Start(Arc<NativeLeaseDriverError>),
	Caller(Arc<NativeLeaseDriverError>),
	Randomness(RandomnessError),
	LocalClockDomain,
}

/// The class of a failed lease run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PgLeaseRunErrorKind {
	/// A database statement or the transaction's own control failed. The error's source chain
	/// holds the database error.
	Database,
	/// The runtime stopped accepting work.
	RuntimeStopped,
	/// The run lost the lease before its task started.
	LeadershipLost,
	/// A local failure that the app cannot correct, such as a failed random source or a
	/// duration outside the local monotonic clock domain.
	Internal,
}

/// A run that holds a lease failed before its task ran, with its complete cause chain.
pub struct PgLeaseRunError {
	source: Box<PgLeaseRunCause>,
}

impl PgLeaseRunError {
	pub(in crate::fleet) fn new(source: PgLeaseRunCause) -> Self {
		Self {
			source: Box::new(source),
		}
	}

	pub(in crate::fleet) fn from_acquire(source: NativeLeaseAcquireError) -> Self {
		Self::new(match source {
			NativeLeaseAcquireError::Start { source, .. } => PgLeaseRunCause::Start(source),
			NativeLeaseAcquireError::Randomness(source) => PgLeaseRunCause::Randomness(source),
			NativeLeaseAcquireError::LocalClockDomain => PgLeaseRunCause::LocalClockDomain,
		})
	}

	/// The class of the failure.
	pub fn kind(&self) -> PgLeaseRunErrorKind {
		match self.source.as_ref() {
			PgLeaseRunCause::Start(source) | PgLeaseRunCause::Caller(source) => {
				driver_error_kind(source)
			}
			PgLeaseRunCause::Randomness(_) | PgLeaseRunCause::LocalClockDomain => {
				PgLeaseRunErrorKind::Internal
			}
		}
	}

	fn source_error(&self) -> Option<&(dyn Error + 'static)> {
		match self.source.as_ref() {
			PgLeaseRunCause::Start(source) | PgLeaseRunCause::Caller(source) => {
				Some(source.as_ref())
			}
			PgLeaseRunCause::Randomness(source) => Some(source),
			PgLeaseRunCause::LocalClockDomain => None,
		}
	}
}

pub(super) fn driver_error_kind(source: &NativeLeaseDriverError) -> PgLeaseRunErrorKind {
	if source.involves_database_failure() {
		return PgLeaseRunErrorKind::Database;
	}
	match source {
		NativeLeaseDriverError::DeliveryClosed
		| NativeLeaseDriverError::Local(
			RuntimeAdmissionError::Stopped { .. } | RuntimeAdmissionError::LifetimeFinished,
		) => PgLeaseRunErrorKind::RuntimeStopped,
		NativeLeaseDriverError::AuthorityLost => PgLeaseRunErrorKind::LeadershipLost,
		source if runtime_stopped_in_chain(source) => PgLeaseRunErrorKind::RuntimeStopped,
		_ => PgLeaseRunErrorKind::Internal,
	}
}

/// Whether `error`'s source chain holds the runtime's refusal of new work after it stopped.
pub(in crate::fleet) fn runtime_stopped_in_chain(error: &(dyn Error + 'static)) -> bool {
	let mut current = Some(error);
	while let Some(error) = current {
		if matches!(
			error.downcast_ref::<RuntimeAdmissionError>(),
			Some(RuntimeAdmissionError::Stopped { .. } | RuntimeAdmissionError::LifetimeFinished)
		) {
			return true;
		}
		current = error.source();
	}
	false
}

impl fmt::Display for PgLeaseRunError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self.source_error() {
			Some(_) => formatter.write_str("lease run failed before its task"),
			None => formatter.write_str(
				"lease run failed before its task: its timing exceeds the local clock domain",
			),
		}
	}
}

impl fmt::Debug for PgLeaseRunError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(formatter, "PgLeaseRunError: {self}")?;
		let mut source = self.source();
		while let Some(error) = source {
			write!(formatter, ": {error}")?;
			source = error.source();
		}
		Ok(())
	}
}

impl Error for PgLeaseRunError {
	fn source(&self) -> Option<&(dyn Error + 'static)> {
		self.source_error()
	}
}

/// The class of a failure of a Once, cache, or Topic handle, found by walking its source chain
/// from the outermost cause. Only the chain of a Once or cache failure holds a lease driver error,
/// which alone gives the classes `Timeout` and `LeadershipLost`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::fleet) enum HandleFailureClass {
	Database,
	Timeout,
	RuntimeStopped,
	LeadershipLost,
	Admission,
	State,
	Internal,
}

pub(in crate::fleet) fn classify_handle_failure(
	error: &(dyn Error + 'static),
) -> HandleFailureClass {
	let mut current = Some(error);
	while let Some(error) = current {
		if error.downcast_ref::<crate::db::DatabaseError>().is_some() {
			return HandleFailureClass::Database;
		}
		current = error.source();
	}
	if runtime_stopped_in_chain(error) {
		return HandleFailureClass::RuntimeStopped;
	}
	let mut current = Some(error);
	while let Some(error) = current {
		if let Some(driver) = error.downcast_ref::<NativeLeaseDriverError>() {
			if matches!(driver, NativeLeaseDriverError::Timeout(_)) {
				return HandleFailureClass::Timeout;
			}
			match driver_error_kind(driver) {
				PgLeaseRunErrorKind::Database => return HandleFailureClass::Database,
				PgLeaseRunErrorKind::RuntimeStopped => return HandleFailureClass::RuntimeStopped,
				PgLeaseRunErrorKind::LeadershipLost => return HandleFailureClass::LeadershipLost,
				PgLeaseRunErrorKind::Internal => {}
			}
		}
		if error
			.downcast_ref::<crate::runtime::admission::NativeAdmissionError>()
			.is_some()
		{
			return HandleFailureClass::Admission;
		}
		if error.downcast_ref::<RandomnessError>().is_some() {
			return HandleFailureClass::Internal;
		}
		current = error.source();
	}
	HandleFailureClass::State
}

/// A key that a handle cannot turn into its lease key. The source holds the key error.
#[derive(Debug)]
pub struct PgLeaseKeyError {
	source: CoordinationKeyError,
}

impl PgLeaseKeyError {
	pub(crate) fn new(source: CoordinationKeyError) -> Self {
		Self { source }
	}
}

impl fmt::Display for PgLeaseKeyError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str("the lease key is invalid")
	}
}

impl Error for PgLeaseKeyError {
	fn source(&self) -> Option<&(dyn Error + 'static)> {
		Some(&self.source)
	}
}

/// The class of a failed lease operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PgLeaseErrorKind {
	/// A database statement or the transaction's own control failed. The error's source chain
	/// holds the database error.
	Database,
	/// The runtime stopped accepting work.
	RuntimeStopped,
	/// The runtime cannot operate on the lease under the installation's recorded policy.
	Admission,
	/// The v10 lease table refused or failed the operation while v10 deployments still write.
	V10,
	/// The stored lease could not be interpreted or changed.
	State,
	/// A local failure that the app cannot correct, such as a failed random source.
	Internal,
}

fn lease_error_kind(error: &NativeLeaseError) -> PgLeaseErrorKind {
	if error.involves_database_failure() {
		return PgLeaseErrorKind::Database;
	}
	if runtime_stopped_in_chain(error) {
		return PgLeaseErrorKind::RuntimeStopped;
	}
	match error {
		NativeLeaseError::Registered(_)
		| NativeLeaseError::SourceMappingAbsent
		| NativeLeaseError::StorageMappingMismatch
		| NativeLeaseError::NativePlacementDuringCoexistence
		| NativeLeaseError::Admission(_)
		| NativeLeaseError::Binding(_)
		| NativeLeaseError::Protocol(_) => PgLeaseErrorKind::Admission,
		NativeLeaseError::SourceBinding(_) | NativeLeaseError::SourceExecution(_) => {
			PgLeaseErrorKind::V10
		}
		NativeLeaseError::Read(_)
		| NativeLeaseError::Mutation(_)
		| NativeLeaseError::Storage(_) => PgLeaseErrorKind::State,
		NativeLeaseError::Randomness(_) => PgLeaseErrorKind::Internal,
		NativeLeaseError::Database(_) => PgLeaseErrorKind::Database,
		NativeLeaseError::RollbackFailed {
			operation_error, ..
		} => lease_error_kind(operation_error),
	}
}

/// A failed lease operation, with its complete cause chain.
pub struct PgLeaseError {
	source: Box<NativeLeaseError>,
}

impl PgLeaseError {
	pub(crate) fn new(source: NativeLeaseError) -> Self {
		Self {
			source: Box::new(source),
		}
	}

	/// The class of the failure.
	pub fn kind(&self) -> PgLeaseErrorKind {
		lease_error_kind(&self.source)
	}

	/// Whether the operation's own `COMMIT` failed, which means that the change may or may not
	/// have committed. An acquisition or renewal with an unknown outcome returns no claim. If
	/// such an acquisition or renewal committed, its undelivered claim holds the lease until the
	/// claim expires: after a renewal, up to the renewal's `duration` after the renewal began. The
	/// `claim` that was passed to `try_renew` can then no longer renew or release the lease.
	pub fn commit_outcome_is_unknown(&self) -> bool {
		self.source.commit_outcome_is_unknown()
	}
}

impl fmt::Display for PgLeaseError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str("lease operation failed")
	}
}

impl fmt::Debug for PgLeaseError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(formatter, "PgLeaseError: {self}")?;
		let mut source = self.source();
		while let Some(error) = source {
			write!(formatter, ": {error}")?;
			source = error.source();
		}
		Ok(())
	}
}

impl Error for PgLeaseError {
	fn source(&self) -> Option<&(dyn Error + 'static)> {
		Some(self.source.as_ref())
	}
}
