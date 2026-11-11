//! Fixed Cache publication on the consumed Lease purpose and one canonical placed pair.

use super::super::final_purpose::{require_final_purpose_deadline, wait_final_purpose_work};
use super::error::{
	NativeCachePublicationCause as Cause, NativeCachePublicationError as Error,
	NativeCachePublicationErrorWithObserver,
};
use super::{
	DriverShared, Instant, NativeCachePreparedPublication, NativeLeaseCachePublication,
	NativeLeaseCloseObserver,
};
use crate::db::{
	TransactionFailure, WriteTx, finish_pool_owned_write_transaction_and_preserve_rollback_error,
};
use crate::fleet::cache::engine::PgNativeCache;
use crate::fleet::cache::entry_state::CacheEntryIdentity;
use crate::fleet::lease::state::NativeLeaseClaimMatchFields;
use crate::fleet::payload::scope::cache::{
	CacheEntryPayloadPrepareFields, PreparedCacheEntryPayload,
};
use crate::kv::Ttl;
use crate::records::postgres::record_set::{
	PgRecordSetLock, PgRecordSetLockFields, PgRecordTarget, PgRecordTargetFields,
};
use crate::records::{RecordMetadata, RecordMutation};
use crate::runtime::admission::payload::PayloadAdmissionFields;
use crate::scope::payload::PayloadSchemaDeclaration;

pub(crate) struct NativeCacheEntryPublicationFields<
	'request,
	'scope,
	'code,
	'payload_scope,
	'payload_code,
	T,
	C,
> {
	pub(crate) cache: &'request PgNativeCache<'scope, 'code>,
	pub(crate) cache_entry_identity: &'request CacheEntryIdentity,
	pub(crate) declaration: &'request PayloadSchemaDeclaration<'payload_scope, 'payload_code, T, C>,
	pub(crate) value: &'request T,
	pub(crate) captured_epoch: i64,
	pub(crate) ttl: Ttl,
}
pub(crate) struct NativeCachePublicationResult {
	pub(crate) publication: NativeCachePreparedPublication,
	pub(crate) observer: NativeLeaseCloseObserver,
}
enum PublicationInput<'a, 'scope, 'code, T, C> {
	Typed {
		declaration: &'a PayloadSchemaDeclaration<'scope, 'code, T, C>,
		value: &'a T,
		captured_epoch: i64,
		ttl: Ttl,
	},
}
enum PreparedPublication<'writer, 'binding, 'code> {
	Typed {
		payload: PreparedCacheEntryPayload<'writer, 'binding, 'code>,
		ttl: Ttl,
	},
}
impl NativeLeaseCachePublication {
	pub(crate) async fn publish_cache_entry<T, C>(
		self,
		fields: NativeCacheEntryPublicationFields<'_, '_, '_, '_, '_, T, C>,
	) -> Result<NativeCachePublicationResult, NativeCachePublicationErrorWithObserver<C>> {
		self.execute(
			fields.cache,
			fields.cache_entry_identity,
			PublicationInput::Typed {
				declaration: fields.declaration,
				value: fields.value,
				captured_epoch: fields.captured_epoch,
				ttl: fields.ttl,
			},
		)
		.await
	}
	async fn execute<T, C>(
		mut self,
		cache: &PgNativeCache<'_, '_>,
		cache_entry_identity: &CacheEntryIdentity,
		input: PublicationInput<'_, '_, '_, T, C>,
	) -> Result<NativeCachePublicationResult, NativeCachePublicationErrorWithObserver<C>> {
		let observer = self.close_observer();
		let result = self.execute_inner(cache, cache_entry_identity, input).await;
		match result {
			Ok(publication) => Ok(NativeCachePublicationResult {
				publication,
				observer,
			}),
			Err(error) => Err(NativeCachePublicationErrorWithObserver { error, observer }),
		}
	}
	async fn execute_inner<T, C>(
		&mut self,
		cache: &PgNativeCache<'_, '_>,
		cache_entry_identity: &CacheEntryIdentity,
		input: PublicationInput<'_, '_, '_, T, C>,
	) -> Result<NativeCachePreparedPublication, Error<C>> {
		self.prepared
			.identity
			.require_cache(cache, cache_entry_identity)
			.map_err(|error| Error::new(Cause::Identity(error)))?;
		let registration = self
			.take_registration()
			.map_err(|error| Error::new(Cause::Authority(error)))?;
		let shared = &self.client.shared;
		let deadline = self.prepared.deadline;
		let bindings = self.prepared.identity.lease_bindings();
		let mut tx = wait_final_purpose_work(
			shared,
			deadline,
			bindings
				.control_pool
				.begin_registered_transaction(registration, "fleet.native_cache.publication"),
		)
		.await
		.map_err(|error| Error::new(Cause::Authority(error)))?
		.map_err(Error::from_database_error)?;
		let marker = tx.statement_interruption_marker();
		let result = wait_final_purpose_work(
			shared,
			deadline,
			self.prepare_and_apply(&mut tx, cache, cache_entry_identity, input),
		)
		.await;
		let result = match result {
			Ok(result) => result,
			Err(source) => {
				marker.mark_interrupted();
				Err(Error::new(Cause::Authority(source)))
			}
		};
		finish_pool_owned_write_transaction_and_preserve_rollback_error(
			"fleet.native_cache.publication",
			tx,
			result,
			Error::from_database_error,
			|_, operation, rollback| Error::from_rollback_failure(operation, rollback),
		)
		.await
	}
	async fn prepare_and_apply<T, C>(
		&self,
		tx: &mut WriteTx<'_>,
		cache: &PgNativeCache<'_, '_>,
		cache_entry_identity: &CacheEntryIdentity,
		input: PublicationInput<'_, '_, '_, T, C>,
	) -> Result<NativeCachePreparedPublication, Error<C>> {
		let prepared = &self.prepared;
		let contracts = prepared
			.registered
			.authenticate_cache_publication(tx, &prepared.identity)
			.await
			.map_err(|error| Error::new(Cause::Registration(error)))?;
		require_before_deadline(&self.client.shared, prepared.deadline)?;
		let entry = cache.entry();
		let writer = contracts
			.bind_cache_entry_writer(entry.declaration())
			.map_err(|error| Error::new(Cause::Admission(error)))?;
		let bindings = prepared.identity.lease_bindings();
		let lease_declaration = bindings.component.declaration();
		let lease_contract = contracts
			.bind(&lease_declaration)
			.map_err(|error| Error::new(Cause::Admission(error)))?;
		let lease_schema = lease_contract
			.transitions()
			.bind_schema(&lease_contract, prepared.identity.mutex_key().clone())
			.map_err(|error| Error::new(Cause::LeaseBinding(error)))?;
		let entry_identity = entry.record_identity(cache_entry_identity);
		let producer = match &input {
			PublicationInput::Typed { declaration, .. } => {
				entry
					.require_payload_declaration(*declaration)
					.map_err(|error| Error::new(Cause::PayloadDeclaration(error)))?;
				Some(
					contracts
						.bind_payload_producer(PayloadAdmissionFields {
							actual_scope: entry.scope(),
							declaration: *declaration,
						})
						.map_err(|error| Error::new(Cause::Admission(error)))?,
				)
			}
		};
		let action = match input {
			PublicationInput::Typed {
				declaration,
				value,
				captured_epoch,
				ttl,
			} => PreparedPublication::Typed {
				payload: writer
					.prepare_payload(CacheEntryPayloadPrepareFields {
						identity: &entry_identity,
						cache_entry_identity,
						captured_epoch,
						declaration,
						producer: producer
							.as_ref()
							.expect("typed publication binds its actual Producer"),
						value,
					})
					.map_err(|error| Error::new(Cause::Payload(error)))?,
				ttl,
			},
		};
		require_before_deadline(&self.client.shared, prepared.deadline)?;
		let lease_target = PgRecordTarget::new(PgRecordTargetFields {
			storage: &bindings.storage,
			record: prepared.identity.lease_identity().clone(),
		});
		let entry_target = PgRecordTarget::header(PgRecordTargetFields {
			storage: entry.storage(),
			record: entry_identity.clone(),
		});
		let lease_placed = lease_target.identity().clone();
		let entry_placed = entry_target.identity().clone();
		if lease_placed == entry_placed {
			return Err(Error::new(Cause::Identity(crate::fleet::cache::engine::publication_identity::NativeCachePublicationIdentityError::AliasedRecords)));
		}
		let group = PgRecordSetLock::acquire(PgRecordSetLockFields {
			tx,
			targets: vec![lease_target, entry_target],
			conflict_attempts: bindings.conflict_attempts,
		})
		.await
		.map_err(|error| Error::new(Cause::Records(error)))?;
		require_before_deadline(&self.client.shared, prepared.deadline)?;
		let mutation = (|| {
			let snapshot = group
				.snapshot(&lease_placed)
				.map_err(|error| Error::new(Cause::Mutation(error)))?;
			if !lease_schema
				.matches_native_claim(NativeLeaseClaimMatchFields {
					snapshot,
					authority: &prepared.claim,
				})
				.map_err(|error| Error::new(Cause::LeaseRecord(error)))?
			{
				return Ok((
					RecordMutation::Keep,
					NativeCachePreparedPublication::Refused,
				));
			}
			let metadata = |ttl: Ttl| -> Result<RecordMetadata, Error<C>> {
				Ok(RecordMetadata {
					updated_at: group.observed_at(),
					expires_at: ttl
						.expiration_at(group.observed_at())
						.map_err(|_| Error::new(Cause::ExpirationOverflow))?,
				})
			};
			match action {
				PreparedPublication::Typed { payload, ttl } => {
					let metadata = metadata(ttl)?;
					Ok((
						RecordMutation::Write(
							payload
								.encode(metadata)
								.map_err(|error| Error::new(Cause::Envelope(error)))?,
						),
						NativeCachePreparedPublication::ValueWrite(metadata),
					))
				}
			}
		})();
		let (mutation, intended) = match mutation {
			Ok(result) => result,
			Err(operation) => {
				return match group.keep().await {
					Ok(()) => Err(operation),
					Err(cleanup) => Err(Error::new(Cause::PreparationCleanup {
						operation_error: Box::new(operation),
						cleanup_error: cleanup,
					})),
				};
			}
		};
		require_before_deadline(&self.client.shared, prepared.deadline)?;
		prepared.progress.prepare(intended);
		if intended == NativeCachePreparedPublication::Refused {
			group
				.keep()
				.await
				.map_err(|error| Error::new(Cause::Records(error)))?;
		} else {
			group
				.apply(vec![
					(lease_placed, RecordMutation::Keep),
					(entry_placed, mutation),
				])
				.await
				.map_err(|error| Error::new(Cause::Records(error)))?
				.map_err(|error| Error::new(Cause::Mutation(error)))?;
		}
		require_before_deadline(&self.client.shared, prepared.deadline)?;
		Ok(intended)
	}
}
fn require_before_deadline<C>(
	shared: &DriverShared,
	deadline: Instant,
) -> Result<Instant, Error<C>> {
	require_final_purpose_deadline(shared, deadline)
		.map_err(|error| Error::new(Cause::Authority(error)))
}
