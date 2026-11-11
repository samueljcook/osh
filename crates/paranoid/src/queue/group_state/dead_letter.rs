//! Dead-letter plans preserve current uniqueness and actual public retry/requeue behavior.
use super::snapshot::Origin;
use super::{
	AuxiliarySchemaBinding, BoundQueueGroup, DeadLetterJob, DeadLetterMove,
	DeadLetterOriginalState, DeadLetterRequeueTarget, DeadLetterTarget, JobId, JobLifecycle,
	JobStatus, NativeDeadLetterState, NativeJob, NativeJobMetadata, NativeJobParts,
	NativeJobPayload, PlanBuilder, PreparedQueueGroupOutcome, QueueGroupError,
	QueueGroupPreparation, QueueGroupSnapshot, QueueRepresentationError, QueueRequeueSchedule,
	QueueSourceRole, RecordIdentity, RecordKey, RecordMutation, at_record, encode_auxiliary,
	inconsistent, metadata, older_than, preparation,
};

impl BoundQueueGroup<'_, '_, '_> {
	pub(in crate::queue) fn prepare_move_to_dead_letter(
		&self,
		snapshot: &QueueGroupSnapshot<'_>,
		id: JobId,
		target: DeadLetterTarget,
	) -> Result<QueueGroupPreparation, QueueGroupError> {
		preparation((|| {
			let Some((state, origin)) = self.read_job(snapshot, id)? else {
				return Ok(QueueGroupPreparation::Unchanged);
			};
			if state.job().status() != JobStatus::Failed {
				return Ok(QueueGroupPreparation::Unchanged);
			}
			self.move_dead_letter(
				snapshot,
				DeadLetterMove {
					before: state.clone(),
					origin,
					failed: state,
					target,
				},
			)
		})())
	}
	pub(super) fn move_dead_letter(
		&self,
		snapshot: &QueueGroupSnapshot<'_>,
		request: DeadLetterMove,
	) -> Result<QueueGroupPreparation, QueueGroupError> {
		let DeadLetterMove {
			before,
			origin,
			failed,
			target,
		} = request;
		if failed.job().status() != JobStatus::Failed || before.job().id != failed.job().id {
			return Err(inconsistent(
				"dead letter move requires the exact prepared failed job",
			));
		}
		if self
			.read_dead_letter(snapshot, target.dead_letter_id)?
			.is_some()
		{
			return Err(QueueGroupError::IdentityCollision);
		}
		let original = self.original_binding(failed.job().id)?;
		let map = original
			.decode_record(original.identity(), snapshot.held(original.identity())?)
			.map_err(|error| at_record(original.identity().clone(), error))?;
		let source = snapshot.probe(&snapshot.source.originals, original.identity())?;
		if let (Some(map), Some(source)) = (&map, source)
			&& map.dead_letter_id != source
		{
			return Err(inconsistent("native and source original owners disagree"));
		}
		if let Some(id) = map.as_ref().map(|map| map.dead_letter_id).or(source) {
			let Some((dead_letter, dead_letter_origin)) = self.read_dead_letter(snapshot, id)?
			else {
				return Err(inconsistent(
					"original mapping points to a missing dead letter",
				));
			};
			DeadLetterOriginalState {
				original_job_id: failed.job().id,
				dead_letter_id: id,
			}
			.check_dead_letter(&dead_letter)?;
			if (source.is_some() && dead_letter_origin != Origin::Source)
				|| (dead_letter_origin == Origin::Source && source != Some(id))
			{
				return Err(inconsistent(
					"source original probe and dead letter disagree",
				));
			}
			return Err(QueueGroupError::OriginalConflict);
		}
		let id = failed.job().id;
		let job = self.public_job(snapshot, &failed)?;
		let parts = failed.into_parts();
		let now = snapshot.observed_at.as_i64();
		let binding = self.dead_letter_binding(target.dead_letter_id)?;
		let dead_letter = NativeDeadLetterState::new(
			DeadLetterJob {
				id: target.dead_letter_id,
				original_job_id: id,
				task_name: job.task_name,
				payload_json: job.payload_json,
				last_error_msg: job.last_error_msg.unwrap_or_default(),
				retry_count: job.retry_count,
				max_retries: job.max_retries,
				timeout: job.timeout,
				dedupe_key: job.dedupe_key,
				reason: target.reason,
				dead_lettered_at_unix_micros: now,
				created_at_unix_micros: now,
				updated_at_unix_micros: now,
			},
			parts.payload_encoding,
		)
		.map_err(|error| at_record(binding.identity().clone(), error))?;
		let mut plan = PlanBuilder::new();
		self.reconcile_dedupe(snapshot, Some((&before, origin)), None, &mut plan)?;
		self.plan_job_delete(snapshot, &before, &mut plan)?;
		plan.remove_source(self.job_identity(id), origin, QueueSourceRole::Job);
		plan.action(
			binding.identity().clone(),
			RecordMutation::Write(encode_auxiliary(
				&binding,
				&dead_letter,
				metadata(snapshot.observed_at),
			)?),
		);
		plan.action(
			original.identity().clone(),
			RecordMutation::Write(encode_auxiliary(
				&original,
				&DeadLetterOriginalState {
					original_job_id: id,
					dead_letter_id: target.dead_letter_id,
				},
				metadata(snapshot.observed_at),
			)?),
		);
		plan.finish(
			snapshot,
			PreparedQueueGroupOutcome::DeadLetterMoved {
				dead_letter_id: target.dead_letter_id,
			},
		)
	}

	pub(in crate::queue) fn prepare_delete_dead_letter(
		&self,
		snapshot: &QueueGroupSnapshot<'_>,
		dead_letter_id: JobId,
	) -> Result<QueueGroupPreparation, QueueGroupError> {
		preparation((|| {
			let Some((dead_letter, origin)) = self.read_dead_letter(snapshot, dead_letter_id)?
			else {
				return Ok(QueueGroupPreparation::Unchanged);
			};
			let mut plan = PlanBuilder::new();
			self.remove_dead_letter(snapshot, &dead_letter, origin, &mut plan)?;
			plan.finish(snapshot, PreparedQueueGroupOutcome::DeadLetterDeleted)
		})())
	}
	/// Deletes a dead letter created more than `retention_micros` before the snapshot's database
	/// time. Any other dead letter is unchanged.
	pub(in crate::queue) fn prepare_delete_expired_dead_letter(
		&self,
		snapshot: &QueueGroupSnapshot<'_>,
		dead_letter_id: JobId,
		retention_micros: i64,
	) -> Result<QueueGroupPreparation, QueueGroupError> {
		preparation((|| {
			let Some((dead_letter, origin)) = self.read_dead_letter(snapshot, dead_letter_id)?
			else {
				return Ok(QueueGroupPreparation::Unchanged);
			};
			if !older_than(
				dead_letter.dead_letter().dead_lettered_at_unix_micros,
				snapshot.observed_at,
				retention_micros,
			) {
				return Ok(QueueGroupPreparation::Unchanged);
			}
			let mut plan = PlanBuilder::new();
			self.remove_dead_letter(snapshot, &dead_letter, origin, &mut plan)?;
			plan.finish(snapshot, PreparedQueueGroupOutcome::DeadLetterDeleted)
		})())
	}
	pub(in crate::queue) fn prepare_requeue_dead_letter(
		&self,
		snapshot: &QueueGroupSnapshot<'_>,
		dead_letter_id: JobId,
		target: DeadLetterRequeueTarget,
	) -> Result<QueueGroupPreparation, QueueGroupError> {
		preparation((|| {
			let Some((dead_letter, origin)) = self.read_dead_letter(snapshot, dead_letter_id)?
			else {
				return Ok(QueueGroupPreparation::Unchanged);
			};
			if self.read_job(snapshot, target.job_id)?.is_some() {
				return Err(QueueGroupError::IdentityCollision);
			}
			let (dead_letter, payload_encoding) = dead_letter.into_parts();
			let now = snapshot.observed_at.as_i64();
			let job = NativeJob::new(NativeJobParts {
				job: NativeJobMetadata {
					id: target.job_id,
					task_name: dead_letter.task_name.clone(),
					lifecycle: JobLifecycle::Pending,
					run_at_or_after_unix_micros: match target.schedule {
						QueueRequeueSchedule::ObservedNow => now,
						QueueRequeueSchedule::At(at) => at.as_unix_micros(),
					},
					last_error_msg: None,
					retry_count: 0,
					max_retries: dead_letter.max_retries,
					timeout: dead_letter.timeout,
					dedupe_key: dead_letter.dedupe_key.clone(),
					created_at_unix_micros: now,
					updated_at_unix_micros: now,
				},
				payload: NativeJobPayload::InlineJson(dead_letter.payload_json.clone()),
				payload_encoding: payload_encoding.clone(),
			})
			.map_err(|error| {
				at_record(
					self.job_identity(target.job_id),
					QueueRepresentationError::from(error),
				)
			})?;
			let dead_letter =
				NativeDeadLetterState::new(dead_letter, payload_encoding).map_err(|error| {
					at_record(
						RecordIdentity::new(
							self.namespaces.fields.dead_letters.clone(),
							RecordKey::new(dead_letter_id.as_bytes().to_vec()),
						),
						error,
					)
				})?;
			let mut plan = PlanBuilder::new();
			self.reconcile_dedupe(snapshot, None, Some(&job), &mut plan)?;
			self.remove_dead_letter(snapshot, &dead_letter, origin, &mut plan)?;
			self.plan_job_write(snapshot, &job, &mut plan)?;
			plan.finish(
				snapshot,
				PreparedQueueGroupOutcome::DeadLetterRequeued {
					job_id: target.job_id,
				},
			)
		})())
	}
	/// Checks the same exact original membership for removal and physical relocation.
	pub(super) fn require_original_membership(
		&self,
		snapshot: &QueueGroupSnapshot<'_>,
		dead_letter: &NativeDeadLetterState,
		origin: Origin,
	) -> Result<(AuxiliarySchemaBinding<'_, DeadLetterOriginalState>, bool), QueueGroupError> {
		let original = self.original_binding(dead_letter.dead_letter().original_job_id)?;
		let map = original
			.decode_record(original.identity(), snapshot.held(original.identity())?)
			.map_err(|error| at_record(original.identity().clone(), error))?;
		let source = snapshot.probe(&snapshot.source.originals, original.identity())?;
		if let Some(map) = &map {
			map.check_dead_letter(dead_letter)?;
		}
		if (origin == Origin::Native && (map.is_none() || source.is_some()))
			|| (origin == Origin::Source && source != Some(dead_letter.dead_letter().id))
		{
			return Err(inconsistent(
				"dead letter has no exact original mapping or source probe",
			));
		}
		let has_map = map.is_some();
		Ok((original, has_map))
	}
	fn remove_dead_letter(
		&self,
		snapshot: &QueueGroupSnapshot<'_>,
		dead_letter: &NativeDeadLetterState,
		origin: Origin,
		plan: &mut PlanBuilder,
	) -> Result<(), QueueGroupError> {
		let (original, has_map) =
			self.require_original_membership(snapshot, dead_letter, origin)?;
		if has_map {
			plan.action(original.identity().clone(), RecordMutation::Delete);
		}
		let identity = self
			.dead_letter_binding(dead_letter.dead_letter().id)?
			.identity()
			.clone();
		plan.action(identity.clone(), RecordMutation::Delete);
		plan.remove_source(identity, origin, QueueSourceRole::DeadLetter);
		Ok(())
	}
}
