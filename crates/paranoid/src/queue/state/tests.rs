use super::*;

fn published_native_job() -> serde_json::Value {
	serde_json::json!({
		"ownership": { "kind": "native_private" },
		"job": {
			"id": [1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
			"task_name": "fixture.task",
			"payload_json": "{\"value\":1}",
			"payload_encoding": { "kind": "opaque_json" },
			"status": "running",
			"run_at_or_after_unix_micros": "1",
			"last_error_message": null,
			"retry_count": 0,
			"max_retries": 2,
			"timeout": { "kind": "registered_def" },
			"dedupe_key": null,
			"worker_owner_id": "fixture:one",
			"worker_claim_token": vec![0_u8; 32],
			"claimed_by_worker_at_unix_micros": "2",
			"execution_started_at_unix_micros": null,
			"execution_heartbeat_at_unix_micros": "2",
			"finished_at_unix_micros": null,
			"created_at_unix_micros": "1",
			"updated_at_unix_micros": "2"
		}
	})
}

#[test]
fn retained_native_job_wire_fields_round_trip_without_changing_claim_material() {
	let wire = published_native_job();
	let state = decode_job(&serde_json::to_vec(&wire).unwrap()).unwrap();
	let encoded = encode_job(&state).unwrap();
	assert_eq!(
		serde_json::from_slice::<serde_json::Value>(&encoded).unwrap(),
		wire
	);
	let mut retired = wire;
	retired["ownership"]["kind"] = "retired_published".into();
	retired["job"]["worker_claim_token"] = serde_json::Value::Null;
	retired["job"]["execution_started_at_unix_micros"] = "3".into();
	let state = decode_job(&serde_json::to_vec(&retired).unwrap()).unwrap();
	assert!(state.claim_token().is_none());
	assert_eq!(
		serde_json::from_slice::<serde_json::Value>(&encode_job(&state).unwrap()).unwrap(),
		retired
	);
}

#[test]
fn retained_job_parser_rejects_inconsistent_status_authority_and_timestamps() {
	for (field, replacement) in [
		("worker_owner_id", serde_json::Value::Null),
		("claimed_by_worker_at_unix_micros", serde_json::Value::Null),
		(
			"execution_heartbeat_at_unix_micros",
			serde_json::Value::Null,
		),
		("worker_claim_token", serde_json::Value::Null),
		("finished_at_unix_micros", "3".into()),
		("status", "pending".into()),
		("status", "completed".into()),
		("status", "failed".into()),
	] {
		let mut wire = published_native_job();
		wire["job"][field] = replacement;
		assert!(
			decode_job(&serde_json::to_vec(&wire).unwrap()).is_err(),
			"invalid field: {field}"
		);
	}
}

#[test]
fn retained_job_parser_rejects_out_of_range_retry_bounds_without_panicking() {
	for max_retries in [i32::MAX as u32 + 1, u32::MAX] {
		let mut wire = published_native_job();
		wire["job"]["max_retries"] = max_retries.into();
		assert!(
			decode_job(&serde_json::to_vec(&wire).unwrap()).is_err(),
			"max_retries {max_retries} must be rejected"
		);
	}

	for retry_count in [i32::MAX as u32 + 1, u32::MAX] {
		let mut running = published_native_job();
		running["job"]["retry_count"] = retry_count.into();
		assert!(
			decode_job(&serde_json::to_vec(&running).unwrap()).is_err(),
			"running retry_count {retry_count} must be rejected"
		);
	}

	let mut failed = published_native_job();
	failed["ownership"]["kind"] = "inactive".into();
	failed["job"]["status"] = "failed".into();
	failed["job"]["worker_owner_id"] = serde_json::Value::Null;
	failed["job"]["worker_claim_token"] = serde_json::Value::Null;
	failed["job"]["claimed_by_worker_at_unix_micros"] = serde_json::Value::Null;
	failed["job"]["execution_heartbeat_at_unix_micros"] = serde_json::Value::Null;
	failed["job"]["finished_at_unix_micros"] = "3".into();
	failed["job"]["max_retries"] = (i32::MAX as u32).into();
	failed["job"]["retry_count"] = (i32::MAX as u32 + 1).into();
	assert!(decode_job(&serde_json::to_vec(&failed).unwrap()).is_ok());

	failed["job"]["retry_count"] = u32::MAX.into();
	assert!(decode_job(&serde_json::to_vec(&failed).unwrap()).is_err());
}
