//! Expiring per-key token buckets with a persistent configuration per group.

use crate::db::WriteDatabase;
use crate::fleet::RateLimit;
use crate::kv::{AtomicMutation, PgRawFields, RecordKey, Ttl};
use crate::runtime::PgRuntime;
use crate::scope::ScopeIdentity;
use primitives_rs::{
	crypto::hash, encoding::binary_to_text::ByteEncoding, rate_limit::token_bucket,
};
use serde::{Deserialize, Serialize};
use std::{
	num::{NonZeroU32, NonZeroU64},
	sync::Arc,
	time::Duration,
};

/// A group of independently keyed buckets that all use the same persistent rate configuration.
pub struct PgRateLimiterFields {
	/// The runtime that owns the declared Raw scope.
	pub runtime: Arc<PgRuntime>,
	/// A Raw scope reserved for rate-limit records, with reader and writer roles.
	pub scope: ScopeIdentity,
	/// The stable group name, from one through 128 bytes. Different groups have independent budgets.
	pub group: String,
	/// The configuration generation. A higher generation atomically replaces the stored rate.
	/// Handles from an older generation reject further charges. Equal generations require equal rates.
	pub generation: NonZeroU64,
	/// The capacity and refill rate that every instance must use for this group.
	pub rate: RateLimit,
}

/// An atomic token charge for one key.
pub struct PgRateLimitInput<'a> {
	/// The identity to limit. The maximum length is 2,048 bytes; an empty identity is allowed.
	pub key: &'a [u8],
	/// The number of tokens to consume, which must not exceed the configured capacity.
	pub cost: NonZeroU32,
}

/// A rate-limit result, calculated using the database clock while the bucket is locked.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PgRateLimitResult {
	/// The bucket consumed the requested credit.
	Allowed,
	/// The bucket lacks credit. The wait assumes no other consumption or database-clock adjustment.
	RetryAfter(Duration),
}

/// Independent rate budgets with bounded idle retention.
///
/// One permanent record retains each group's configuration. Individual bucket records expire
/// once their credit would be full, so forgotten keys do not require permanent storage.
/// Callers must run bounded expired-record cleanup on the reserved Raw scope. Configuration
/// records do not expire. Opening a higher configuration generation replaces the shared rate.
/// Each charge checks that generation. Existing buckets retain available whole tokens up to
/// the new capacity; conversion discards less than one fractional token and starts refill at
/// the new rate. Already-issued allowances are not revoked by a configuration change.
///
/// Operations on a pool commit their charge before returning. Operations in a caller's
/// transaction commit with that transaction; callers must commit before acting on an allowance.
/// A failed or cancelled application operation does not refund committed credit. The limiter
/// stores no concurrency slots, circuit state, or task ownership.
#[derive(Clone)]
pub struct PgRateLimiter {
	runtime: Arc<PgRuntime>,
	scope: ScopeIdentity,
	group_prefix: String,
	config: token_bucket::Config,
	generation: NonZeroU64,
}

/// Invalid rate-limit input, incompatible stored data, or a failed storage operation.
#[derive(Debug, thiserror::Error)]
pub enum PgRateLimiterError {
	/// A transaction could not begin or settle.
	#[error("keyed rate-limit transaction failed")]
	Database(#[source] crate::db::DatabaseError),
	/// The installation cannot admit this operation.
	#[error("keyed rate-limit protocol admission failed")]
	Protocol(#[source] crate::installation::ProtocolError),
	/// The operation failed and its transaction also failed to roll back.
	#[error("keyed rate-limit operation and rollback failed")]
	RollbackFailed {
		/// The original operation failure.
		#[source]
		operation_error: Box<Self>,
		/// The subsequent rollback failure.
		rollback_error: crate::db::DatabaseError,
	},
	/// The group, rate, key, or cost is outside the supported range.
	#[error("invalid keyed rate-limit configuration or input")]
	InvalidInput,
	/// A group already has a different persistent configuration.
	#[error("keyed rate-limit group configuration does not match")]
	ConfigMismatch,
	/// Stored data is malformed or uses an unsupported representation.
	#[error("invalid or unsupported keyed rate-limit record")]
	InvalidRecord,
	/// A database operation failed. No allowance is returned after uncertain completion.
	#[error("keyed rate-limit storage operation failed")]
	Storage(#[source] Box<dyn std::error::Error + Send + Sync>),
}

crate::db::impl_transaction_failure!(PgRateLimiterError, Self::Storage(_));

#[derive(Serialize, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct GroupRecord {
	format: u8,
	generation: NonZeroU64,
	rate: RateRecord,
}

#[derive(Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct RateRecord {
	capacity: u32,
	refill_tokens: u32,
	period_nanos: u128,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BucketRecord {
	format: u8,
	generation: NonZeroU64,
	rate: RateRecord,
	credit_units: u128,
	last_time_micros: u64,
}

enum RateLimitDecision {
	Awaiting,
	Prepared(PgRateLimitResult),
}

enum PreparedBucket {
	Unchanged(token_bucket::TokenBucket),
	Converted(token_bucket::TokenBucket),
}

impl PreparedBucket {
	fn bucket_mut(&mut self) -> &mut token_bucket::TokenBucket {
		match self {
			Self::Unchanged(bucket) | Self::Converted(bucket) => bucket,
		}
	}
}

impl PgRateLimiter {
	/// Opens a group and atomically creates or checks its persistent configuration.
	/// With a pool, the configuration commits before this method returns a handle.
	/// With a caller's transaction, that transaction must commit before the handle is used elsewhere.
	pub async fn open(
		database: impl WriteDatabase,
		fields: PgRateLimiterFields,
	) -> Result<Self, PgRateLimiterError> {
		if fields.group.is_empty() || fields.group.len() > 128 {
			return Err(PgRateLimiterError::InvalidInput);
		}
		let config = token_bucket::Config {
			capacity: NonZeroU32::new(fields.rate.capacity)
				.ok_or(PgRateLimiterError::InvalidInput)?,
			refill_tokens: NonZeroU32::new(fields.rate.tokens_per_interval)
				.ok_or(PgRateLimiterError::InvalidInput)?,
			refill_period: fields.rate.interval,
		};
		let mut bucket = token_bucket::TokenBucket::new(config, Duration::ZERO)
			.map_err(|_| PgRateLimiterError::InvalidInput)?;
		bucket.try_take(token_bucket::TakeInput {
			tokens: config.capacity,
			now: Duration::ZERO,
		});
		// TTL arithmetic uses signed microseconds. Reject rates that cannot retain a drained bucket.
		Ttl::expires_after(bucket.time_until_full().max(Duration::from_secs(1)))
			.map_err(|_| PgRateLimiterError::InvalidInput)?;
		let group_prefix = format!(
			"paranoid-rate-limit:{}:",
			ByteEncoding::HexLower.encode(fields.group.as_bytes())
		);
		let key = RecordKey::new(format!("{group_prefix}config:").into_bytes());
		let expected = GroupRecord {
			format: 1,
			generation: fields.generation,
			rate: RateRecord::from_config(config),
		};
		let raw = fields
			.runtime
			.raw(PgRawFields {
				scope: fields.scope.clone(),
			})
			.map_err(storage_error)?;
		raw.mutate(database, &key, |current| {
			if let Some(bytes) = current.live_value() {
				let actual: GroupRecord = decode_record(bytes)?;
				if actual.format != 1 {
					return Err(PgRateLimiterError::InvalidRecord);
				}
				if actual.generation > expected.generation
					|| actual.generation == expected.generation && actual.rate != expected.rate
				{
					return Err(PgRateLimiterError::ConfigMismatch);
				}
				if actual == expected {
					return Ok(AtomicMutation::KeepExisting);
				}
			}
			Ok(AtomicMutation::SetBytes {
				value: encode_record(&expected)?,
				ttl: Ttl::NO_EXPIRATION,
			})
		})
		.await
		.map_err(mutation_error)?;
		Ok(Self {
			runtime: fields.runtime,
			scope: fields.scope,
			group_prefix,
			config,
			generation: fields.generation,
		})
	}

	/// Charges one key under a row lock. Rejected charges rewrite a bucket only when its
	/// configuration generation needs conversion. A stale handle rejects new charges.
	/// A backwards database-clock adjustment neither creates credit nor shortens its retention.
	pub async fn try_acquire(
		&self,
		database: impl WriteDatabase,
		input: PgRateLimitInput<'_>,
	) -> Result<PgRateLimitResult, PgRateLimiterError> {
		if input.key.len() > 2048 || input.cost > self.config.capacity {
			return Err(PgRateLimiterError::InvalidInput);
		}
		let digest = hash::hash_fields(hash::Fields {
			purpose: b"paranoid/keyed-rate-limit/v1",
			fields: &[input.key],
		});
		let key = RecordKey::new(
			format!(
				"{}bucket:{}:",
				self.group_prefix,
				ByteEncoding::HexLower.encode(digest.as_bytes())
			)
			.into_bytes(),
		);
		let raw = self
			.runtime
			.raw_scope(PgRawFields {
				scope: self.scope.clone(),
			})
			.map_err(storage_error)?;
		database
			.run_write(
				"db.rate_limit.acquire",
				PgRateLimiterError::Database,
				|_, operation_error, rollback_error| PgRateLimiterError::RollbackFailed {
					operation_error: Box::new(operation_error),
					rollback_error,
				},
				async |tx| {
					let config_key =
						RecordKey::new(format!("{}config:", self.group_prefix).into_bytes());
					let mut decision = RateLimitDecision::Awaiting;
					raw.mutate_after_read(
						tx,
						&config_key,
						&key,
						|bytes| {
							let bytes = bytes.ok_or(PgRateLimiterError::InvalidRecord)?;
							let group: GroupRecord = decode_record(bytes)?;
							if group.format != 1 {
								return Err(PgRateLimiterError::InvalidRecord);
							}
							if group.generation != self.generation
								|| group.rate != RateRecord::from_config(self.config)
							{
								return Err(PgRateLimiterError::ConfigMismatch);
							}
							Ok(group)
						},
						|current, group| {
							// Bias the signed PostgreSQL timestamp into an unsigned monotonic coordinate.
							let now = Duration::from_micros(
								(i128::from(current.observed_at().as_i64()) - i128::from(i64::MIN))
									as u64,
							);
							let mut prepared = if let Some(bytes) = current.live_value() {
								let record: BucketRecord = decode_record(bytes)?;
								if record.format != 1 {
									return Err(PgRateLimiterError::InvalidRecord);
								}
								if record.generation > self.generation {
									return Err(PgRateLimiterError::ConfigMismatch);
								}
								if record.generation == self.generation && record.rate != group.rate
								{
									return Err(PgRateLimiterError::InvalidRecord);
								}
								let mut bucket = token_bucket::TokenBucket::from_snapshot(
									record.rate.config()?,
									token_bucket::Snapshot {
										credit_units: record.credit_units,
										last_time: Duration::from_micros(record.last_time_micros),
									},
								)
								.map_err(|_| PgRateLimiterError::InvalidRecord)?;
								if record.generation < self.generation && record.rate != group.rate
								{
									bucket.refill(now);
									let snapshot = bucket.snapshot();
									let available = (snapshot.credit_units
										/ record.rate.period_nanos)
										.min(u128::from(self.config.capacity.get()));
									bucket = token_bucket::TokenBucket::from_snapshot(
										self.config,
										token_bucket::Snapshot {
											credit_units: available
												* self.config.refill_period.as_nanos(),
											last_time: snapshot.last_time,
										},
									)
									.map_err(|_| PgRateLimiterError::InvalidRecord)?;
								}
								if record.generation < self.generation {
									PreparedBucket::Converted(bucket)
								} else {
									PreparedBucket::Unchanged(bucket)
								}
							} else {
								PreparedBucket::Unchanged(
									token_bucket::TokenBucket::new(self.config, now)
										.map_err(|_| PgRateLimiterError::InvalidInput)?,
								)
							};
							let bucket = prepared.bucket_mut();
							let result = bucket.try_take(token_bucket::TakeInput {
								tokens: input.cost,
								now,
							});
							let snapshot = bucket.snapshot();
							let clock_delay = snapshot.last_time.saturating_sub(now);
							let outcome = match result {
								token_bucket::Admission::Allowed => PgRateLimitResult::Allowed,
								token_bucket::Admission::RetryAfter(delay) => {
									PgRateLimitResult::RetryAfter(
										clock_delay
											.checked_add(delay)
											.ok_or(PgRateLimiterError::InvalidRecord)?,
									)
								}
								token_bucket::Admission::ExceedsCapacity => {
									return Err(PgRateLimiterError::InvalidInput);
								}
							};
							let mutation = match prepared {
								PreparedBucket::Unchanged(_)
									if result != token_bucket::Admission::Allowed =>
								{
									AtomicMutation::KeepExisting
								}
								PreparedBucket::Unchanged(bucket)
								| PreparedBucket::Converted(bucket) => {
									let ttl = clock_delay
										.checked_add(bucket.time_until_full())
										.ok_or(PgRateLimiterError::InvalidRecord)?;
									let ttl = Ttl::expires_after(ttl.max(Duration::from_secs(1)))
										.map_err(|_| PgRateLimiterError::InvalidRecord)?;
									let record = BucketRecord {
										format: 1,
										generation: self.generation,
										rate: group.rate.clone(),
										credit_units: snapshot.credit_units,
										last_time_micros: snapshot
											.last_time
											.as_micros()
											.try_into()
											.map_err(|_| PgRateLimiterError::InvalidRecord)?,
									};
									AtomicMutation::SetBytes {
										value: encode_record(&record)?,
										ttl,
									}
								}
							};
							decision = RateLimitDecision::Prepared(outcome);
							Ok(mutation)
						},
					)
					.await
					.map_err(native_mutation_error)?;
					match decision {
						RateLimitDecision::Prepared(outcome) => Ok(outcome),
						RateLimitDecision::Awaiting => {
							unreachable!("successful atomic mutation evaluated its callback")
						}
					}
				},
			)
			.await
	}
}

impl RateRecord {
	fn from_config(config: token_bucket::Config) -> Self {
		Self {
			capacity: config.capacity.get(),
			refill_tokens: config.refill_tokens.get(),
			period_nanos: config.refill_period.as_nanos(),
		}
	}
	fn config(&self) -> Result<token_bucket::Config, PgRateLimiterError> {
		if self.period_nanos == 0 {
			return Err(PgRateLimiterError::InvalidRecord);
		}
		let refill_period = primitives_rs::time::duration_from_nanos_u128(self.period_nanos)
			.map_err(|_| PgRateLimiterError::InvalidRecord)?;
		let config = token_bucket::Config {
			capacity: NonZeroU32::new(self.capacity).ok_or(PgRateLimiterError::InvalidRecord)?,
			refill_tokens: NonZeroU32::new(self.refill_tokens)
				.ok_or(PgRateLimiterError::InvalidRecord)?,
			refill_period,
		};
		token_bucket::TokenBucket::new(config, Duration::ZERO)
			.map_err(|_| PgRateLimiterError::InvalidRecord)?;
		Ok(config)
	}
}

fn encode_record(value: &impl Serialize) -> Result<Vec<u8>, PgRateLimiterError> {
	serde_json::to_vec(value).map_err(|_| PgRateLimiterError::InvalidRecord)
}
fn decode_record<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, PgRateLimiterError> {
	if bytes.len() > 1024 {
		return Err(PgRateLimiterError::InvalidRecord);
	}
	serde_json::from_slice(bytes).map_err(|_| PgRateLimiterError::InvalidRecord)
}
fn storage_error(error: impl std::error::Error + Send + Sync + 'static) -> PgRateLimiterError {
	PgRateLimiterError::Storage(Box::new(error))
}

fn mutation_error(error: crate::kv::PgRawError<PgRateLimiterError>) -> PgRateLimiterError {
	if error.database_error().is_some() {
		return storage_error(error);
	}
	match error.callback_error() {
		Some(PgRateLimiterError::InvalidInput) => PgRateLimiterError::InvalidInput,
		Some(PgRateLimiterError::ConfigMismatch) => PgRateLimiterError::ConfigMismatch,
		Some(PgRateLimiterError::InvalidRecord) => PgRateLimiterError::InvalidRecord,
		_ => storage_error(error),
	}
}

fn native_mutation_error(
	error: crate::kv::raw::engine::store::NativeRawKvError<PgRateLimiterError>,
) -> PgRateLimiterError {
	match error {
		crate::kv::raw::engine::store::NativeRawKvError::Mutation(
			crate::kv::raw::state::RecordMutationError::Callback(error),
		) => error,
		error => storage_error(error),
	}
}
