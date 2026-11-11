//! Detached inspection of an existing installation.

use std::sync::Arc;

use primitives_rs::id::time_sortable::SortableId;

use super::RecordedInstallationIdentity;
use super::administration::{PgNativeAdministration, PgNativeAdministrationFields};
use super::creation::v10::recorded_source::PgRecordedImportedInstallation;
use super::creation::{
	InspectedInstallation, NativeInstallationError, PgNativeInstallation, inspect_installation,
};
use super::layout::PgStorageLayout;
use crate::db::{Database, DatabaseError, PgSchemaName, Pool, run_transaction_read};
use crate::fleet::counter::engine::{NativeCounterBindings, PgNativeCounter};
use crate::fleet::counter::state::CounterComponent;
use crate::fleet::lease::engine::NativeLeaseReadBindings;
use crate::fleet::lease::{
	LeaseComponent, LeaseSourceBridge, NativeLeaseError, NativeLeaseSourceBindings,
};
use crate::fleet::{CounterKey, LeaseKey, PgCounterError, PgLeaseError, PgLeaseHolder};
use crate::queue::engine::{NativeQueueBindings, NativeQueueGroupError, PgNativeQueueGroup};
use crate::queue::{
	PgQueueReader, QueueAuxiliaryComponent, QueueJobComponent, QueueSourceImportDefs,
};
use crate::records::{RecordIdentity, RecordKey};
use crate::runtime::admission::bindings::PgInstallationAdmissionBinding;
use crate::runtime::admission::observation::{PgNativeObservation, PgOperationAdmission};
use crate::runtime::parameters;
use crate::runtime::participation::state::ParticipantSchema;
use crate::runtime::v10::{
	NativeSourcePreparationError, NativeSourcePreparationFields, RuntimeSourceAdapters,
};

/// Inputs for opening an existing installation without registering a participant.
pub struct PgInstallationObserverFields<'a> {
	/// A neutral pool. Read-only PostgreSQL credentials are sufficient.
	pub pool: &'a Pool,
	/// The PostgreSQL schema that contains the installation.
	pub schema: PgSchemaName,
	/// Whether to discover the installation ID or require an exact ID.
	pub identity: RecordedInstallationIdentity,
}

/// Read-only Queue and Fleet inspection without a runtime or background tasks.
///
/// Each read checks the current installed contracts and holds a shared control
/// pin until its transaction ends. An observer does not delay upgrades between
/// reads. Unsupported policies fail before protected data is read. Opening and
/// reading create no tables, register no participant, and renew no authority.
/// Diagnostics grant no mutation authority and can become stale after a read.
/// A caller-owned transaction retains its pin after a read returns and can delay
/// upgrades until the caller commits or rolls back that transaction.
/// Queue reads include application payloads, so the database role must be
/// authorized to read those payloads. An observer applies no application access policy.
///
/// ```no_run
/// use paranoid::db::{PgSchemaName, Pool};
/// use paranoid::installation::{
///     PgInstallationObserver, PgInstallationObserverFields, RecordedInstallationIdentity,
/// };
/// use paranoid::fleet::CounterKey;
/// # async fn inspect(pool: &Pool) -> Result<(), Box<dyn std::error::Error>> {
/// let observer = PgInstallationObserver::open(PgInstallationObserverFields {
///     pool,
///     schema: PgSchemaName::from_identifier_text("app_state")?,
///     identity: RecordedInstallationIdentity::DiscoverRecorded,
/// }).await?;
/// let count = observer.fetch_counter_value(pool, &CounterKey::new("completed")?).await?;
/// # let _ = count;
/// # Ok(())
/// # }
/// ```
pub struct PgInstallationObserver {
	installation_id: SortableId,
	admission: Arc<PgNativeObservation>,
	queue: PgQueueReader,
	counters: Arc<NativeCounterBindings>,
	leases: LeaseComponent,
	lease_storage: super::creation::PgNativeNamespaceBinding,
	lease_source: Option<NativeLeaseSourceBindings>,
}

impl PgInstallationObserver {
	/// Opens and validates an existing installation using owned read-only transactions.
	/// Missing installations are errors; opening never creates or imports one.
	pub async fn open(
		fields: PgInstallationObserverFields<'_>,
	) -> Result<Self, PgInstallationObserverOpenError> {
		Self::open_inner(fields)
			.await
			.map_err(|cause| PgInstallationObserverOpenError {
				cause: Box::new(cause),
			})
	}

	async fn open_inner(
		fields: PgInstallationObserverFields<'_>,
	) -> Result<Self, ObserverOpenCause> {
		let layout = PgStorageLayout::new(&fields.schema).into_bindings();
		let (native, control, installation, queue_source, installation_id) =
			match inspect_installation(fields.pool, &layout, None).await? {
				InspectedInstallation::Fresh => {
					let opened =
						PgNativeInstallation::open_recorded(fields.pool, layout, fields.identity)
							.await?;
					let id = opened.installation_id();
					let (native, control) = opened.into_parts();
					let native = Arc::new(native);
					let installation = Arc::new(PgInstallationAdmissionBinding::Fresh {
						native: Arc::clone(&native),
					});
					(native, control, installation, None, id)
				}
				InspectedInstallation::ImportedFromV10(_) => {
					let opened = PgRecordedImportedInstallation::open_recorded(
						fields.pool,
						layout,
						fields.identity,
					)
					.await?;
					let id = opened.installation_id();
					let source_queue = crate::runtime::v10::prepare_recorded_queue_source(
						&opened,
						Arc::new(QueueSourceImportDefs::new()),
					);
					let (source, native, control) = opened.into_parts();
					let native = Arc::new(native);
					let installation = Arc::new(PgInstallationAdmissionBinding::Imported {
						native: Arc::clone(&native),
						source: Arc::new(source),
					});
					(native, control, installation, source_queue, id)
				}
				InspectedInstallation::Absent | InspectedInstallation::PublishedV10 => {
					return Err(ObserverOpenCause::Absent);
				}
			};
		let administration = Arc::new(
			PgNativeAdministration::new(PgNativeAdministrationFields {
				participant: Arc::new(ParticipantSchema::new()),
				control: Arc::new(control),
				native: Arc::clone(&native),
			})
			.map_err(ObserverOpenCause::Administration)?,
		);
		let lease_bridge = Arc::new(LeaseSourceBridge::new());
		let sources = RuntimeSourceAdapters::prepare(NativeSourcePreparationFields {
			pool: fields.pool,
			installation: &installation,
			administration: &administration,
			lease_bridge: &lease_bridge,
			queue: queue_source,
		})
		.await
		.map_err(|error| ObserverOpenCause::Storage(Box::new(error)))?;
		let (queue_source, counter_source, lease_source) = match sources {
			RuntimeSourceAdapters::Native => (None, None, None),
			RuntimeSourceAdapters::Source(sources) => (
				Some(sources.queue),
				Some(sources.counter),
				Some(sources.lease),
			),
		};
		let admission = Arc::new(PgNativeObservation {
			installation_binding: installation,
			control_storage: native.control().clone(),
			administration,
		});
		let operation_admission = PgOperationAdmission::Observation(Arc::clone(&admission));
		let queue = PgNativeQueueGroup::new(NativeQueueBindings {
			source: queue_source,
			jobs: Arc::new(QueueJobComponent::new()),
			auxiliary: Arc::new(QueueAuxiliaryComponent::new()),
			storage: native.queue().clone(),
			admission: operation_admission.clone(),
			attempt_budget: crate::records::postgres::group_attempt::PgRecordGroupAttemptBudget {
				conflict_attempts: parameters::CONFLICT_ATTEMPTS,
				identity_count: parameters::GROUP_IDENTITY_COUNT,
				whole_set_attempts: parameters::WHOLE_SET_ATTEMPTS,
			},
		})
		.map_err(|error| ObserverOpenCause::Queue(Box::new(error)))?;
		Ok(Self {
			installation_id,
			admission,
			queue: PgQueueReader::new(queue),
			counters: Arc::new(NativeCounterBindings {
				component: CounterComponent::new(),
				storage: native.counters().storage().clone(),
				source: counter_source,
				admission: operation_admission,
				conflict_attempts: parameters::CONFLICT_ATTEMPTS,
				namespace: native.counters().namespace().clone(),
			}),
			leases: LeaseComponent::new(),
			lease_storage: native.leases().clone(),
			lease_source,
		})
	}

	/// Returns the ID that opening verified. Each read verifies that ID again.
	pub fn installation_id(&self) -> SortableId {
		self.installation_id
	}

	/// Returns read-only job lookup, job listing, and dead-letter listing operations.
	pub fn queue(&self) -> PgQueueReader {
		self.queue.clone()
	}

	/// Fetches a counter value. An absent counter has value zero.
	pub async fn fetch_counter_value(
		&self,
		database: impl Database,
		key: &CounterKey,
	) -> Result<i64, PgCounterError> {
		let identity = RecordIdentity::new(
			self.counters.namespace.clone(),
			RecordKey::new(key.as_str().as_bytes().to_vec()),
		);
		PgNativeCounter::new(Arc::clone(&self.counters), identity)
			.fetch_value(database)
			.await
			.map_err(PgCounterError::new)
	}

	/// Fetches a lease's live holder at the database observation time.
	/// The returned holder label and fencing token grant no authority.
	pub async fn fetch_live_lease_holder(
		&self,
		database: impl Database,
		key: &LeaseKey,
	) -> Result<Option<PgLeaseHolder>, PgLeaseError> {
		run_transaction_read(
			database,
			"db.observer.fetch_live_lease_holder",
			async |tx| {
				self.admission
					.execute_read_with_contracts(tx, async |contracts, tx| {
						NativeLeaseReadBindings {
							component: &self.leases,
							storage: self.lease_storage.storage(),
							source: self.lease_source.as_ref(),
							namespace: self.lease_storage.namespace(),
							installation: &self.admission.installation_binding,
						}
						.fetch_holder(tx, &contracts, key, contracts.storage_route())
						.await
					})
					.await
					.map_err(NativeLeaseError::from_operation)
			},
		)
		.await
		.map(|holder| holder.map(PgLeaseHolder::new))
		.map_err(PgLeaseError::new)
	}
}

/// An observer could not open an existing installation.
#[derive(Debug, thiserror::Error)]
#[error("cannot open Paranoid observer")]
pub struct PgInstallationObserverOpenError {
	#[source]
	cause: Box<ObserverOpenCause>,
}

/// The class of an observer opening failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PgInstallationObserverOpenErrorKind {
	/// No recorded installation exists in the supplied schema.
	InstallationAbsent,
	/// The installation, storage, or installed contracts cannot be opened by this code.
	Installation,
	/// The installation still reads v10 data, and the database's encoding is neither `UTF8` nor
	/// `SQL_ASCII`. The error's source names the encoding. See
	/// [`PgV10Fields`](crate::installation::PgV10Fields).
	UnsupportedDatabaseEncoding,
}

impl PgInstallationObserverOpenError {
	/// Returns the failure class.
	pub fn kind(&self) -> PgInstallationObserverOpenErrorKind {
		match self.cause.as_ref() {
			ObserverOpenCause::Absent => PgInstallationObserverOpenErrorKind::InstallationAbsent,
			ObserverOpenCause::Installation(
				NativeInstallationError::UnsupportedDatabaseEncoding { .. },
			) => PgInstallationObserverOpenErrorKind::UnsupportedDatabaseEncoding,
			_ => PgInstallationObserverOpenErrorKind::Installation,
		}
	}
	/// Returns a database failure when PostgreSQL or a connection failed.
	pub fn database_error(&self) -> Option<&DatabaseError> {
		match self.cause.as_ref() {
			ObserverOpenCause::Installation(error) => error.database_error(),
			ObserverOpenCause::Storage(error) => error.database_error(),
			ObserverOpenCause::Queue(error) => error.database_error(),
			_ => None,
		}
	}
}

#[derive(Debug, thiserror::Error)]
enum ObserverOpenCause {
	#[error("the schema has no recorded Paranoid installation")]
	Absent,
	#[error("installation validation failed")]
	Installation(#[from] NativeInstallationError),
	#[error("administrative policy binding failed")]
	Administration(#[source] super::administration::NativeAdministrationError),
	#[error("storage preparation failed")]
	Storage(#[source] Box<NativeSourcePreparationError>),
	#[error("Queue storage binding failed")]
	Queue(#[source] Box<NativeQueueGroupError>),
}
