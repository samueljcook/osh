//! Full restoration precedes every new native acquisition after source ownership or a changed proposal.
use super::super::delete::{
	acquire,
	records::{HeaderRow, RawDeleteRecords},
};
use super::super::{DatabaseOperationKind, owned_portable_query_as};
use super::RawSlotRetryReasonError;
use super::selection::{Opportunity, Selected};
use super::{RawSlotStorageError, SlotOwner};
use crate::kv::raw::model::delete::RecordDeleteContention;
use crate::kv::raw::model::slot::prepare_raw_slot_seed;
use crate::records::RecordKey;
use std::collections::BTreeSet;
const ACQUIRE_NATIVE_SET_LABEL: &str = "db.state_storage.raw_slot.acquire_native_set";
const ACQUIRE_SOURCE_CANDIDATE_LABEL: &str = "db.state_storage.raw_slot.acquire_source_candidate";
pub(super) async fn select(
	owner: &mut SlotOwner<'_, '_>,
) -> Result<Opportunity, RawSlotStorageError> {
	let mut advisory = super::observation::observe(owner).await?;
	owner.seed = Some(prepare_raw_slot_seed(advisory.observed_at)?);
	let mut skipped = BTreeSet::new();
	let mut attempts = 0u32;
	loop {
		let mut seeds = BTreeSet::new();
		let mut proposed = None;
		for (index, pair) in advisory.pairs.iter().enumerate() {
			let key = &owner.candidates.keys()[index];
			let metadata = pair.native.or(pair.source);
			if metadata.is_none() {
				seeds.insert(key.clone());
			}
			let eligible = metadata
				.or_else(|| owner.seed.map(|s| s.metadata()))
				.is_some_and(|m| !m.is_live_at(advisory.observed_at));
			if proposed.is_none() && !skipped.contains(key) && eligible {
				proposed = Some(key.clone());
			}
		}
		if seeds.is_empty() && proposed.is_none() {
			return Ok(Opportunity::Unavailable(advisory.observed_at));
		}
		attempts = attempts.checked_add(1).ok_or(RawSlotStorageError::Shape)?;
		match attempt(owner, &seeds, proposed.as_ref()).await? {
			Attempt::Done(done) => return Ok(done),
			Attempt::Retry { reason, skip } => {
				if skip {
					skipped.insert(reason.identity().key().clone());
				}
				restore(owner, reason, attempts).await?;
				advisory = super::observation::observe(owner).await?;
			}
		}
	}
}
enum Attempt {
	Done(Opportunity),
	Retry {
		reason: RawSlotRetryReasonError,
		skip: bool,
	},
}
async fn attempt(
	owner: &mut SlotOwner<'_, '_>,
	seeds: &BTreeSet<RecordKey>,
	proposed: Option<&RecordKey>,
) -> Result<Attempt, RawSlotStorageError> {
	let mut known = seeds.clone();
	if let Some(key) = proposed {
		known.insert(key.clone());
	}
	owner.records = RawDeleteRecords::new(owner.namespace.clone());
	owner.seeded.clear();
	for key in &known {
		owner.records.push(key.clone())?;
	}
	let proposed_index = proposed
		.map(|key| {
			owner
				.records
				.positions
				.get(key)
				.copied()
				.ok_or(RawSlotStorageError::Shape)
		})
		.transpose()?;
	let proposed_rank = proposed_index
		.map(|index| i64::try_from(index).map_err(|_| RawSlotStorageError::Shape))
		.transpose()?;
	let seed_indices = seeds
		.iter()
		.map(|key| {
			owner
				.records
				.positions
				.get(key)
				.copied()
				.ok_or(RawSlotStorageError::Shape)
		})
		.collect::<Result<BTreeSet<_>, _>>()?;
	owner.begin_attempt().await?;
	let sql = acquire::native_query(acquire::QueryFields {
		table: owner.native_table()?,
		contention: RecordDeleteContention::SkipLocked,
	});
	let keys = known
		.iter()
		.map(|k| k.as_bytes().to_vec())
		.collect::<Vec<_>>();
	owner.tx.record_database_operation(
		DatabaseOperationKind::FetchAll,
		ACQUIRE_NATIVE_SET_LABEL,
		Some(sql.as_str()),
	);
	let rows = owned_portable_query_as::<HeaderRow>(sql)
		.bind_validated(owner.namespace.as_bytes())
		.bind_validated(keys)
		.fetch_all(owner.tx.inner.as_mut())
		.await
		.map_err(|e| e.with_operation_label(ACQUIRE_NATIVE_SET_LABEL))?;
	acquire::receive_native(&mut owner.records, owner.layout, rows)?;
	for index in &seed_indices {
		let c = &owner.records.candidates[*index];
		if !c.native.is_held() || c.native.value().is_some() {
			return Ok(Attempt::Retry {
				reason: RawSlotRetryReasonError::NativeSeedChanged {
					identity: owner.identity(&c.key),
				},
				skip: false,
			});
		}
	}
	if let Some(index) = proposed_index {
		let c = &owner.records.candidates[index];
		if !c.native.is_held() {
			return Ok(Attempt::Retry {
				reason: RawSlotRetryReasonError::CandidateSkipped {
					identity: owner.identity(&c.key),
				},
				skip: true,
			});
		}
	}
	let source =
		super::observation::probe_source(owner, &known.iter().cloned().collect::<Vec<_>>()).await?;
	for c in &owner.records.candidates {
		if c.native.value().is_some() && source.contains_key(&c.key) {
			return Err(
				owner.identify_error_by_key(RawSlotStorageError::DuplicatePhysicalValues, &c.key)
			);
		}
		if seeds.contains(&c.key) && source.contains_key(&c.key) {
			return Ok(Attempt::Retry {
				reason: RawSlotRetryReasonError::SourceSeedChanged {
					identity: owner.identity(&c.key),
				},
				skip: false,
			});
		}
	}
	if let (Some(key), Some(index), Some(rank)) = (proposed, proposed_index, proposed_rank) {
		if source.contains_key(key) {
			let sql = acquire::source_query_builder(acquire::QueryFields {
				table: owner.source_table()?,
				contention: RecordDeleteContention::SkipLocked,
			})
			.finish();
			let texts = vec![owner.source_text(key)?.to_owned()];
			owner.tx.record_database_operation(
				DatabaseOperationKind::FetchAll,
				ACQUIRE_SOURCE_CANDIDATE_LABEL,
				Some(sql.as_str()),
			);
			let rows = owned_portable_query_as::<HeaderRow>(sql)
				.bind_validated(texts)
				.bind_validated(vec![rank])
				.fetch_all(owner.tx.inner.as_mut())
				.await
				.map_err(|e| e.with_operation_label(ACQUIRE_SOURCE_CANDIDATE_LABEL))?;
			acquire::receive_source_untimed(
				&mut owner.records,
				owner.layout,
				&BTreeSet::from([index]),
				rows,
			)?;
			let c = &owner.records.candidates[index];
			if !c.source.is_present() {
				return Ok(Attempt::Retry {
					reason: RawSlotRetryReasonError::CandidateDisappeared {
						identity: owner.identity(key),
					},
					skip: true,
				});
			}
			if !c.source.is_held() {
				return Ok(Attempt::Retry {
					reason: RawSlotRetryReasonError::CandidateSkipped {
						identity: owner.identity(key),
					},
					skip: true,
				});
			}
		} else if owner.records.candidates[index].native.value().is_none() && !seeds.contains(key) {
			return Ok(Attempt::Retry {
				reason: RawSlotRetryReasonError::CandidateDisappeared {
					identity: owner.identity(key),
				},
				skip: true,
			});
		}
	}
	let prepared = super::apply::prepare_seeds(owner, &seed_indices)?;
	if let Some(prepared) = prepared {
		super::apply::execute(owner, prepared).await?;
	}
	owner.seeded = seeds.clone();
	let observed_at = super::selection::observe_time(owner).await?;
	if let Some(index) = proposed_index {
		let c = &owner.records.candidates[index];
		let metadata =
			c.native.value().or(c.source.value()).ok_or_else(|| {
				owner.identify_error_by_key(RawSlotStorageError::Coverage, &c.key)
			})?;
		if !metadata.is_live_at(observed_at) {
			return Ok(Attempt::Done(Opportunity::Selected(Selected {
				index,
				observed_at,
			})));
		}
		if c.source.is_held() {
			return Ok(Attempt::Retry {
				reason: RawSlotRetryReasonError::CandidateBecameLive {
					identity: owner.identity(&c.key),
				},
				skip: true,
			});
		}
	}
	if let Some(index) = super::selection::eligible_held_seed(owner, observed_at) {
		return Ok(Attempt::Done(Opportunity::Selected(Selected {
			index,
			observed_at,
		})));
	}
	if let Some(key) = proposed {
		return Ok(Attempt::Retry {
			reason: RawSlotRetryReasonError::CandidateBecameLive {
				identity: owner.identity(key),
			},
			skip: true,
		});
	}
	super::apply::keep(owner).await?;
	owner.release().await?;
	Ok(Attempt::Done(Opportunity::Unavailable(observed_at)))
}
async fn restore(
	owner: &mut SlotOwner<'_, '_>,
	reason: RawSlotRetryReasonError,
	attempts: u32,
) -> Result<(), RawSlotStorageError> {
	let rejected = attempts >= owner.conflict_attempts.get();
	if let Err(cleanup) = owner.restore_attempt().await {
		return Err(RawSlotStorageError::RejectionCleanup {
			primary_error: Box::new(RawSlotStorageError::Replan { source: reason }),
			cleanup_error: Box::new(cleanup),
		});
	}
	owner.records = RawDeleteRecords::new(owner.namespace.clone());
	owner.seeded.clear();
	if rejected {
		return Err(RawSlotStorageError::RejectedAfterRestore { source: reason });
	}
	Ok(())
}
