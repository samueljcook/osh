//! Closed native sequence/event acquisition, without installation or payload authority.
//!
//! The fixed owning operation binds current core/scoped policy before calling this adapter.
//! Actual event encoding and selected sequence encoding remain its preparation obligation.

use std::cmp::Ordering;
use std::collections::BTreeSet;

use crate::fleet::TopicKey;
use crate::fleet::payload::identity::NativeFleetPayloadIdentity;
use crate::fleet::scalar::NativeFleetScalarResource;
use crate::fleet::scalar_codec::{ScalarSchemaBinding, ScalarStateError};
use crate::fleet::topic::event_state::{TopicEventIdentity, TopicEventIdentityFields};
use crate::fleet::topic::state::TopicMetadataComponent;
use crate::installation::contract::BoundComponentContract;
use crate::installation::creation::PgNativeFleetScalarStorage;
use crate::records::{
	EncodedRecord, RecordIdentity, RecordMutation, RecordNamespace, StateTimestampMicros,
};

use crate::db::WriteTx;
use crate::records::postgres::PgPointStorage;
use crate::records::postgres::attempt_boundary::{
	GroupAttemptRejectionError, PgRecordGroupAttemptBudget, RecordAttemptProgress,
};
use crate::records::postgres::record_set::{
	PgAcquiringRecordSet, PgAcquiringRecordSetAttempt, PgPlacedRecordIdentity, PgRecordSetAttempt,
	PgRecordSetLock, PgRecordTarget, PgRecordTargetFields, RecordSetError, RecordSetRejectionError,
};

mod error;
pub(in crate::fleet::topic) use error::{
	TopicPublicationRejectionError, TopicPublicationStorageError,
};

pub(in crate::fleet::topic) struct TopicSequenceTargetFields<'storage, 'policy, 'bound, 'code> {
	pub(in crate::fleet::topic) fleet_scalar: &'storage PgNativeFleetScalarStorage,
	pub(in crate::fleet::topic) component: &'policy TopicMetadataComponent,
	pub(in crate::fleet::topic) contract:
		&'policy BoundComponentContract<'bound, 'code, TopicMetadataComponent>,
}
/// Construction checks actual component/schema identity, but grants no current admission.
pub(in crate::fleet::topic) struct TopicSequenceTarget<'storage, 'policy> {
	fleet_scalar: &'storage PgNativeFleetScalarStorage,
	component: &'policy TopicMetadataComponent,
	schema: ScalarSchemaBinding<'policy, i64>,
}
impl<'storage, 'policy> TopicSequenceTarget<'storage, 'policy> {
	pub(in crate::fleet::topic) fn new(
		fields: TopicSequenceTargetFields<'storage, 'policy, '_, '_>,
	) -> Result<Self, ScalarStateError> {
		Ok(Self {
			fleet_scalar: fields.fleet_scalar,
			component: fields.component,
			schema: fields.component.bind_sequence(fields.contract)?,
		})
	}
	fn next(
		&self,
		acquiring: &PgAcquiringRecordSet<'_, '_>,
		identity: &PgPlacedRecordIdentity,
	) -> Result<i64, TopicPublicationRejectionError> {
		let current = acquiring
			.record(identity)
			.map_err(TopicPublicationRejectionError::Plan)?
			.map(|record| self.schema.decode_record(record))
			.transpose()
			.map_err(|error| TopicPublicationRejectionError::Sequence {
				identity: Box::new(identity.clone()),
				source: Box::new(error),
			})?;
		self.component.next_sequence(current).map_err(|error| {
			TopicPublicationRejectionError::Sequence {
				identity: Box::new(identity.clone()),
				source: Box::new(error),
			}
		})
	}
}

pub(in crate::fleet::topic) struct TopicEventTarget<'storage> {
	pub(in crate::fleet::topic) storage: &'storage PgPointStorage,
	pub(in crate::fleet::topic) namespace: &'storage RecordNamespace,
}
pub(in crate::fleet::topic) struct TopicPublicationLockFields<'a, 'tx, 'policy> {
	pub(in crate::fleet::topic) tx: &'a mut WriteTx<'tx>,
	pub(in crate::fleet::topic) sequence: TopicSequenceTarget<'a, 'policy>,
	pub(in crate::fleet::topic) events: TopicEventTarget<'a>,
	pub(in crate::fleet::topic) topic: &'a TopicKey,
	pub(in crate::fleet::topic) budget: PgRecordGroupAttemptBudget,
}

enum TopicAcquisition<'a, 'tx> {
	SequenceFirst(PgAcquiringRecordSet<'a, 'tx>),
	EventFirstDiscovery {
		set: PgAcquiringRecordSetAttempt<'a, 'tx>,
		progress: RecordAttemptProgress<PgPlacedRecordIdentity>,
	},
	EventFirstComplete {
		set: PgAcquiringRecordSetAttempt<'a, 'tx>,
		progress: RecordAttemptProgress<PgPlacedRecordIdentity>,
	},
}
impl<'a, 'tx> TopicAcquisition<'a, 'tx> {
	fn records(&self) -> &PgAcquiringRecordSet<'a, 'tx> {
		match self {
			Self::SequenceFirst(set) => set,
			Self::EventFirstDiscovery { set, .. } | Self::EventFirstComplete { set, .. } => {
				set.records()
			}
		}
	}
	async fn reject(self) -> Result<(), RecordSetError> {
		match self {
			Self::SequenceFirst(set) => set.keep().await,
			Self::EventFirstDiscovery { set, .. } | Self::EventFirstComplete { set, .. } => {
				set.restore().await
			}
		}
	}
}
enum TopicPublicationSet<'a, 'tx> {
	Retained(PgRecordSetLock<'a, 'tx>),
	Attempt(PgRecordSetAttempt<'a, 'tx>),
}
impl TopicPublicationSet<'_, '_> {
	fn observed_at(&self) -> StateTimestampMicros {
		match self {
			Self::Retained(set) => set.observed_at(),
			Self::Attempt(set) => set.observed_at(),
		}
	}
	async fn apply(
		self,
		actions: Vec<(PgPlacedRecordIdentity, RecordMutation)>,
	) -> Result<Result<(), RecordSetRejectionError>, RecordSetError> {
		match self {
			Self::Retained(set) => set.apply(actions).await,
			Self::Attempt(set) => set.apply(actions).await,
		}
	}
	async fn reject(self) -> Result<(), RecordSetError> {
		match self {
			Self::Retained(set) => set.keep().await,
			Self::Attempt(set) => set.restore().await,
		}
	}
}

/// The exact pair and any superseded discovery identities remain held until consumption.
/// A success is physical preparation, not commit, a private grant or source exclusion.
pub(in crate::fleet::topic) struct PgTopicPublicationLock<'a, 'tx> {
	set: TopicPublicationSet<'a, 'tx>,
	sequence: PgPlacedRecordIdentity,
	event: PgPlacedRecordIdentity,
	topic_event_identity: TopicEventIdentity,
}
pub(in crate::fleet::topic) struct TopicPublicationRecords {
	pub(in crate::fleet::topic) sequence: EncodedRecord,
	pub(in crate::fleet::topic) event: EncodedRecord,
}
impl<'a, 'tx> PgTopicPublicationLock<'a, 'tx> {
	/// Requires two identities; event-first placement also requires a discovery and full-set
	/// attempt.
	pub(in crate::fleet::topic) async fn acquire(
		fields: TopicPublicationLockFields<'a, 'tx, '_>,
	) -> Result<Result<Self, TopicPublicationRejectionError>, TopicPublicationStorageError> {
		if fields.budget.identity_count.get() < 2 {
			return Ok(Err(TopicPublicationRejectionError::Budget(
				GroupAttemptRejectionError::IdentityBudgetExhausted,
			)));
		}
		let sequence_record = RecordIdentity::new(
			fields.sequence.fleet_scalar.topic_sequence().clone(),
			NativeFleetScalarResource::TopicSequence(fields.topic).record_key(),
		);
		let sequence_target = PgRecordTarget::new(PgRecordTargetFields {
			storage: fields.sequence.fleet_scalar.storage(),
			record: sequence_record,
		});
		let sequence_identity = sequence_target.identity().clone();
		let order = fields
			.sequence
			.fleet_scalar
			.storage()
			.table()
			.cmp(fields.events.storage.table())
			.then_with(|| {
				fields
					.sequence
					.fleet_scalar
					.topic_sequence()
					.cmp(fields.events.namespace)
			});
		if order == Ordering::Equal {
			return Ok(Err(TopicPublicationRejectionError::OverlappingPlacement));
		}
		if order == Ordering::Greater && fields.budget.whole_set_attempts.get() < 2 {
			return Ok(Err(TopicPublicationRejectionError::Budget(
				GroupAttemptRejectionError::AttemptBudgetExhausted,
			)));
		}
		let mut acquisition = if order == Ordering::Less {
			let mut set = PgAcquiringRecordSet::begin(fields.tx);
			set.acquire_known(vec![sequence_target], fields.budget.conflict_attempts)
				.await?;
			TopicAcquisition::SequenceFirst(set)
		} else {
			let mut set = PgAcquiringRecordSet::begin_attempt(fields.tx).await?;
			set.acquire_known(vec![sequence_target], fields.budget.conflict_attempts)
				.await?;
			let progress = RecordAttemptProgress::new(
				BTreeSet::from([sequence_identity.clone()]),
				fields.budget,
			)
			.expect("the initial scalar identity fits the checked two-identity budget");
			TopicAcquisition::EventFirstDiscovery { set, progress }
		};
		loop {
			let next = match fields
				.sequence
				.next(acquisition.records(), &sequence_identity)
			{
				Ok(next) => next,
				Err(rejection) => return reject_acquiring(acquisition, rejection).await,
			};
			let topic_event_identity = TopicEventIdentity::new(TopicEventIdentityFields {
				topic: fields.topic.clone(),
				sequence: next,
			})
			.expect("actual checked Topic allocation is positive");
			let event_target = PgRecordTarget::new(PgRecordTargetFields {
				storage: fields.events.storage,
				record: RecordIdentity::new(
					fields.events.namespace.clone(),
					NativeFleetPayloadIdentity::TopicEvent(&topic_event_identity).record_key(),
				),
			});
			let event_identity = event_target.identity().clone();
			let ready = match acquisition {
				TopicAcquisition::SequenceFirst(mut set) => {
					set.acquire_known(vec![event_target], fields.budget.conflict_attempts)
						.await?;
					TopicAcquisition::SequenceFirst(set)
				}
				TopicAcquisition::EventFirstComplete { set, progress }
					if progress.identities().contains(&event_identity) =>
				{
					TopicAcquisition::EventFirstComplete { set, progress }
				}
				TopicAcquisition::EventFirstDiscovery { set, mut progress }
				| TopicAcquisition::EventFirstComplete { set, mut progress } => {
					let mut set = match set
						.restore_and_advance(&mut progress, BTreeSet::from([event_identity]))
						.await?
					{
						Ok(set) => set,
						Err(rejection) => {
							return Ok(Err(TopicPublicationRejectionError::Budget(rejection)));
						}
					};
					let targets = progress
						.identities()
						.iter()
						.map(|identity| {
							PgRecordTarget::new(PgRecordTargetFields {
								storage: if identity == &sequence_identity {
									fields.sequence.fleet_scalar.storage()
								} else {
									fields.events.storage
								},
								record: identity.record().clone(),
							})
						})
						.collect();
					set.acquire_known(targets, fields.budget.conflict_attempts)
						.await?;
					acquisition = TopicAcquisition::EventFirstComplete { set, progress };
					continue;
				}
			};
			match ready.records().record(&event_identity) {
				Ok(None) => {}
				Ok(Some(_)) => {
					return reject_acquiring(
						ready,
						TopicPublicationRejectionError::OccupiedEvent {
							identity: Box::new(event_identity),
						},
					)
					.await;
				}
				Err(rejection) => {
					return reject_acquiring(
						ready,
						TopicPublicationRejectionError::Plan(rejection),
					)
					.await;
				}
			}
			let set = match ready {
				TopicAcquisition::SequenceFirst(set) => {
					TopicPublicationSet::Retained(set.finish().await?)
				}
				TopicAcquisition::EventFirstComplete { set, .. } => {
					TopicPublicationSet::Attempt(set.finish().await?)
				}
				TopicAcquisition::EventFirstDiscovery { .. } => {
					unreachable!("discovery must reacquire the complete set")
				}
			};
			return Ok(Ok(Self {
				set,
				sequence: sequence_identity,
				event: event_identity,
				topic_event_identity,
			}));
		}
	}
	pub(in crate::fleet::topic) fn topic_event_identity(&self) -> &TopicEventIdentity {
		&self.topic_event_identity
	}
	pub(in crate::fleet::topic) fn sequence_identity(&self) -> &PgPlacedRecordIdentity {
		&self.sequence
	}
	pub(in crate::fleet::topic) fn event_identity(&self) -> &PgPlacedRecordIdentity {
		&self.event
	}
	pub(in crate::fleet::topic) fn observed_at(&self) -> StateTimestampMicros {
		self.set.observed_at()
	}

	/// The owning actual bound codecs must prepare both records and all metadata before this call.
	/// Physical preparation still derives every projection/cleanup subset before any write.
	pub(in crate::fleet::topic) async fn apply(
		self,
		records: TopicPublicationRecords,
	) -> Result<Result<(), TopicPublicationRejectionError>, TopicPublicationStorageError> {
		let actions = vec![
			(self.sequence, RecordMutation::Write(records.sequence)),
			(self.event, RecordMutation::Write(records.event)),
		];
		let result = self.set.apply(actions).await?;
		Ok(result.map_err(TopicPublicationRejectionError::Plan))
	}
	/// The caller retains its pure preparation cause and pairs any cleanup error with that cause.
	pub(in crate::fleet::topic) async fn reject(self) -> Result<(), TopicPublicationStorageError> {
		self.set.reject().await?;
		Ok(())
	}
}

async fn reject_acquiring<'a, 'tx>(
	acquiring: TopicAcquisition<'a, 'tx>,
	rejection: TopicPublicationRejectionError,
) -> Result<
	Result<PgTopicPublicationLock<'a, 'tx>, TopicPublicationRejectionError>,
	TopicPublicationStorageError,
> {
	let cleanup = acquiring.reject().await;
	match cleanup {
		Ok(()) => Ok(Err(rejection)),
		Err(cleanup) => Err(TopicPublicationStorageError::RejectionCleanup {
			rejection_error: Box::new(rejection),
			cleanup_error: Box::new(cleanup),
		}),
	}
}
