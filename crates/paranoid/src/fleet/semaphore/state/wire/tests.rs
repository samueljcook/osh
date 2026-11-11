//! Stored Semaphore wire codec tests for settings bounds and held-slot shapes.

use super::{decode, encode};
use serde_json::{Value, json};

fn record_with_slots(max_hold_micros: &str, slots: Vec<Value>) -> Value {
	json!({
		"key": "wire.semaphore",
		"settings": { "capacity": 1, "max_hold_micros": max_hold_micros },
		"slots": slots
	})
}

fn record(max_hold_micros: &str) -> Value {
	record_with_slots(max_hold_micros, vec![json!({ "kind": "absent" }); 100])
}

#[test]
fn a_stored_hold_shorter_than_one_second_round_trips() {
	let input = record("500000");
	let parsed = decode(&serde_json::to_vec(&input).expect("encode the wire fixture"))
		.expect("parse a stored hold of half a second");
	let output: Value = serde_json::from_slice(&encode(&parsed).expect("encode the parsed state"))
		.expect("parse the encoded state");
	assert_eq!(output, input);
}

#[test]
fn stored_holds_enforce_the_settings_duration_bounds() {
	let longest = record("896996973605477580");
	let parsed = decode(&serde_json::to_vec(&longest).expect("encode the longest hold fixture"))
		.expect("parse the longest stored hold");
	let output: Value = serde_json::from_slice(&encode(&parsed).expect("encode the parsed state"))
		.expect("parse the encoded state");
	assert_eq!(output, longest);

	for micros in [
		"0",
		"-1",
		"896996973605477581",
		"9223372036854775807",
		"9223372036854775808",
	] {
		assert!(
			decode(&serde_json::to_vec(&record(micros)).expect("encode invalid settings")).is_err()
		);
	}
}

#[test]
fn stored_slots_reject_unowned_or_incomplete_variants() {
	let mut held_slots = vec![json!({ "kind": "absent" }); 100];
	held_slots[0] = json!({
		"kind": "native_private",
		"holder": "holder.1",
		"capability": "01".repeat(32),
		"expires_at": "2000",
		"updated_at": "1000"
	});
	let valid = record_with_slots("500000", held_slots);
	let parsed = decode(&serde_json::to_vec(&valid).expect("encode valid held slot"))
		.expect("parse a held private slot");
	let output: Value = serde_json::from_slice(&encode(&parsed).expect("encode held slot"))
		.expect("parse encoded held slot");
	assert_eq!(output, valid);

	for invalid_slot in [
		json!({ "kind": "vacant", "expires_at": "2000", "updated_at": "1000" }),
		json!({
			"kind": "retained_published",
			"source_owner": "legacy-owner",
			"expires_at": "2000",
			"updated_at": "1000"
		}),
		json!({
			"kind": "native_private",
			"capability": "01".repeat(32),
			"expires_at": "2000",
			"updated_at": "1000"
		}),
		json!({
			"kind": "native_private",
			"holder": "holder.1",
			"expires_at": "2000",
			"updated_at": "1000"
		}),
	] {
		let mut slots = vec![json!({ "kind": "absent" }); 100];
		slots[0] = invalid_slot;
		let candidate = record_with_slots("500000", slots);
		assert!(
			decode(&serde_json::to_vec(&candidate).expect("encode invalid slot fixture")).is_err()
		);
	}
}
