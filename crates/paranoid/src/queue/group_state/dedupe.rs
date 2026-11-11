//! Dedupe reconciliation composes actual job transitions and the complete logical owner view.
use super::super::auxiliary_state::membership;
use super::snapshot::Origin;
use super::{
	ActiveDedupeState, AuxiliarySchemaBinding, BoundQueueGroup, ClaimedJob, DeadLetterMove,
	DeadLetterReason, DeadLetterTarget, JobId, NativeJob, OwnedJobMutation, PlanBuilder,
	PreparedQueueGroupOutcome, QueueGroupError, QueueGroupPreparation, QueueGroupSnapshot,
	QueueSourceRole, RecordMutation, StaleClaimRecoveryKind, StaleClaimRecoveryRequest, at_record,
	encode_auxiliary, inconsistent, metadata, preparation,
};

impl BoundQueueGroup<'_, '_, '_> {
	/// Prepares one exact private-owned job change; no new claim is delivered.
	pub(in crate::queue) fn prepare_owned_reconciliation(
		&self,
		snapshot: &QueueGroupSnapshot<'_>,
		authority: &ClaimedJob,
		mutation: OwnedJobMutation,
	) -> Result<QueueGroupPreparation, QueueGroupError> {
		preparation((|| {
			let id = authority.job_id();
			let Some((before, origin)) = self.read_job(snapshot, id)? else {
				return Ok(QueueGroupPreparation::Unchanged);
			};
			let after = self
				.job_binding(id)?
				.prepare_owned_mutation(
					before.clone(),
					&super::super::record_transitions::JobClaimCredentials::from_claimed_job(
						authority,
					),
					mutation,
					snapshot.observed_at,
				)
				.map_err(|error| at_record(self.job_identity(id), error))?;
			let Some(after) = after else {
				return Ok(QueueGroupPreparation::Unchanged);
			};
			let mut plan = PlanBuilder::new();
			self.reconcile_dedupe(snapshot, Some((&before, origin)), Some(&after), &mut plan)?;
			self.plan_job_write(snapshot, &after, &mut plan)?;
			plan.remove_source(self.job_identity(id), origin, QueueSourceRole::Job);
			plan.finish(snapshot, PreparedQueueGroupOutcome::Reconciled)
		})())
	}
	/// Reevaluates strict staleness once and composes any required dead letter move in the same plan.
	/// The supplied dead letter ID is used only when the installed recovery policy requires a move.
	pub(in crate::queue) fn prepare_stale_recovery(
		&self,
		snapshot: &QueueGroupSnapshot<'_>,
		request: StaleClaimRecoveryRequest,
	) -> Result<QueueGroupPreparation, QueueGroupError> {
		preparation((|| {
			let StaleClaimRecoveryRequest {
				job_id: id,
				dead_letter_id,
			} = request;
			let Some((before, origin)) = self.read_job(snapshot, id)? else {
				return Ok(QueueGroupPreparation::Unchanged);
			};
			let Some(change) = self
				.job_binding(id)?
				.prepare_stale_recovery(before.clone(), snapshot.observed_at)
				.map_err(|error| at_record(self.job_identity(id), error))?
			else {
				return Ok(QueueGroupPreparation::Unchanged);
			};
			if change.kind == StaleClaimRecoveryKind::ExecutionExpiredDeadLetter {
				return self.move_dead_letter(
					snapshot,
					DeadLetterMove {
						before,
						origin,
						failed: change.state,
						target: DeadLetterTarget {
							dead_letter_id,
							reason: DeadLetterReason::ExecutionExpired,
						},
					},
				);
			}
			let mut plan = PlanBuilder::new();
			self.reconcile_dedupe(
				snapshot,
				Some((&before, origin)),
				Some(&change.state),
				&mut plan,
			)?;
			self.plan_job_write(snapshot, &change.state, &mut plan)?;
			plan.remove_source(self.job_identity(id), origin, QueueSourceRole::Job);
			plan.finish(snapshot, PreparedQueueGroupOutcome::Reconciled)
		})())
	}

	/// Returns an exact owner after checking its body and both physical/index observations.
	pub(super) fn active_owner(
		&self,
		snapshot: &QueueGroupSnapshot<'_>,
		binding: &AuxiliarySchemaBinding<'_, ActiveDedupeState>,
		task: &super::super::TaskName,
		dedupe: &super::super::DedupeKey,
	) -> Result<(Option<JobId>, bool), QueueGroupError> {
		let map = binding
			.decode_record(binding.identity(), snapshot.held(binding.identity())?)
			.map_err(|error| at_record(binding.identity().clone(), error))?;
		let source = snapshot.probe(&snapshot.source.active_dedupe, binding.identity())?;
		if let (Some(map), Some(source)) = (&map, source)
			&& map.job_id != source
		{
			return Err(inconsistent("native and source active owners disagree"));
		}
		let owner = map.as_ref().map(|map| map.job_id).or(source);
		if let Some(id) = owner {
			let Some((state, origin)) = self.read_job(snapshot, id)? else {
				return Err(inconsistent("active dedupe points to a missing job"));
			};
			ActiveDedupeState {
				task_name: task.clone(),
				dedupe_key: dedupe.clone(),
				job_id: id,
			}
			.check_job(state.job())?;
			if source.is_some() && origin != Origin::Source {
				return Err(inconsistent(
					"source active probe has no matching source body",
				));
			}
			if origin == Origin::Source && source != Some(id) {
				return Err(inconsistent(
					"source active body has no matching source probe",
				));
			}
		}
		Ok((owner, map.is_some()))
	}
	pub(super) fn reconcile_dedupe(
		&self,
		snapshot: &QueueGroupSnapshot<'_>,
		before: Option<(&NativeJob, Origin)>,
		after: Option<&NativeJob>,
		plan: &mut PlanBuilder,
	) -> Result<(), QueueGroupError> {
		let old =
			before.and_then(|(state, origin)| membership(state.job()).map(|tuple| (tuple, origin)));
		let new = after.and_then(|state| membership(state.job()));
		if let Some(((task, dedupe, id), origin)) = old {
			let binding = self.dedupe_binding(task, dedupe)?;
			let (owner, has_map) = self.active_owner(snapshot, &binding, task, dedupe)?;
			if owner != Some(id) || (!has_map && origin == Origin::Native) {
				return Err(inconsistent("active job has no exact dedupe mapping"));
			}
			if new == Some((task, dedupe, id)) {
				if !has_map {
					plan.action(
						binding.identity().clone(),
						RecordMutation::Write(encode_auxiliary(
							&binding,
							&ActiveDedupeState {
								task_name: task.clone(),
								dedupe_key: dedupe.clone(),
								job_id: id,
							},
							metadata(snapshot.observed_at),
						)?),
					);
				}
				return Ok(());
			}
			if has_map {
				plan.action(binding.identity().clone(), RecordMutation::Delete);
			}
		}
		if let Some((task, dedupe, id)) = new {
			let binding = self.dedupe_binding(task, dedupe)?;
			let (owner, _) = self.active_owner(snapshot, &binding, task, dedupe)?;
			if owner.is_some() {
				return Err(QueueGroupError::DedupeConflict);
			}
			plan.action(
				binding.identity().clone(),
				RecordMutation::Write(encode_auxiliary(
					&binding,
					&ActiveDedupeState {
						task_name: task.clone(),
						dedupe_key: dedupe.clone(),
						job_id: id,
					},
					metadata(snapshot.observed_at),
				)?),
			);
		}
		Ok(())
	}
}
