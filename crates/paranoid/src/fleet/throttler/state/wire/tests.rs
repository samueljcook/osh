//! Persisted circuit fields and stored settings bounds must survive parsing into the authoritative
//! Throttler model.

use super::{decode, encode};
use serde_json::{Value, json};

const CAPABILITY: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

fn record(circuit: Value) -> Value {
	json!({
		"key": "wire.circuit",
		"settings": {
			"rate": null,
			"concurrency": { "capacity": 1, "max_hold_nanos": "1000000000" },
			"circuit": {
				"failure_threshold": 2,
				"recovery_timeout_nanos": "1000000",
				"probe_window_nanos": "1000000000"
			}
		},
		"activity": {
			"kind": "present",
			"expires_at": "1234",
			"updated_at": "1000",
			"tokens_bits": "0000000000000000",
			"last_refill": "1000",
			"slots": [{
				"number": 1,
				"expires_at": "1200",
				"ownership": { "kind": "native_private", "holder": "wire.holder", "capability": CAPABILITY }
			}],
			"failures": 1,
			"circuit": circuit
		}
	})
}

#[test]
fn persisted_circuit_forms_round_trip_and_reject_closed_opening_or_legacy_ownership() {
	let ownerships = [
		json!({ "kind": "uninitialized" }),
		json!({ "kind": "native", "epoch": CAPABILITY, "probe": null }),
	];
	for ownership in &ownerships {
		for (state, opened_at) in [
			("Closed", Value::Null),
			("Open", Value::Null),
			("Open", json!("1000")),
		] {
			let input = record(json!({
				"state": state,
				"opened_at": opened_at,
				"ownership": ownership
			}));
			let parsed = decode(&serde_json::to_vec(&input).expect("encode a wire fixture"))
				.expect("parse a supported persisted circuit form");
			let output: Value =
				serde_json::from_slice(&encode(&parsed).expect("encode the parsed state"))
					.expect("parse the encoded state");
			assert_eq!(output, input);
		}

		let closed_with_opening = record(json!({
			"state": "Closed",
			"opened_at": "1000",
			"ownership": ownership
		}));
		assert!(
			decode(&serde_json::to_vec(&closed_with_opening).expect("encode closed opening"))
				.is_err()
		);
	}

	let retained_circuit = record(json!({
		"state": "Open",
		"opened_at": "1000",
		"ownership": {
			"kind": "retained_published",
			"source_owner": "source.holder",
			"expires_at": "1200"
		}
	}));
	assert!(
		decode(&serde_json::to_vec(&retained_circuit).expect("encode legacy circuit")).is_err()
	);

	let mut retained_slot = record(json!({
		"state": "Closed",
		"opened_at": null,
		"ownership": { "kind": "uninitialized" }
	}));
	retained_slot["activity"]["slots"][0]["ownership"] = json!({
		"kind": "retained_published",
		"source_owner": "source.holder"
	});
	assert!(decode(&serde_json::to_vec(&retained_slot).expect("encode legacy slot")).is_err());
}

#[test]
fn stored_settings_enforce_the_settings_duration_bounds() {
	let circuit =
		json!({ "state": "Closed", "opened_at": null, "ownership": { "kind": "uninitialized" } });
	let mut input = record(circuit);
	input["settings"]["concurrency"]["max_hold_nanos"] = json!("896996973605477580000");
	let state = decode(&serde_json::to_vec(&input).expect("encode the longest hold fixture"))
		.expect("parse the longest stored hold");
	let output: Value = serde_json::from_slice(&encode(&state).expect("encode stored settings"))
		.expect("parse encoded settings");
	assert_eq!(output, input);

	input["settings"]["concurrency"]["max_hold_nanos"] = json!("896996973605477581000");
	assert!(
		decode(&serde_json::to_vec(&input).expect("encode the oversized hold fixture")).is_err()
	);
}

#[test]
fn a_persisted_private_probe_requires_an_open_circuit() {
	let mut input = record(json!({
		"state": "Open",
		"opened_at": null,
		"ownership": {
			"kind": "native",
			"epoch": CAPABILITY,
			"probe": { "holder": "wire.probe", "capability": CAPABILITY, "expires_at": "1200" }
		}
	}));
	let parsed = decode(&serde_json::to_vec(&input).expect("encode the open probe fixture"))
		.expect("parse the open probe");
	let output: Value = serde_json::from_slice(&encode(&parsed).expect("encode the open probe"))
		.expect("parse the encoded probe");
	assert_eq!(output, input);
	input["activity"]["circuit"]["state"] = json!("Closed");
	assert!(decode(&serde_json::to_vec(&input).expect("encode the closed probe fixture")).is_err());
}
