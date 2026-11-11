use super::{
	EnqueueOptions, EnqueueRequestError, JobId, JobTimeout, Serialize, StoredJobTimeout, TaskDef,
};
use primitives_rs::serialization::json::{
	self, JsonError, JsonFormat, JsonLimits, SerializeOptions,
};

const MAX_PAYLOAD_JSON_CONTAINER_DEPTH: usize = 100;

/// Builds a native enqueue request with the same policy, timeout, and payload validation as
/// the published enqueue.
pub(crate) fn native_enqueue_request<T: Serialize>(
	def: &TaskDef<T>,
	payload: &T,
	options: EnqueueOptions,
	payload_json_size_limit: usize,
) -> Result<super::group_state::NativeEnqueueRequest, EnqueueRequestError> {
	def.policy().require_storable()?;
	let payload_json = serialize_payload_json(payload, payload_json_size_limit)
		.map_err(PayloadJsonSerializationError::into_single_payload_error)?;
	let job_id = JobId::generate().map_err(EnqueueRequestError::JobIdGeneration)?;
	let policy = def.policy();
	Ok(super::group_state::NativeEnqueueRequest {
		job_id,
		task_name: def.name().clone(),
		payload_json,
		payload_encoding: super::NativePayloadEncoding::OpaqueJson {},
		run_at_or_after: options.run_at_or_after,
		max_retries: policy.max_retries,
		timeout: match policy.timeout {
			JobTimeout::NoTimeout => StoredJobTimeout::NoTimeout,
			JobTimeout::ExpiresAfter(duration) => StoredJobTimeout::ExpiresAfter(duration),
		},
		dedupe_key: options.dedupe_key,
	})
}

enum PayloadJsonSerializationError {
	Json(serde_json::Error),
	TooLarge { actual_minimum: usize, max: usize },
}

impl PayloadJsonSerializationError {
	fn into_single_payload_error(self) -> EnqueueRequestError {
		match self {
			Self::Json(source) => EnqueueRequestError::PayloadJson(source),
			Self::TooLarge {
				actual_minimum,
				max,
			} => EnqueueRequestError::PayloadJsonTooLarge {
				actual_minimum,
				max,
			},
		}
	}
}

fn serialize_payload_json<T: Serialize + ?Sized>(
	payload: &T,
	payload_json_size_limit: usize,
) -> Result<String, PayloadJsonSerializationError> {
	match json::serialize_to_string(
		payload,
		SerializeOptions {
			limits: JsonLimits {
				max_size: payload_json_size_limit,
				max_depth: MAX_PAYLOAD_JSON_CONTAINER_DEPTH,
			},
			format: JsonFormat::Compact,
		},
	) {
		Ok(text) => Ok(text),
		Err(JsonError::TooLarge {
			actual_minimum,
			max,
		}) => Err(PayloadJsonSerializationError::TooLarge {
			actual_minimum,
			max,
		}),
		Err(error) => Err(PayloadJsonSerializationError::Json(
			error.into_serde_json_error(),
		)),
	}
}

#[cfg(test)]
mod tests {
	use super::super::group_state::NativeEnqueueRequest;
	use super::super::*;
	use super::*;
	use std::time::Duration;

	const PAYLOAD_JSON_SIZE_LIMIT: usize = 1 << 20;

	fn job_policy() -> JobPolicy {
		JobPolicy {
			max_retries: 3,
			timeout: JobTimeout::ExpiresAfter(Duration::from_secs(30)),
		}
	}

	fn no_options() -> EnqueueOptions {
		EnqueueOptions {
			run_at_or_after: None,
			dedupe_key: None,
		}
	}

	fn request<T: Serialize>(
		policy: JobPolicy,
		payload: &T,
		options: EnqueueOptions,
	) -> Result<NativeEnqueueRequest, EnqueueRequestError> {
		let def = TaskDef::new(TaskName::new("task.enqueue").expect("a task name"), policy);
		native_enqueue_request(&def, payload, options, PAYLOAD_JSON_SIZE_LIMIT)
	}

	#[test]
	fn a_payload_that_cannot_be_serialized_or_exceeds_the_limit_is_rejected() {
		struct FailingSerialize;

		impl Serialize for FailingSerialize {
			fn serialize<S: serde::Serializer>(&self, _serializer: S) -> Result<S::Ok, S::Error> {
				Err(serde::ser::Error::custom("intentional payload failure"))
			}
		}

		let Err(error) = request(job_policy(), &FailingSerialize, no_options()) else {
			panic!("a payload that cannot be serialized must be rejected");
		};
		assert!(matches!(error, EnqueueRequestError::PayloadJson(_)));
		assert!(
			error
				.to_string()
				.contains("queue payload could not be encoded as JSON")
		);

		let def = TaskDef::new(
			TaskName::new("task.enqueue").expect("a task name"),
			job_policy(),
		);
		let Err(error) = native_enqueue_request(&def, &"too large", no_options(), 4) else {
			panic!("an oversized payload must be rejected");
		};
		assert!(matches!(
			error,
			EnqueueRequestError::PayloadJsonTooLarge {
				actual_minimum,
				max: 4,
			} if actual_minimum > 4
		));

		let within_depth = (1..100).fold(serde_json::json!([]), |inner, _| {
			serde_json::Value::Array(vec![inner])
		});
		assert!(request(job_policy(), &within_depth, no_options()).is_ok());
		let too_deep = serde_json::Value::Array(vec![within_depth]);
		assert!(matches!(
			request(job_policy(), &too_deep, no_options()),
			Err(EnqueueRequestError::PayloadJson(_))
		));
	}

	#[test]
	fn a_request_carries_the_definition_policy_and_the_options() {
		let plain = request(job_policy(), &123_u16, no_options()).expect("a plain request");
		assert_eq!(plain.task_name.as_str(), "task.enqueue");
		assert_eq!(plain.payload_json, "123");
		assert_eq!(plain.run_at_or_after, None);
		assert_eq!(plain.max_retries, 3);
		assert_eq!(
			plain.timeout,
			StoredJobTimeout::ExpiresAfter(Duration::from_secs(30))
		);
		assert_eq!(plain.dedupe_key, None);

		let run_at = JobRunAtOrAfter::from_unix_micros(42).expect("a run time");
		let scheduled = request(
			JobPolicy {
				max_retries: 0,
				timeout: JobTimeout::NoTimeout,
			},
			&123_u16,
			EnqueueOptions {
				run_at_or_after: Some(run_at),
				dedupe_key: Some(DedupeKey::new("dedupe-key").expect("a dedupe key")),
			},
		)
		.expect("a scheduled request");
		assert_eq!(scheduled.run_at_or_after, Some(run_at));
		assert_eq!(scheduled.max_retries, 0);
		assert_eq!(scheduled.timeout, StoredJobTimeout::NoTimeout);
		assert_eq!(
			scheduled.dedupe_key.as_ref().map(DedupeKey::as_str),
			Some("dedupe-key")
		);
	}

	#[test]
	fn a_policy_that_a_stored_job_cannot_hold_is_rejected() {
		for max_retries in [0, 1, i32::MAX as u32, i32::MAX as u32 + 1, u32::MAX] {
			let result = request(
				JobPolicy {
					max_retries,
					..job_policy()
				},
				&123_u16,
				no_options(),
			);
			if max_retries <= i32::MAX as u32 {
				assert_eq!(
					result.expect("a storable retry count").max_retries,
					max_retries
				);
			} else {
				assert!(
					matches!(
						result,
						Err(EnqueueRequestError::JobPolicy(
							JobPolicyError::MaxRetriesTooLarge
						))
					),
					"a retry count of {max_retries} does not fit a Postgres integer"
				);
			}
		}

		for timeout in [
			JobTimeout::NoTimeout,
			JobTimeout::ExpiresAfter(Duration::from_nanos(1)),
			JobTimeout::ExpiresAfter(Duration::from_nanos(i64::MAX as u64)),
			JobTimeout::ExpiresAfter(Duration::from_nanos(i64::MAX as u64 + 1)),
			JobTimeout::ExpiresAfter(Duration::ZERO),
		] {
			let result = request(
				JobPolicy {
					timeout,
					..job_policy()
				},
				&123_u16,
				no_options(),
			);
			match timeout {
				JobTimeout::NoTimeout => {
					assert_eq!(
						result.expect("no timeout").timeout,
						StoredJobTimeout::NoTimeout
					);
				}
				JobTimeout::ExpiresAfter(duration)
					if !duration.is_zero() && duration.as_nanos() <= i64::MAX as u128 =>
				{
					assert_eq!(
						result.expect("a storable timeout").timeout,
						StoredJobTimeout::ExpiresAfter(duration)
					);
				}
				JobTimeout::ExpiresAfter(_) => {
					assert!(matches!(
						result,
						Err(EnqueueRequestError::JobPolicy(
							JobPolicyError::InvalidTimeout
						))
					));
				}
			}
		}
	}

	#[test]
	fn payload_json_keeps_exact_64_bit_and_128_bit_integers() {
		#[derive(Serialize)]
		struct WidePayload {
			smallest_i64: i64,
			small_u64: u64,
			largest_u64: u64,
			largest_u128: u128,
			nested: NestedWidePayload,
		}

		#[derive(Serialize)]
		struct NestedWidePayload {
			integer: u128,
		}

		let request = request(
			job_policy(),
			&WidePayload {
				smallest_i64: i64::MIN,
				small_u64: 8453,
				largest_u64: u64::MAX,
				largest_u128: u128::MAX,
				nested: NestedWidePayload { integer: u128::MAX },
			},
			no_options(),
		)
		.expect("a wide-integer request");
		assert_eq!(
			request.payload_json,
			r#"{"smallest_i64":-9223372036854775808,"small_u64":8453,"largest_u64":18446744073709551615,"largest_u128":340282366920938463463374607431768211455,"nested":{"integer":340282366920938463463374607431768211455}}"#
		);
	}
}
