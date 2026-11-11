use super::*;
use crate::installation::contract::{RepresentationPolicy, SchemaPolicy, StateStorageEnvelope};
use crate::records::RecordNamespace;
use crate::scope::{DocumentScopePolicy, ScopeIdentity, ScopeIdentityFields, ScopeOwner};

#[test]
fn verification_preserves_its_range_and_prior_state_on_rejected_updates() {
	let representation = RepresentationPolicy {
		version: 1,
		envelope: StateStorageEnvelope::NativeJson,
	};
	let policy = ScopePolicy::new(
		ScopeIdentity::new(ScopeIdentityFields {
			namespace: RecordNamespace::new(vec![1]).unwrap(),
		}),
		ScopeOwner::Document(
			DocumentScopePolicy::new(
				SchemaPolicy::new(
					"fixture.document".to_owned(),
					representation,
					vec![representation],
				)
				.unwrap(),
				Vec::new(),
			)
			.unwrap(),
		),
	);
	let mut checkpoint = DocumentVerificationCheckpoint::from_fields(
		policy,
		DocumentVerificationProcedure::SelectedWriterProjectionEqualityV1,
		DocumentVerificationExtent::Through {
			upper_key: RecordKey::new(vec![9]),
		},
		DocumentVerificationProgress::Scanning {
			after_key: Some(RecordKey::new(vec![4])),
		},
	)
	.unwrap();
	assert!(matches!(
		checkpoint.advance(DocumentVerificationUpdate::Advance {
			after_key: RecordKey::new(vec![9])
		}),
		Err(DocumentVerificationError::CursorOutsideExtent)
	));
	assert_eq!(
		checkpoint.progress(),
		DocumentVerificationProgress::Scanning {
			after_key: Some(&RecordKey::new(vec![4]))
		}
	);
	checkpoint
		.advance(DocumentVerificationUpdate::Advance {
			after_key: RecordKey::new(vec![5]),
		})
		.unwrap();
	assert!(matches!(
		checkpoint.advance(DocumentVerificationUpdate::Verified {
			last_key: Some(RecordKey::new(vec![4]))
		}),
		Err(DocumentVerificationError::CursorRewind)
	));
	checkpoint
		.advance(DocumentVerificationUpdate::Verified {
			last_key: Some(RecordKey::new(vec![8])),
		})
		.unwrap();
	assert_eq!(
		checkpoint.extent(),
		DocumentVerificationExtent::Through {
			upper_key: &RecordKey::new(vec![9])
		}
	);
	assert_eq!(
		checkpoint.progress(),
		DocumentVerificationProgress::Verified {
			last_key: Some(&RecordKey::new(vec![8]))
		}
	);
	assert!(matches!(
		checkpoint.advance(DocumentVerificationUpdate::Verified { last_key: None }),
		Err(DocumentVerificationError::AlreadyVerified)
	));
}
