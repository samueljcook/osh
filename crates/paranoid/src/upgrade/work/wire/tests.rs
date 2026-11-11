use super::*;

fn fixture() -> serde_json::Value {
	let scope = serde_json::json!({
		"version": 1,
		"policy": {
			"scope": { "storage": [1], "namespace": [2] },
			"owner": { "document": {
				"schema": {
					"identifier": "fixture.document",
					"writer": { "version": 1, "envelope": "native_json" },
					"required_readers": [{ "version": 1, "envelope": "native_json" }]
				},
				"maintenance_indexes": [], "ready_indexes": []
			} }
		}
	});
	serde_json::json!({
		"kind": "installation_upgrade_work", "version": 1,
		"target": {
			"kind": "complete_policy_target", "version": 1,
			"request_id": [1,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],
			"installation_id": [2,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],
			"base_generation": "1",
			"policies": [{ "component": "fixture.component", "semantic_contract": "v1", "schemas": [], "settings": { "parameterless": {} } }],
			"scopes": { "replace": { "scopes": { "version": 1, "scopes": [scope["policy"].clone()] } } },
			"source_goal": "unchanged"
		},
		"owned_generation": "2", "status": "building",
		"document_conversion_checkpoints": [{
			"invariant": scope.clone(),
			"procedure": { "kind": "document_selected_writer_conversion_v1" },
			"progress": { "kind": "scanned", "last_key": [4] }
		}],
		"document_verification_checkpoints": [{
			"invariant": scope,
			"procedure": { "kind": "selected_writer_projection_equality_v1" },
			"extent": { "kind": "through", "upper_key": [9] },
			"progress": { "kind": "scanning", "after_key": [4] }
		}],
		"payload_checkpoints": [], "source_checkpoints": []
	})
}

fn parse(value: serde_json::Value) -> Result<UpgradeWork, UpgradeWorkError> {
	decode_work(&serde_json::value::to_raw_value(&value).unwrap())
}

#[test]
fn retained_work_bytes_and_cancelled_checkpoint_history_round_trip() {
	let value = fixture();
	let wire: WorkWire<'_> = serde_json::from_value(value.clone()).unwrap();
	let bytes = serde_json::to_string(&wire).unwrap();
	let mut work = parse(value).unwrap();
	assert_eq!(work.encode_json().unwrap().get(), bytes);
	let before = work.checkpoint_context();
	let change = work.record_cancellation().unwrap();
	assert_eq!(change.expected, change.next);
	assert_eq!(work.status(), PgUpgradeStatus::Cancelled);
	assert_eq!(work.document_conversion_checkpoints().len(), 1);
	assert_eq!(work.document_verification_checkpoints().len(), 1);
	assert_ne!(work.checkpoint_context(), before);
	let bytes = work.encode_json().unwrap();
	assert!(work.matches_work(&decode_work(&bytes).unwrap()));
	assert!(matches!(
		work.record_cancellation(),
		Err(UpgradeWorkError::TerminalWork)
	));
	assert_eq!(work.encode_json().unwrap().get(), bytes.get());
	work.follow_generation_after_finish(std::num::NonZeroU64::new(3).unwrap())
		.unwrap();
	assert!(work.matches_work(&decode_work(&work.encode_json().unwrap()).unwrap()));
}

#[test]
fn work_parser_rejects_queued_progress_and_invalid_verification_ranges() {
	let mut queued = fixture();
	queued["status"] = "queued".into();
	queued["owned_generation"] = "1".into();
	assert!(parse(queued).is_err());
	let mut empty = fixture();
	empty["document_verification_checkpoints"][0]["extent"] =
		serde_json::json!({ "kind": "empty" });
	assert!(parse(empty).is_err());
	let mut outside = fixture();
	outside["document_verification_checkpoints"][0]["progress"]["after_key"] =
		serde_json::json!([9]);
	assert!(parse(outside).is_err());
	let mut completed = fixture();
	completed["status"] = "completed".into();
	assert!(parse(completed).is_err());
}

#[test]
fn work_parser_rejects_completed_work_with_progress() {
	let mut completed = fixture();
	completed["status"] = "completed".into();
	completed["document_verification_checkpoints"][0]["progress"] =
		serde_json::json!({ "kind": "verified", "last_key": [9] });
	assert!(matches!(
		parse(completed.clone()),
		Err(UpgradeWorkError::DocumentCheckpoint(
			super::super::document_checkpoint::DocumentCheckpointError::CompletedProgress
		))
	));
	let mut verification_only = completed.clone();
	verification_only["document_conversion_checkpoints"] = serde_json::json!([]);
	assert!(matches!(
		parse(verification_only),
		Err(UpgradeWorkError::DocumentVerification(
			super::super::document_verification::DocumentVerificationError::CompletedProgress
		))
	));
	let mut source = completed.clone();
	source["document_conversion_checkpoints"] = serde_json::json!([]);
	source["document_verification_checkpoints"] = serde_json::json!([]);
	source["target"]["source_goal"] = "data_retired".into();
	source["source_checkpoints"] = serde_json::json!([{
		"procedure": { "kind": "raw_bytes_relocation_v1" },
		"invariant": {
			"component_policies": [{ "component": "fixture.component", "semantic_contract": "v1", "schemas": [], "settings": { "parameterless": {} } }],
			"scope_policies": { "version": 1, "scopes": [] }
		},
		"progress": { "kind": "scanned", "last_key": null }
	}]);
	assert!(matches!(
		parse(source.clone()),
		Err(UpgradeWorkError::SourceCheckpoint(
			super::super::v10::source_checkpoint::SourceCheckpointError::CompletedProgress
		))
	));
	let mut consumed = source;
	consumed["source_checkpoints"] = serde_json::json!([]);
	let wire: WorkWire<'_> = serde_json::from_value(consumed.clone()).unwrap();
	let work = parse(consumed).unwrap();
	assert_eq!(work.status(), PgUpgradeStatus::Completed);
	assert_eq!(
		work.encode_json().unwrap().get(),
		serde_json::to_string(&wire).unwrap()
	);
}

#[test]
fn work_parser_rejects_top_level_policy_target_slot_kind() {
	let policy_target_slot = serde_json::json!({
		"kind": "policy_target",
		"version": 1,
		"request_id": [1,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],
		"installation_id": [2,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],
		"base_generation": "1",
		"policies": [{
			"component": "fixture.component",
			"semantic_contract": "v1",
			"schemas": [],
			"settings": { "parameterless": {} }
		}],
		"scopes": { "unchanged": {} }
	});
	assert!(matches!(
		parse(policy_target_slot),
		Err(UpgradeWorkError::Json { .. })
	));
}
