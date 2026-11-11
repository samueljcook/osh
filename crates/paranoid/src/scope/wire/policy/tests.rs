use super::super::*;
use super::*;

fn fixture() -> serde_json::Value {
	serde_json::json!({
		"version": 1,
		"policy": {
			"scope": { "storage": [1], "namespace": [2] },
			"owner": { "document": {
				"schema": {
					"identifier": "fixture.document",
					"writer": { "version": 1, "envelope": "native_json" },
					"required_readers": [{ "version": 1, "envelope": "native_json" }]
				},
				"maintenance_indexes": [
					{ "identifier": "alpha", "version": 1, "fields": [{ "name": "value", "scalar_type": "signed_integer" }] },
					{ "identifier": "beta", "version": 1, "fields": [{ "name": "value", "scalar_type": "signed_integer" }] },
					{ "identifier": "gamma", "version": 1, "fields": [{ "name": "value", "scalar_type": "signed_integer" }] }
				],
				"ready_indexes": [
					{ "identifier": "gamma", "version": 1 },
					{ "identifier": "alpha", "version": 1 }
				]
			} }
		}
	})
}

#[test]
fn document_policy_preserves_independent_stored_index_orders() {
	let wire: ScopePolicyWire<'_> = serde_json::from_value(fixture()).unwrap();
	let bytes = serde_json::to_vec(&wire).unwrap();
	let policy = wire.into_policy().unwrap();
	assert_eq!(
		serde_json::to_vec(&ScopePolicyWire::from_policy(&policy)).unwrap(),
		bytes
	);
	let ScopeOwner::Document(document) = policy.owner() else {
		panic!("fixture document owner")
	};
	let beta = IndexContract::new(
		IndexContractIdentity::new("beta".to_owned(), 1).unwrap(),
		vec![IndexFieldDef::new("value".to_owned(), IndexScalarType::SignedInteger).unwrap()],
	)
	.unwrap();
	assert!(matches!(
		document.require_ready_index(&beta),
		Err(ScopeError::QueryIndexNotReady)
	));
	let ready = document.with_ready_index(&beta).unwrap();
	assert!(ready.require_ready_index(&beta).is_ok());
	assert_eq!(
		ready
			.ready_indexes()
			.map(IndexContractIdentity::identifier)
			.collect::<Vec<_>>(),
		vec!["gamma", "alpha", "beta"]
	);
	assert_eq!(
		ready
			.maintenance_indexes()
			.map(|index| index.identity().identifier())
			.collect::<Vec<_>>(),
		vec!["alpha", "beta", "gamma"]
	);
	assert!(matches!(
		document.require_ready_index(&beta),
		Err(ScopeError::QueryIndexNotReady)
	));
	assert!(ready.matches_index_policy(&ready.with_ready_index(&beta).unwrap()));
}

#[test]
fn document_policy_rejects_duplicate_and_unmaintained_ready_indexes() {
	let mut duplicate = fixture();
	duplicate["policy"]["owner"]["document"]["ready_indexes"] = serde_json::json!([
		{ "identifier": "alpha", "version": 1 },
		{ "identifier": "alpha", "version": 1 }
	]);
	let wire: ScopePolicyWire<'_> = serde_json::from_value(duplicate).unwrap();
	assert!(matches!(
		wire.into_policy(),
		Err(ScopeWireError::Scope(ScopeError::DuplicateReadyIndex {
			index: 1
		}))
	));
	let mut absent = fixture();
	absent["policy"]["owner"]["document"]["ready_indexes"] = serde_json::json!([
		{ "identifier": "absent", "version": 1 }
	]);
	let wire: ScopePolicyWire<'_> = serde_json::from_value(absent).unwrap();
	assert!(matches!(
		wire.into_policy(),
		Err(ScopeWireError::Scope(ScopeError::ReadyIndexNotMaintained {
			index: 0
		}))
	));
}
